//! Canonical event-kind normalization shared between the SSH and the
//! alleycat paths.
//!
//! The two transports speak different vocabularies for the same
//! logical turn:
//!
//!   * SSH (`drive_turn`) consumes codex `session/update` notifications
//!     whose `update.sessionUpdate` discriminator is one of
//!     `tool_call` / `tool_call_update` / `agent_message_chunk` /
//!     `toolUse`. Tool-call status lives under `update.status`
//!     (`pending` / `in_progress` / `completed` / `failed`).
//!
//!   * Alleycat (`drive_alleycat_turn`) consumes alleycat-pi-bridge
//!     `item/started` and `item/completed` notifications whose
//!     `item.type` is `commandExecution` / `agentMessage` /
//!     `userMessage`. Tool-call status lives under `item.status`.
//!
//! VAL-REM-010 requires that the same logical turn emits an
//! identical *set* of `event_kind` values on the JSONL transcript
//! across both paths (validated by `diff <(jq -r .event_kind ... |
//! sort -u) <(jq -r .event_kind ... | sort -u)`). The helper below
//! maps both vocabularies onto a single
//! [`NormalizedEventKind`] enum so the runner can emit a dedicated
//! transcript line for each canonical kind from either transport
//! without duplicating the dispatch logic.

use serde_json::Value as JsonValue;

/// Canonical event kind emitted on the JSONL transcript. Each
/// variant's [`as_event_kind`](Self::as_event_kind) string is the
/// literal value that lands in the `event_kind` JSONL field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizedEventKind {
    /// A tool call has been started (or its in-progress state is
    /// being streamed). The transcript carries the tool name,
    /// command, and the raw transport payload for full fidelity.
    ToolExec,
    /// A tool call has reached a terminal state (`completed` or
    /// `failed`). Validators key off this line to know the tool
    /// finished.
    ToolExecResult,
    /// An assistant message chunk (streaming token delta or final
    /// chunk). Both vocabularies stream assistant text in chunks
    /// during a turn, and validators measure assistant output by
    /// counting these lines.
    AgentMessageChunk,
}

impl NormalizedEventKind {
    /// The string written to the JSONL `event_kind` field. Kept
    /// public so future callers (and the unit tests in this module)
    /// can pin against the literal strings.
    #[allow(dead_code)]
    pub fn as_event_kind(self) -> &'static str {
        match self {
            NormalizedEventKind::ToolExec => "tool_exec",
            NormalizedEventKind::ToolExecResult => "tool_exec_result",
            NormalizedEventKind::AgentMessageChunk => "agent_message_chunk",
        }
    }
}

/// Map a codex `session/update` notification's `params` payload onto
/// a [`NormalizedEventKind`]. Returns `None` for notifications that
/// have no canonical mapping (those fall through to a generic
/// `server_notification` transcript line).
///
/// Only `method == "session/update"` should be fed to this helper;
/// anything else returns `None`.
pub fn normalize_codex_session_update(
    method: &str,
    params: &JsonValue,
) -> Option<NormalizedEventKind> {
    if method != "session/update" {
        return None;
    }
    let update = params.get("update").unwrap_or(params);
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .and_then(JsonValue::as_str)?;
    match kind {
        "agent_message_chunk" | "assistantMessageChunk" => {
            Some(NormalizedEventKind::AgentMessageChunk)
        }
        "tool_call" | "toolCall" | "toolUse" => Some(NormalizedEventKind::ToolExec),
        "tool_call_update" | "toolCallUpdate" => {
            // Only terminal status transitions cross the canonical
            // boundary. Intermediate `in_progress`/`pending` updates
            // fall through to `server_notification` so the SSH path
            // emits a single `tool_exec` for the initial `tool_call`
            // and a single `tool_exec_result` for the terminal
            // update, matching the alleycat path's `item/started` /
            // `item/completed` pair (VAL-REM-010).
            let status = update.get("status").and_then(JsonValue::as_str);
            if matches!(status, Some("completed") | Some("failed") | Some("error")) {
                Some(NormalizedEventKind::ToolExecResult)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Map an alleycat-pi-bridge `item/started` or `item/completed`
/// notification's `params` payload onto a [`NormalizedEventKind`].
/// Returns `None` for notifications that have no canonical mapping.
pub fn normalize_alleycat_item(
    method: &str,
    params: &JsonValue,
) -> Option<NormalizedEventKind> {
    if method != "item/started" && method != "item/completed" {
        return None;
    }
    let item = params.get("item").or_else(|| params.get("itemUpdate"))?;
    let kind = item
        .get("type")
        .and_then(JsonValue::as_str)
        .or_else(|| item.get("kind").and_then(JsonValue::as_str))?;
    match (method, kind) {
        // Tool execution: started -> tool_exec; completed -> tool_exec_result.
        ("item/started", "commandExecution")
        | ("item/started", "command_exec")
        | ("item/started", "tool_exec")
        | ("item/started", "execution_started") => Some(NormalizedEventKind::ToolExec),
        ("item/completed", "commandExecution")
        | ("item/completed", "command_exec")
        | ("item/completed", "tool_exec")
        | ("item/completed", "execution_completed") => {
            Some(NormalizedEventKind::ToolExecResult)
        }
        // Assistant messages: each `item/*` boundary on an
        // agentMessage is part of the streaming chunk lifecycle.
        (_, "agentMessage") | (_, "assistantMessage") => {
            Some(NormalizedEventKind::AgentMessageChunk)
        }
        _ => None,
    }
}

/// Also recognise alleycat's `item/agentMessage/delta` notification
/// as an assistant message chunk. Validators need every delta line
/// to count toward `agent_message_chunk` so the SSH-path stream of
/// `agent_message_chunk` notifications has a matching counterpart.
pub fn normalize_alleycat_method(method: &str) -> Option<NormalizedEventKind> {
    match method {
        "item/agentMessage/delta" | "item/assistantMessage/delta" => {
            Some(NormalizedEventKind::AgentMessageChunk)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- Codex session/update vocabulary -------------------------------------

    #[test]
    fn codex_tool_call_maps_to_tool_exec() {
        let params = json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1",
                "toolName": "Bash",
                "status": "pending",
                "rawInput": {"command": "ls /root"},
            }
        });
        assert_eq!(
            normalize_codex_session_update("session/update", &params),
            Some(NormalizedEventKind::ToolExec)
        );
    }

    #[test]
    fn codex_tool_call_update_in_progress_falls_through() {
        // Intermediate `in_progress` updates intentionally do NOT
        // produce a canonical event — only the terminal `completed`
        // / `failed` update does. This keeps the SSH path emitting
        // exactly one `tool_exec` + one `tool_exec_result`, matching
        // the alleycat `item/started` + `item/completed` pair.
        let params = json!({
            "update": {
                "sessionUpdate": "tool_call_update",
                "status": "in_progress",
            }
        });
        assert!(normalize_codex_session_update("session/update", &params).is_none());
    }

    #[test]
    fn codex_tool_call_update_completed_maps_to_tool_exec_result() {
        let params = json!({
            "update": {
                "sessionUpdate": "tool_call_update",
                "status": "completed",
            }
        });
        assert_eq!(
            normalize_codex_session_update("session/update", &params),
            Some(NormalizedEventKind::ToolExecResult)
        );
    }

    #[test]
    fn codex_tool_call_update_failed_maps_to_tool_exec_result() {
        let params = json!({
            "update": {
                "sessionUpdate": "tool_call_update",
                "status": "failed",
            }
        });
        assert_eq!(
            normalize_codex_session_update("session/update", &params),
            Some(NormalizedEventKind::ToolExecResult)
        );
    }

    #[test]
    fn codex_agent_message_chunk_maps_to_agent_message_chunk() {
        let params = json!({
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "Hi!"}
            }
        });
        assert_eq!(
            normalize_codex_session_update("session/update", &params),
            Some(NormalizedEventKind::AgentMessageChunk)
        );
    }

    #[test]
    fn codex_unknown_session_update_is_passthrough() {
        let params = json!({
            "update": {"sessionUpdate": "plan_update"}
        });
        assert!(normalize_codex_session_update("session/update", &params).is_none());
    }

    #[test]
    fn codex_non_session_update_method_returns_none() {
        let params = json!({"update": {"sessionUpdate": "tool_call"}});
        assert!(normalize_codex_session_update("session/created", &params).is_none());
    }

    // --- Alleycat-pi-bridge `item/*` vocabulary -------------------------------

    #[test]
    fn alleycat_item_started_command_execution_maps_to_tool_exec() {
        let params = json!({
            "item": {
                "type": "commandExecution",
                "command": "ls /root",
                "id": "tool-1",
                "status": "inProgress",
            }
        });
        assert_eq!(
            normalize_alleycat_item("item/started", &params),
            Some(NormalizedEventKind::ToolExec)
        );
    }

    #[test]
    fn alleycat_item_completed_command_execution_maps_to_tool_exec_result() {
        let params = json!({
            "item": {
                "type": "commandExecution",
                "command": "ls /root",
                "id": "tool-1",
                "status": "failed",
            }
        });
        assert_eq!(
            normalize_alleycat_item("item/completed", &params),
            Some(NormalizedEventKind::ToolExecResult)
        );
    }

    #[test]
    fn alleycat_item_legacy_execution_started_kind_maps_to_tool_exec() {
        // Older alleycat-pi-bridge builds stamped the tool-exec
        // event as `kind == "execution_started"` rather than the
        // newer `type == "commandExecution"`. Both must normalize.
        let params = json!({
            "item": {
                "kind": "execution_started",
                "command": "ls /root",
                "id": "tool-2",
            }
        });
        assert_eq!(
            normalize_alleycat_item("item/started", &params),
            Some(NormalizedEventKind::ToolExec)
        );
    }

    #[test]
    fn alleycat_item_legacy_execution_completed_kind_maps_to_tool_exec_result() {
        let params = json!({
            "item": {
                "kind": "execution_completed",
                "id": "tool-2",
            }
        });
        assert_eq!(
            normalize_alleycat_item("item/completed", &params),
            Some(NormalizedEventKind::ToolExecResult)
        );
    }

    #[test]
    fn alleycat_agent_message_maps_to_agent_message_chunk() {
        let params = json!({
            "item": {"type": "agentMessage", "text": ""}
        });
        assert_eq!(
            normalize_alleycat_item("item/started", &params),
            Some(NormalizedEventKind::AgentMessageChunk)
        );
        assert_eq!(
            normalize_alleycat_item("item/completed", &params),
            Some(NormalizedEventKind::AgentMessageChunk)
        );
    }

    #[test]
    fn alleycat_user_message_is_passthrough() {
        let params = json!({"item": {"type": "userMessage"}});
        assert!(normalize_alleycat_item("item/started", &params).is_none());
    }

    #[test]
    fn alleycat_non_item_method_returns_none() {
        let params = json!({"item": {"type": "commandExecution"}});
        assert!(normalize_alleycat_item("thread/started", &params).is_none());
    }

    #[test]
    fn alleycat_agent_message_delta_method_maps_to_chunk() {
        assert_eq!(
            normalize_alleycat_method("item/agentMessage/delta"),
            Some(NormalizedEventKind::AgentMessageChunk)
        );
    }

    #[test]
    fn alleycat_other_method_returns_none_from_method_helper() {
        assert!(normalize_alleycat_method("turn/started").is_none());
    }

    // --- Canonical event_kind strings -----------------------------------------

    #[test]
    fn event_kind_strings_are_stable() {
        assert_eq!(NormalizedEventKind::ToolExec.as_event_kind(), "tool_exec");
        assert_eq!(
            NormalizedEventKind::ToolExecResult.as_event_kind(),
            "tool_exec_result"
        );
        assert_eq!(
            NormalizedEventKind::AgentMessageChunk.as_event_kind(),
            "agent_message_chunk"
        );
    }
}
