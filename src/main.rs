//! phi-plan — a plan-mode extension for Phi: read-only session mode + `update_plan`.
//!
//! What you get after installing and reloading extensions (Ctrl+K):
//! - `/plan [on|off|status]` — enter/exit read-only planning mode, or show
//!   the current mode + plan. Status shows in a toast; while on, the footer
//!   status bar reads "plan mode: read-only".
//! - While plan mode is ON every turn is steered read-only (prompt append)
//!   and any tool call outside [`plan::READ_ONLY_TOOLS`] is denied *before*
//!   the host's permission gate, with a concise denial reason. Toggling
//!   is refused while an agent turn is running.
//! - The model gets an `update_plan` tool (always available):
//!   replace-all plan steps with statuses pending/in_progress/completed/failed,
//!   coerced tolerantly, at most one step in_progress. The plan is rendered
//!   back to the model and printed by `/plan status`.
//!
//! Session semantics: plan mode applies to the session it was enabled in and
//! resets to off when you start or resume another session.

mod plan;

use phi_ext::{phi, pxb};
use std::cell::RefCell;
use std::rc::Rc;

const VERSION: &str = "0.2.0";

/// Extension state, shared between command / tool / intercept handlers.
#[derive(Default)]
struct Shared {
    /// plan mode (read-only gate) is ON for the current session.
    mode_on: bool,
    /// an agent turn is running (mode may only change between turns).
    turn_active: bool,
    /// most recently observed session id.
    session_id: String,
    /// current plan, kept by the `update_plan` tool.
    plan: plan::Plan,
}

fn main() -> Result<(), phi::Error> {
    let state = Rc::new(RefCell::new(Shared::default()));
    let mut ext = phi::Extension::new("phi-plan", VERSION);

    // ── /plan [on|off|status] ─────────────────────────────────────────────
    {
        let state = state.clone();
        ext.register_command(
            "plan",
            phi::Command::new(
                "Enter/exit read-only plan mode: /plan on|off|status",
                move |args, ctx| {
                    let arg = args.trim();
                    {
                        let mut st = state.borrow_mut();
                        st.session_id = ctx.session_id.clone();
                        match arg {
                            "" | "status" => {
                                let mode = if st.mode_on { "ON" } else { "off" };
                                let text = format!(
                                    "plan mode: {mode} — {}\n{}",
                                    st.plan.summary(),
                                    st.plan.render()
                                );
                                ctx.notify("info", &text);
                            }
                            "on" => {
                                if st.mode_on {
                                    ctx.notify("info", "plan mode is already on (read-only).");
                                } else if st.turn_active {
                                    return Err(
                                        "cannot change plan mode while a turn is active".into()
                                    );
                                } else {
                                    st.mode_on = true;
                                    ctx.set_status(plan::STATUS_ON);
                                    ctx.notify("info", plan::ON_BLURB);
                                }
                            }
                            "off" => {
                                if !st.mode_on {
                                    ctx.notify("info", "plan mode is already off.");
                                } else if st.turn_active {
                                    return Err(
                                        "cannot change plan mode while a turn is active".into()
                                    );
                                } else {
                                    st.mode_on = false;
                                    ctx.set_status("");
                                    ctx.notify("info", plan::OFF_BLURB);
                                }
                            }
                            _ => return Err("usage: /plan [on|off|status] (bare = status)".into()),
                        }
                    }
                    Ok(())
                },
            ),
        );
    }

    // ── update_plan tool (model-driven plan artifact) ─────────────────────────
    {
        let state = state.clone();
        let schema = phi::Schema::object()
            .property(
                "plan",
                phi::Schema::array(
                    phi::Schema::object()
                        .property(
                            "content",
                            phi::Schema::string().description("The plan step description."),
                        )
                        .property(
                            "status",
                            phi::Schema::string()
                                .description("Status of this step.")
                                .enum_values(["pending", "in_progress", "completed", "failed"]),
                        )
                        .property(
                            "notes",
                            phi::Schema::string().description("Optional notes for this step."),
                        )
                        .required(["content"]),
                )
                .description("Ordered list of plan items, replacing any previous plan."),
            )
            .required(["plan"]);

        let execute = move |args: &[u8]| {
            let items = plan::parse_items(args)
                .map_err(|e| format!("Error: invalid arguments for update_plan: {e}"))?;
            let mut st = state.borrow_mut();
            st.plan.items = items;
            let content = st.plan.render();
            Ok(phi::ToolResult {
                content,
                ..Default::default()
            })
        };
        ext.register_tool(
            phi::Tool::new("update_plan", plan::TOOL_DESCRIPTION, schema, execute)
                .detail_from_args(plan::detail_from_args),
        );
    }

    // ── Tool gate: deny everything outside READ_ONLY_TOOLS in plan mode ──
    {
        let state = state.clone();
        ext.on_tool_call(move |ev| {
            let st = state.borrow();
            if !st.mode_on {
                return None;
            }
            if plan::READ_ONLY_TOOLS.contains(&ev.tool_name.as_str()) {
                return None;
            }
            Some(phi::ToolCallResult {
                block: true,
                reason: plan::deny_reason(&ev.tool_name),
                ..Default::default()
            })
        });
    }

    // ── Prompt steering: announce plan mode each turn ─────────────────────
    {
        let state = state.clone();
        ext.on_before_agent_start(move |_ev| {
            if state.borrow().mode_on {
                Some(phi::BeforeAgentStartResult {
                    system_prompt_append: plan::MODE_APPEND.to_string(),
                    ..Default::default()
                })
            } else {
                None
            }
        });
    }

    // ── Lifecycle bookkeeping (observe only) ──────────────────────────────
    {
        let state = state.clone();
        ext.subscribe(pxb::Event::AgentStart, move |_ev| {
            state.borrow_mut().turn_active = true;
        });
    }
    {
        let state = state.clone();
        ext.subscribe(pxb::Event::AgentEnd, move |_ev| {
            let mut st = state.borrow_mut();
            st.turn_active = false;
        });
    }
    {
        let state = state.clone();
        ext.subscribe(pxb::Event::SessionStart, move |ev| {
            let mut st = state.borrow_mut();
            if !ev.session_id.is_empty() {
                st.session_id = ev.session_id;
            }
            // A new/resumed session starts outside plan mode with a clean plan.
            match ev.reason.as_str() {
                "new" | "resume" | "switch" => {
                    st.mode_on = false;
                    st.turn_active = false;
                    st.plan.items.clear();
                }
                _ => {}
            }
        });
    }

    ext.run()
}
