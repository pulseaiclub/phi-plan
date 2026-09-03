//! Plan-mode domain logic for the `update_plan` tool and `/plan` command.
//! Behavior:
//! - `update_plan` is replace-all over an ordered step list; each step has
//!   `content` (+ optional `status`, `notes`); free-form statuses are coerced
//!   to `pending` / `in_progress` / `completed` / `failed`, and at most one
//!   step stays `in_progress` (earlier ones are marked `completed`).
//! - While plan mode is ON, only the tools in [`READ_ONLY_TOOLS`] are allowed
//!   to run; every other tool call is denied before the host's permission
//!   gate.

use crate::json;

// ── Plan-mode policy ────────────────────────────────────────────────────────

/// Canonical plan statuses.
pub const PENDING: &str = "pending";
pub const IN_PROGRESS: &str = "in_progress";
pub const COMPLETED: &str = "completed";
pub const FAILED: &str = "failed";

/// The only tools allowed to run while plan mode is on: read-only,
/// permission-free tools. Enforced at the dispatch gate, since an extension
/// cannot hide tools from the model.
///
/// `update_plan` is read-only (in-memory state only), so it stays available.
/// Adjust this list freely: it is the plugin's safety contract.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "ls",
    "mcp_list",
    "mcp_inspect",
    "agent_list",
    "update_plan",
];

/// Tool allowed while in plan mode → exit message for everything else.
pub fn deny_reason(tool: &str) -> String {
    format!(
        "Tool \"{tool}\" is not available in plan mode (read-only). \
         End plan mode with `/plan off` to allow it, or add it to \
         READ_ONLY_TOOLS in the extension if it is truly read-only."
    )
}

/// Appended to the user message every turn while plan mode is active.
pub const MODE_APPEND: &str = "\
Plan mode is active for this session: you are in read-only planning mode and must not change \
files or run shell commands. Inspect the workspace with read/grep/find/ls/mcp_list/mcp_inspect/\
agent_list, and shape your plan with the update_plan tool (keep at most one step in_progress; \
statuses are pending/in_progress/completed/failed). Mutating tools (write/edit/bash/agent_spawn/\
agent_cancel/mcp_call and anything not on the plan-mode allowlist) are blocked and will error. \
When the plan is ready, stop and present it for review; the user will end plan mode with \
`/plan off` and then ask you to implement it.";

/// Text shown in the host footer status while plan mode is on.
pub const STATUS_ON: &str = "plan mode: read-only";

/// Short blurb for `/plan on`.
pub const ON_BLURB: &str = "\
plan mode ON: read-only exploration and planning. Use update_plan to shape the plan; \
mutating tools are blocked. End with /plan off.";

/// Short blurb for `/plan off`.
pub const OFF_BLURB: &str = "\
plan mode OFF: tools are no longer gated. The plan (if any) is still available \
via /plan status or update_plan.";

/// A single plan step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub content: String,
    pub status: String,
    pub notes: String,
}

/// The in-memory plan, replaced wholesale on each `update_plan` call.
#[derive(Debug, Default)]
pub struct Plan {
    pub items: Vec<PlanItem>,
}

impl Plan {
    /// Renders the plan in the canonical transcript format.
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return "Plan is currently empty.".to_string();
        }
        let mut lines = Vec::with_capacity(self.items.len());
        for (index, item) in self.items.iter().enumerate() {
            let mut line = format!("{}. [{}] {}", index + 1, item.status, item.content);
            if !item.notes.is_empty() {
                line.push_str("\n   Notes: ");
                line.push_str(&item.notes);
            }
            lines.push(line);
        }
        lines.insert(0, "Current Plan:".to_string());
        lines.join("\n")
    }

    /// One-line summary for toasts / status output.
    pub fn summary(&self) -> String {
        let n = self.items.len();
        if n == 0 {
            return "no plan yet".to_string();
        }
        let open = self
            .items
            .iter()
            .filter(|i| i.status == PENDING || i.status == IN_PROGRESS)
            .count();
        format!("{n} step{s} ({open} open)", s = if n == 1 { "" } else { "s" })
    }
}

// ── update_plan argument handling ─────────────────────────────────────────

/// Tool description shown to the model.
pub const TOOL_DESCRIPTION: &str = "\
Create or update the in-memory plan for implementation or investigation with at least three \
meaningful dependent steps. Do not use it for simple lookups, explanations, or code navigation. \
Pass the full ordered list of steps each call; it replaces the previous plan. Each item needs a \
`content` string; `status` defaults to \"pending\". Non-canonical status values are coerced to \
the nearest of pending/in_progress/completed/failed, and at most one item stays in_progress \
(earlier ones are marked completed).";

/// Parses and normalizes `update_plan` args: `{"plan":[{"content":…,"status":…,"notes":…},…]}`.
/// Unknown fields (e.g. `id`) are ignored.
pub fn parse_items(args: &[u8]) -> Result<Vec<PlanItem>, String> {
    let root = json::parse(args).map_err(|e| format!("invalid JSON: {e}"))?;
    let raw_items = root
        .get("plan")
        .and_then(|p| p.as_array())
        .ok_or_else(|| "plan must be an array (field \"plan\")".to_string())?;

    let mut items = Vec::with_capacity(raw_items.len());
    for (index, raw) in raw_items.iter().enumerate() {
        let obj = match raw {
            json::Value::Obj(_) => raw,
            _ => return Err(format!("plan item {} must be an object", index + 1)),
        };
        let content = match obj.get("content").and_then(|c| c.as_str()) {
            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
            _ => {
                return Err(format!(
                    "plan item {} is missing a non-empty `content` string",
                    index + 1
                ))
            }
        };
        let status = match obj.get("status").and_then(|s| s.as_str()) {
            Some(s) => normalize_status(s),
            None => PENDING.to_string(),
        };
        let notes = obj
            .get("notes")
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string();
        items.push(PlanItem {
            content,
            status,
            notes,
        });
    }
    Ok(enforce_single_in_progress(items))
}

/// Coerces a free-form status to one of the four canonical values.
/// Unknown/empty input maps to `pending` so a weak model's stray
/// status never fails the whole call.
pub fn normalize_status(status: &str) -> String {
    let s = status.trim().to_lowercase();
    match s.as_str() {
        "completed" | "complete" | "done" | "finished" | "resolved" | "✓" | "x" | "[x]" => {
            COMPLETED.to_string()
        }
        "in_progress" | "in-progress" | "inprogress" | "in progress" | "active" | "doing"
        | "started" | "current" | "wip" | "ongoing" | "running" => IN_PROGRESS.to_string(),
        "failed" | "fail" | "error" | "errored" | "blocked" | "cancelled" | "canceled"
        | "abandoned" | "skipped" => FAILED.to_string(),
        // pending, todo, not_started, queued, "", or anything unrecognized
        _ => PENDING.to_string(),
    }
}

/// Keeps at most one `in_progress` item: if several are marked, only the LAST
/// stays in_progress and the earlier ones are downgraded to completed — a
/// single active step drives the panel.
fn enforce_single_in_progress(mut plan: Vec<PlanItem>) -> Vec<PlanItem> {
    let mut last = None;
    let mut count = 0usize;
    for (i, item) in plan.iter().enumerate() {
        if item.status == IN_PROGRESS {
            count += 1;
            last = Some(i);
        }
    }
    if count <= 1 {
        return plan;
    }
    let last = last.expect("count>1 implies an in_progress index");
    for (i, item) in plan.iter_mut().enumerate() {
        if i != last && item.status == IN_PROGRESS {
            item.status = COMPLETED.to_string();
        }
    }
    plan
}

/// One-line TUI row for a pending `update_plan` call.
pub fn detail_from_args(args: &[u8]) -> String {
    match json::parse(args)
        .ok()
        .and_then(|v| v.get("plan").and_then(|p| p.as_array()).map(|a| a.len()))
    {
        Some(n) => format!("plan · {n} step{}", if n == 1 { "" } else { "s" }),
        None => "plan".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_coerce() {
        for s in ["completed", "Complete", "done", "finished", "resolved", "✓", "[x]", "x"] {
            assert_eq!(normalize_status(s), COMPLETED, "input: {s}");
        }
        for s in ["in_progress", "in-progress", "in progress", "active", "wip", "ongoing"] {
            assert_eq!(normalize_status(s), IN_PROGRESS, "input: {s}");
        }
        for s in ["failed", "fail", "blocked", "cancelled", "abandoned", "skipped"] {
            assert_eq!(normalize_status(s), FAILED, "input: {s}");
        }
        for s in ["pending", "todo", "not_started", "", "nope"] {
            assert_eq!(normalize_status(s), PENDING, "input: {s}");
        }
    }

    #[test]
    fn parses_full_args() {
        let items = parse_items(
            br#"{"plan":[{"content":"Investigate the bug","status":"done","notes":"repro found"},{"content":"Fix it"}]}"#,
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].content, "Investigate the bug");
        assert_eq!(items[0].status, COMPLETED);
        assert_eq!(items[0].notes, "repro found");
        assert_eq!(items[1].status, PENDING);
        assert!(items[1].notes.is_empty());
    }

    #[test]
    fn parses_id_and_extras_are_ignored() {
        let items =
            parse_items(br#"{"plan":[{"id":"abc","content":"Step","extra":123}]}"#).unwrap();
        assert_eq!(items[0].content, "Step");
        assert_eq!(items[0].status, PENDING);
    }

    #[test]
    fn rejects_bad_args() {
        for bad in [
            br#"{}"#.as_slice(),
            br#"{"plan":"nope"}"#,
            br#"{"plan":[{"status":"done"}]}"#,
            br#"not json"#,
        ] {
            assert!(parse_items(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn only_last_in_progress_survives() {
        let items = parse_items(
            br#"{"plan":[{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"},{"content":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(items[0].status, COMPLETED);
        assert_eq!(items[1].status, IN_PROGRESS);
        assert_eq!(items[2].status, PENDING);
    }

    #[test]
    fn renders_canonical_format() {
        let mut plan = Plan::default();
        plan.items = parse_items(
            br#"{"plan":[{"content":"Investigate","status":"in_progress"},{"content":"Fix","notes":"two files"}]}"#,
        )
        .unwrap();
        let text = plan.render();
        assert_eq!(
            text,
            "Current Plan:\n1. [in_progress] Investigate\n2. [pending] Fix\n   Notes: two files"
        );
        assert!(Plan::default().render().contains("empty"));
    }

    #[test]
    fn allowlist_policy_is_sane() {
        assert!(READ_ONLY_TOOLS.contains(&"update_plan"));
        assert!(READ_ONLY_TOOLS.contains(&"read"));
        for mutator in ["write", "edit", "bash", "agent_spawn", "agent_cancel", "mcp_call"] {
            assert!(
                !READ_ONLY_TOOLS.contains(&mutator),
                "mutator leaked into allowlist: {mutator}"
            );
            assert!(deny_reason(mutator).contains(mutator));
        }
    }
}
