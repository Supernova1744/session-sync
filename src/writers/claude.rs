use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use uuid::Uuid;

use crate::discover::encode_claude_path;
use crate::error::Result;
use crate::id::ms_to_iso;
use crate::ir::{AssistantPart, Session, ThinkingBlock, ToolCall};
use crate::loss::{LossKind, LossReport};
use crate::writers::{atomic_write, Writer};

pub struct ClaudeWriter {
    projects_dir: PathBuf,
}

impl ClaudeWriter {
    pub fn new(projects_dir: PathBuf) -> Self {
        Self { projects_dir }
    }
}

impl Writer for ClaudeWriter {
    fn write_session(&self, session: &Session, out_dir: Option<&Path>) -> Result<String> {
        let base = out_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.projects_dir.clone());

        let cwd = if session.cwd.is_empty() {
            "unknown"
        } else {
            &session.cwd
        };
        let project_dir = base.join(encode_claude_path(cwd));
        fs::create_dir_all(&project_dir)?;

        let session_uuid = Uuid::new_v4().to_string();
        let out_path = project_dir.join(format!("{}.jsonl", session_uuid));

        let mut losses = LossReport::default();
        let mut output = String::new();
        let mut parent_uuid: Option<String> = None;

        for turn in &session.turns {
            let prompt_id = Uuid::new_v4().to_string();

            // --- User prompt line -----------------------------------------------
            let user_uuid = Uuid::new_v4().to_string();
            push_line(
                &mut output,
                json!({
                    "type": "user",
                    "uuid": user_uuid,
                    "promptId": &prompt_id,
                    "timestamp": ms_to_iso(turn.user.created_ms),
                    "userType": "external",
                    "entrypoint": "cli",
                    "cwd": session.cwd,
                    "message": { "role": "user", "content": turn.user.text },
                    "permissionMode": "auto"
                }),
                parent_uuid.as_deref(),
                &session_uuid,
            )?;
            parent_uuid = Some(user_uuid);

            let Some(asst) = &turn.assistant else {
                continue;
            };

            // --- One assistant JSONL line per step ------------------------------
            for (step_idx, step_parts) in split_at_step_breaks(&asst.parts).into_iter().enumerate()
            {
                let asst_uuid = Uuid::new_v4().to_string();
                let asst_ts = ms_to_iso(asst.created_ms + step_idx as i64);

                let mut content_blocks: Vec<Value> = Vec::new();
                let mut tool_calls: Vec<&ToolCall> = Vec::new();

                for part in step_parts {
                    match part {
                        AssistantPart::Text(text) => {
                            content_blocks.push(json!({ "type": "text", "text": text }));
                        }
                        AssistantPart::Thinking(ThinkingBlock::Plaintext(text)) => {
                            content_blocks.push(json!({
                                "type": "thinking",
                                "thinking": text,
                                "id": Uuid::new_v4().to_string()
                            }));
                        }
                        AssistantPart::Thinking(ThinkingBlock::Opaque { .. }) => {
                            losses.add(LossKind::EncryptedThinking, 1, None);
                        }
                        AssistantPart::ToolCall(tc) => {
                            content_blocks.push(json!({
                                "type": "tool_use",
                                "id": tc.call_id,
                                "name": tc.tool_name,
                                "input": tc.input
                            }));
                            tool_calls.push(tc);
                        }
                        // `split_at_step_breaks` already split on these; a stray
                        // one here is harmless. (Defensive; should not occur.)
                        AssistantPart::StepBreak => {}
                    }
                }

                let stop_reason = if tool_calls.is_empty() {
                    "end_turn"
                } else {
                    "tool_use"
                };
                let usage = asst.tokens.as_ref().map(|t| {
                    json!({
                        "input_tokens": t.input,
                        "output_tokens": t.output,
                        "cache_read_input_tokens": t.cache_read,
                        "cache_creation_input_tokens": t.cache_write
                    })
                });

                let asst_message_id = Uuid::new_v4().to_string();

                push_line(
                    &mut output,
                    json!({
                        "type": "assistant",
                        "uuid": asst_uuid,
                        "timestamp": asst_ts,
                        "cwd": session.cwd,
                        "message": {
                            "id": asst_message_id,
                            "role": "assistant",
                            "content": content_blocks,
                            "stop_reason": stop_reason,
                            "model": asst.model.as_deref().unwrap_or("unknown"),
                            "type": "message",
                            "usage": usage
                        }
                    }),
                    parent_uuid.as_deref(),
                    &session_uuid,
                )?;
                parent_uuid = Some(asst_uuid);

                // --- One tool_result user line per tool call in this step ---------
                for tc in tool_calls {
                    let result_uuid = Uuid::new_v4().to_string();
                    let result_content = tc.output.as_deref().unwrap_or("");
                    push_line(
                        &mut output,
                        json!({
                            "type": "user",
                            "uuid": result_uuid,
                            "promptId": &prompt_id,
                            "timestamp": ms_to_iso(asst.created_ms + step_idx as i64 + 1),
                            "userType": "external",
                            "entrypoint": "cli",
                            "cwd": session.cwd,
                            "toolUseResult": result_content,
                            "message": {
                                "role": "user",
                                "content": [{
                                    "type": "tool_result",
                                    "tool_use_id": tc.call_id,
                                    "content": result_content,
                                    "is_error": tc.is_error
                                }]
                            }
                        }),
                        parent_uuid.as_deref(),
                        &session_uuid,
                    )?;
                    parent_uuid = Some(result_uuid);
                }
            }
        }

        // last-prompt pointer references the most recently emitted line. It does
        // NOT carry the parentUuid/isSidechain/version envelope, so emit directly.
        if let Some(leaf) = parent_uuid {
            let last_prompt_text = session.turns.last().map(|t| &t.user.text);
            let mut last_prompt_obj = json!({
                "type": "last-prompt",
                "leafUuid": leaf,
                "sessionId": &session_uuid
            });
            if let Some(text) = last_prompt_text {
                last_prompt_obj["lastPrompt"] = json!(text);
            }
            output.push_str(&serde_json::to_string(&last_prompt_obj)?);
            output.push('\n');
        }

        atomic_write(&out_path, output.as_bytes())?;

        if !losses.is_empty() {
            losses.print_summary();
        }

        Ok(out_path.to_string_lossy().into_owned())
    }
}

/// Split a list of assistant parts into steps delimited by `StepBreak`.
///
/// Returns references into `parts` (no cloning). Always yields at least one
/// (possibly empty) step; trailing empty steps are trimmed.
fn split_at_step_breaks(parts: &[AssistantPart]) -> Vec<Vec<&AssistantPart>> {
    let mut steps: Vec<Vec<&AssistantPart>> = vec![Vec::new()];
    for part in parts {
        match part {
            AssistantPart::StepBreak => steps.push(Vec::new()),
            _ => steps
                .last_mut()
                .expect("steps is initialized with one vec")
                .push(part),
        }
    }
    while steps.last().map(|s| s.is_empty()).unwrap_or(false) {
        steps.pop();
    }
    if steps.is_empty() {
        steps.push(Vec::new());
    }
    steps
}

/// Serialize one JSONL line with the shared envelope fields and append it.
fn push_line(
    out: &mut String,
    mut line: Value,
    parent: Option<&str>,
    session_uuid: &str,
) -> Result<()> {
    line["parentUuid"] = parent.map(Value::from).unwrap_or(Value::Null);
    line["isSidechain"] = json!(false);
    line["sessionId"] = json!(session_uuid);
    line["version"] = json!("2.1.143");
    line["gitBranch"] = json!("main");
    out.push_str(&serde_json::to_string(&line)?);
    out.push('\n');
    Ok(())
}
