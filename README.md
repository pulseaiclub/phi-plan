# phi-plan

A **plan mode** extension for [Phi](https://github.com/pulseaiclub/phi): a
read-only session mode (`/plan`) plus the model-driven `update_plan` tool.
Written in Rust against the [`phi-ext`](../phi/ext/rust) SDK; speaks the PXB
wire protocol over stdin/stdout. Its only extra dependency is `serde_json`,
used to parse the `update_plan` tool-argument payload.

## What you get

- **`/plan on|off|status`** — type an argument; bare `/plan` (or picker accept)
  leaves `/plan ` in the composer so you can finish `on` / `off` / `status`
  - `on` — switches the current session into **read-only plan mode**. The model
    is told every turn to plan, not implement; any tool call outside the
    plan-mode allowlist is **denied before the host's permission gate** with
    a concise denial reason; the footer status bar shows
    `plan mode: read-only`. Toggling mid-turn is refused.
  - `off` — exits plan mode (the gate lifts, the plan stays).
  - `status` — prints current mode + latest plan (toast).
- **`update_plan` tool** (always available): the model maintains an
  ordered step list with statuses `pending` / `in_progress` / `completed` /
  `failed`. Replace-all semantics; free-form statuses are coerced to the
  nearest canonical value and at most one step stays `in_progress` (earlier
  ones become `completed`). Output uses the canonical format
  (`Current Plan:\n1. [status] step …`).

### Plan-mode allowlist

While plan mode is on, only these tools may run (edit `READ_ONLY_TOOLS` in
`src/plan.rs` to suit your setup):

`read` · `grep` · `find` · `ls` · `mcp_list` · `mcp_inspect` · `agent_list` · `update_plan`

Everything else — `write`, `edit`, `bash`, `agent_spawn`/`agent_wait`/
`agent_cancel`, `mcp_call`, and any tool added later — is blocked with a
message explaining how to allow it. In plan mode, only
read-only, permission-free tools are visible/advertised; since an extension
cannot hide tools from the model, the same policy is enforced as a
dispatch-time denial.

## Layout

| Path | Role |
|------|------|
| `src/main.rs` | Extension wiring: `/plan` command, tool gate, prompt steering, lifecycle bookkeeping |
| `src/plan.rs` | Domain logic: allowlist, status coercion, single-in-progress rule, rendering (tool-arg JSON via `serde_json`) |
| `phi.yaml` | Manifest installed next to the binary |
| `Cargo.toml` | Crate; depends on `phi-ext` (SDK) and `serde_json` |

## Build & test

```bash
cargo test      # unit tests (normalization, parsing, rendering, allowlist)
cargo build --release
```

## Install

The extension is a self-contained binary plus the `phi.yaml` manifest, dropped
into an extension directory: global `~/.phi/extensions/phi-plan` applies
everywhere, project-local `.phi/extensions/phi-plan` pins it to one project.

### From a release (recommended)

Download the archive for your platform from
[GitHub Releases](https://github.com/pulseaiclub/phi-plan/releases). Naming is
`phi-plan_<version>_<os>_<arch>.tar.gz` (Linux/macOS) or `.zip` (Windows),
where `<version>` is the tag without the `v` (a `v0.1.0` tag yields
`phi-plan_0.1.0_linux_amd64.tar.gz`, etc.). Each archive already contains
`phi-plan` (`phi-plan.exe` on Windows) and `phi.yaml`.

Global install, Linux/macOS:

```bash
mkdir -p ~/.phi/extensions/phi-plan
tar xzf phi-plan_0.1.0_linux_amd64.tar.gz -C ~/.phi/extensions/phi-plan
```

Windows (PowerShell) or a project-local install: same idea — extract
`phi-plan.exe` + `phi.yaml` (or `phi-plan` + `phi.yaml`) into
`~/.phi/extensions/phi-plan` / `.phi/extensions/phi-plan`.

### From source

Requires a stable Rust toolchain; the first build fetches `phi-ext` and
`serde_json` from crates.io, so it needs network access.

```bash
cargo build --release
mkdir -p ~/.phi/extensions/phi-plan
cp target/release/phi-plan phi.yaml ~/.phi/extensions/phi-plan/
```

Then reload extensions in the TUI (**Ctrl+K → extensions → reload**) and try
`/plan on`, ask the agent to plan something, review, `/plan off`. Disable all
extensions with `PHI_EXTENSIONS=off`.

## Session semantics

Plan mode applies to the session it was enabled in; starting or resuming
another session resets it to off with a clean plan. State lives in the
extension process (per TUI controller). Note: plan mode cannot hide
mutating tools from the model's tool list or block mutating *local*
commands (`/rewind`,
`/export`, …) — those are host-side capabilities an extension doesn't expose.

See `../phi/doc/extensions.md` and `../phi/ext/rust/examples/{hello,full}.rs`
for the extension API surface.
