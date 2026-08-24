use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use std::collections::HashMap;
use std::path::PathBuf;

/// Kiro CLI (2.x) hooks preset.
///
/// Kiro 2.x embeds hooks in agent configs and feeds event context via stdin:
/// core fields are `hook_event_name`, `cwd`, `session_id`; tool events add
/// `tool_name`, `tool_input` (and `tool_response` for postToolUse).
/// `transcript_path` is NOT part of the documented payload, so it is treated
/// as optional here and model extraction falls back to "unknown" without
/// blocking attribution.
pub struct KiroPreset;

impl AgentPreset for KiroPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let cwd = parse::required_str(&data, "cwd")?;

        let session_id = parse::optional_str(&data, "session_id")
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let tool_name = parse::optional_str(&data, "tool_name");
        let hook_event = parse::optional_str(&data, "hook_event_name");
        let tool_use_id =
            parse::str_or_default_multi(&data, &["tool_use_id", "toolUseId"], "kiro");

        let is_bash = tool_name
            .map(|n| bash_tool::classify_tool(Agent::Kiro, n) == ToolClass::Bash)
            .unwrap_or(false);

        // transcript_path is not part of Kiro's documented hook payload; keep it
        // optional so we never hard-fail on a missing field. Model stays unknown.
        let transcript_path = parse::optional_str(&data, "transcript_path").unwrap_or("");

        let model = if transcript_path.is_empty() {
            "unknown".to_string()
        } else {
            crate::streams::model_extraction::extract_model(
                std::path::Path::new(transcript_path),
                crate::streams::sweep::StreamFormat::ClaudeJsonl,
                None,
            )
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string())
        };

        let context = PresetContext {
            agent_id: AgentId {
                tool: "kiro".to_string(),
                id: session_id.clone(),
                model,
            },
            external_session_id: session_id.clone(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata: HashMap::from([(
                "transcript_path".to_string(),
                transcript_path.to_string(),
            )]),
        };

        let bash_command = parse::bash_command_from_hook_input(&data);
        let event = match (hook_event, is_bash) {
            (Some("preToolUse"), true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: bash_command,
            }),
            (Some("preToolUse"), false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: parse::file_paths_from_tool_input(&data, cwd),
                dirty_files: None,
                tool_use_id: Some(tool_use_id.to_string()),
            }),
            (_, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: bash_command,
                stream_source: None,
            }),
            (_, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths: parse::file_paths_from_tool_input(&data, cwd),
                dirty_files: None,
                stream_source: None,
                tool_use_id: Some(tool_use_id.to_string()),
            }),
        };

        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;

    fn make_kiro_hook_input(event: &str, tool: &str) -> String {
        json!({
            "hook_event_name": event,
            "session_id": "sess-kiro-1",
            "cwd": "/workspace/project",
            "tool_name": tool,
            "tool_input": {"file_path": "/workspace/project/src/main.rs"}
        })
        .to_string()
    }

    #[test]
    fn test_kiro_pre_file_edit_fs_write() {
        let input = make_kiro_hook_input("preToolUse", "fs_write");
        let events = KiroPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "kiro");
                assert_eq!(e.context.external_session_id, "sess-kiro-1");
                assert_eq!(e.context.trace_id, "t_test123456789a");
                assert_eq!(e.context.cwd, PathBuf::from("/workspace/project"));
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/workspace/project/src/main.rs")]
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_kiro_post_file_edit_write_alias() {
        let input = make_kiro_hook_input("postToolUse", "write");
        let events = KiroPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "kiro");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/workspace/project/src/main.rs")]
                );
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_kiro_pre_bash_execute_bash() {
        let input = make_kiro_hook_input("preToolUse", "execute_bash");
        let events = KiroPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "kiro");
                assert_eq!(e.tool_use_id, "kiro");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_kiro_post_bash_shell_alias() {
        let input = make_kiro_hook_input("postToolUse", "shell");
        let events = KiroPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "kiro");
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_kiro_read_tool_is_not_bash() {
        // fs_read must not classify as Bash; it produces a file-edit style
        // event whose file list is empty (fs_read has no file_path in tool_input
        // for edits), which the checkpoint pipeline drops safely.
        let input = make_kiro_hook_input("postToolUse", "fs_read");
        let events = KiroPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "kiro");
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_kiro_missing_session_id_falls_back() {
        let input = json!({
            "hook_event_name": "postToolUse",
            "cwd": "/workspace/project",
            "tool_name": "fs_write",
            "tool_input": {"file_path": "/workspace/project/src/main.rs"}
        })
        .to_string();
        let events = KiroPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.external_session_id, "unknown");
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_kiro_missing_cwd_is_error() {
        let input = json!({
            "hook_event_name": "postToolUse",
            "session_id": "s1",
            "tool_name": "fs_write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        assert!(KiroPreset.parse(&input, "t_test123456789a").is_err());
    }
}
