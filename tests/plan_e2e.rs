//! End-to-end test: a fake PXB host drives the compiled `phi-plan` binary and
//! exercises the whole plan-mode surface — handshake, `/plan` toggling,
//! tool-call gate (block in plan mode, pass outside), `update_plan` tool,
//! turn-active guard, and session reset.
//!
//! Mirrors the host harness used in `phi/ext/rust/tests/sdk_test.rs`.

use std::io::{BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use phi_ext::pxb;

struct Host {
    child: Child,
    rd: BufReader<ChildStdout>,
    wr: BufWriter<ChildStdin>,
    next_id: u32,
}

impl Host {
    fn spawn() -> Self {
        let bin = env!("CARGO_BIN_EXE_phi-plan");
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn phi-plan binary");
        let rd = BufReader::new(child.stdout.take().unwrap());
        let wr = BufWriter::new(child.stdin.take().unwrap());
        Self {
            child,
            rd,
            wr,
            next_id: 0,
        }
    }

    fn read(&mut self) -> pxb::Frame {
        match pxb::read_frame(&mut self.rd) {
            Ok(f) => f,
            Err(e) => panic!("host read failed (extension crashed?): {e}"),
        }
    }

    fn write(&mut self, typ: u16, flags: u16, id: u32, body: &[u8]) {
        pxb::write_frame(&mut self.wr, typ, flags, id, body).unwrap();
        self.wr.flush().unwrap();
    }

    fn rpc(&mut self, typ: u16, body: Vec<u8>) -> pxb::Frame {
        self.next_id += 1;
        self.write(typ, pxb::FLAG_HAS_ID, self.next_id, &body);
        let f = self.read();
        assert_eq!(
            f.header.flags & pxb::FLAG_HAS_ID,
            pxb::FLAG_HAS_ID,
            "response must echo FLAG_HAS_ID"
        );
        assert_eq!(f.header.id, self.next_id, "response must echo rpc id");
        f
    }

    /// Handshake: read Hello, reply HelloAck, drain registrations to Ready.
    fn handshake(&mut self) -> (pxb::Hello, Vec<pxb::Frame>) {
        let f = self.read();
        assert_eq!(f.header.typ, pxb::TYPE_HELLO);
        let hello = pxb::decode_hello(&f.body).unwrap();
        self.write(
            pxb::TYPE_HELLO_ACK,
            0,
            0,
            &pxb::encode_hello_ack(&pxb::HelloAck {
                protocol: pxb::PROTOCOL_VERSION,
                phi_version: "v0.0.0-e2e".into(),
                cwd: "/tmp".into(),
                session_id: "s1".into(),
                extension_dir: "/ext".into(),
            }),
        );
        let mut regs = Vec::new();
        loop {
            let f = self.read();
            match pxb::FrameType::from_u16(f.header.typ) {
                pxb::FrameType::RegisterCommand
                | pxb::FrameType::RegisterTool
                | pxb::FrameType::Subscribe => regs.push(f),
                pxb::FrameType::Ready => break,
                other => panic!("unexpected frame during registration: {other:?}"),
            }
        }
        (hello, regs)
    }

    /// Runs a slash command; returns (ok, error, notify messages).
    fn command(&mut self, args: &str) -> (bool, String, Vec<(String, String)>) {
        let body = pxb::encode_command_invoked(&pxb::CommandInvoked {
            name: "plan".into(),
            args: args.into(),
        });
        self.next_id += 1;
        self.write(
            pxb::TYPE_COMMAND_INVOKED,
            pxb::FLAG_HAS_ID,
            self.next_id,
            &body,
        );
        let mut notifies = Vec::new();
        loop {
            let f = self.read();
            match pxb::FrameType::from_u16(f.header.typ) {
                pxb::FrameType::Notify => {
                    let n = pxb::decode_notify(&f.body).unwrap();
                    notifies.push((n.level, n.message));
                }
                pxb::FrameType::CommandResponse => {
                    assert_eq!(f.header.id, self.next_id);
                    let r = pxb::decode_command_response(&f.body).unwrap();
                    return (r.ok, r.error, notifies);
                }
                other => panic!("unexpected frame during command: {other:?}"),
            }
        }
    }

    /// Intercepts a tool call; returns the decoded response.
    fn intercept_tool(&mut self, tool: &str) -> pxb::InterceptResp {
        let body = pxb::encode_intercept_req(&pxb::InterceptReq {
            event: pxb::Event::ToolCall.code(),
            tool_name: tool.into(),
            tool_call_id: "t1".into(),
            input: br#"{}"#.to_vec(),
            ..Default::default()
        });
        let f = self.rpc(pxb::TYPE_INTERCEPT, body);
        assert_eq!(f.header.typ, pxb::TYPE_INTERCEPT_RESPONSE);
        pxb::decode_intercept_resp(&f.body).unwrap()
    }

    fn send_event(&mut self, ev: pxb::Event, reason: &str, session_id: &str) {
        let body = pxb::encode_event_notify(&pxb::EventNotify {
            event: ev.code(),
            reason: reason.into(),
            session_id: session_id.into(),
            ..Default::default()
        });
        self.write(pxb::TYPE_EVENT, 0, 0, &body);
    }

    fn shutdown(&mut self) {
        self.write(pxb::TYPE_SHUTDOWN, 0, 0, &[]);
        let f = self.read();
        assert_eq!(f.header.typ, pxb::TYPE_SHUTDOWN_ACK);
        let status = self.child.wait().expect("wait for extension exit");
        assert!(status.success(), "extension exited with {status:?}");
    }
}

fn notify_text(notifies: &[(String, String)]) -> String {
    notifies
        .iter()
        .map(|(_, m)| m.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn registers_plan_surface() {
    let mut h = Host::spawn();
    let (hello, regs) = h.handshake();
    assert_eq!(hello.name, "phi-plan");
    assert_eq!(hello.version, "0.2.0");
    assert_eq!(
        hello.caps,
        pxb::CAP_COMMANDS | pxb::CAP_TOOLS | pxb::CAP_INTERCEPT | pxb::CAP_EVENTS
    );

    let mut commands = Vec::new();
    let mut tools = Vec::new();
    let mut events = Vec::new();
    for f in regs {
        match pxb::FrameType::from_u16(f.header.typ) {
            pxb::FrameType::RegisterCommand => {
                let c = pxb::decode_register_command(&f.body).unwrap();
                commands.push(c);
            }
            pxb::FrameType::RegisterTool => {
                let t = pxb::decode_register_tool(&f.body).unwrap();
                tools.push(t);
            }
            pxb::FrameType::Subscribe => {
                let s = pxb::decode_subscribe(&f.body).unwrap();
                events.extend(s.events);
                events.extend(s.intercept);
            }
            _ => unreachable!(),
        }
    }
    assert!(
        commands.iter().any(|c| c.name == "plan"),
        "expected /plan command, got {commands:?}"
    );
    assert!(
        tools.iter().any(|t| t.name == "update_plan"),
        "expected update_plan tool, got {tools:?}"
    );
    for want in [
        pxb::Event::AgentStart,
        pxb::Event::AgentEnd,
        pxb::Event::SessionStart,
    ] {
        assert!(
            events.contains(&want.code()),
            "expected subscription to {want:?}, got {events:?}"
        );
    }
    h.shutdown();
}

#[test]
fn plan_mode_toggle_gate_and_tool() {
    let mut h = Host::spawn();
    h.handshake();

    // /plan (status) while off → info notify, ok.
    let (ok, err, notes) = h.command("");
    assert!(ok, "status should succeed: {err}");
    let text = notify_text(&notes);
    assert!(text.contains("plan mode: off"), "got: {text}");

    // /plan bogus → usage error.
    let (ok, err, _) = h.command("sideways");
    assert!(!ok && err.contains("usage"), "got ok={ok} err={err}");

    // Outside plan mode every tool call passes the gate untouched.
    let resp = h.intercept_tool("write");
    assert!(
        !resp.block && resp.reason.is_empty(),
        "off-mode must not block"
    );

    // /plan on while an agent turn is running → refused.
    h.send_event(pxb::Event::AgentStart, "", "s1");
    let (ok, err, _) = h.command("on");
    assert!(
        !ok && err.contains("turn is active"),
        "got ok={ok} err={err}"
    );
    h.send_event(pxb::Event::AgentEnd, "", "s1");

    // /plan on → ok.
    let (ok, err, notes) = h.command("on");
    assert!(ok, "on should succeed: {err}");
    assert!(
        notify_text(&notes).contains("plan mode ON"),
        "got: {notes:?}"
    );

    // In plan mode: mutators blocked with reason, readers pass.
    let resp = h.intercept_tool("bash");
    assert!(resp.block, "bash must be blocked in plan mode");
    assert!(
        resp.reason.contains("plan mode") && resp.reason.contains("bash"),
        "reason: {}",
        resp.reason
    );
    let resp = h.intercept_tool("read");
    assert!(!resp.block, "read must stay available");
    let resp = h.intercept_tool("update_plan");
    assert!(!resp.block, "update_plan must stay available");

    // update_plan tool: replace-all + canonical rendering.
    let args = br#"{"plan":[{"content":"Investigate","status":"done"},{"content":"Fix it","status":"in_progress","notes":"two files"}]}"#;
    let body = pxb::encode_tool_invoke(&pxb::ToolInvoke {
        name: "update_plan".into(),
        args: args.to_vec(),
    });
    let f = h.rpc(pxb::TYPE_TOOL_INVOKE, body);
    assert_eq!(f.header.typ, pxb::TYPE_TOOL_RESULT);
    let res = pxb::decode_tool_result(&f.body).unwrap();
    assert!(!res.is_error, "tool errored: {}", res.error);
    assert!(
        res.content.contains("Current Plan:")
            && res.content.contains("1. [completed] Investigate")
            && res.content.contains("2. [in_progress] Fix it"),
        "content: {}",
        res.content
    );

    // /plan status now shows mode + plan.
    let (ok, err, notes) = h.command("status");
    assert!(ok, "status should succeed: {err}");
    let text = notify_text(&notes);
    assert!(text.contains("plan mode: ON"), "got: {text}");
    assert!(text.contains("2. [in_progress] Fix it"), "got: {text}");

    // A new session resets plan mode off and clears the plan.
    h.send_event(pxb::Event::SessionStart, "new", "s2");
    let resp = h.intercept_tool("write");
    assert!(!resp.block, "new session must leave plan mode");
    let (ok, err, notes) = h.command("status");
    assert!(ok, "status should succeed: {err}");
    let text = notify_text(&notes);
    assert!(text.contains("plan mode: off"), "got: {text}");
    assert!(text.contains("empty"), "plan should be cleared: {text}");

    h.shutdown();
}

#[test]
fn update_plan_validation_and_coercion() {
    let mut h = Host::spawn();
    h.handshake();

    let invoke = |h: &mut Host, args: &[u8]| -> pxb::ToolResultMsg {
        let body = pxb::encode_tool_invoke(&pxb::ToolInvoke {
            name: "update_plan".into(),
            args: args.to_vec(),
        });
        let f = h.rpc(pxb::TYPE_TOOL_INVOKE, body);
        assert_eq!(f.header.typ, pxb::TYPE_TOOL_RESULT);
        pxb::decode_tool_result(&f.body).unwrap()
    };

    // Bad args → tool error, not a crash.
    let res = invoke(&mut h, br#"{"plan":"oops"}"#);
    assert!(res.is_error && res.error.contains("update_plan"), "{res:?}");

    // Status coercion + single-in-progress enforcement (wip & in-progress both
    // coerce to in_progress; the later one wins, `a` drops to completed).
    let res = invoke(
        &mut h,
        br#"{"plan":[{"content":"a","status":"wip"},{"content":"b","status":"in-progress"},{"content":"c"}]}"#,
    );
    assert!(!res.is_error, "{}", res.error);
    assert!(
        res.content.contains("1. [completed] a")
            && res.content.contains("2. [in_progress] b")
            && res.content.contains("3. [pending] c"),
        "coerced statuses wrong: {}",
        res.content
    );

    h.shutdown();
}
