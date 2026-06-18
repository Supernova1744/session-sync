use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use uuid::Uuid;

use crate::discover::encode_claude_path;
use crate::error::Result;
use crate::id::ms_to_iso;
use crate::ir::{AssistantPart, Session, ThinkingBlock};
use crate::loss::{LossKind, LossReport};
use crate::writers::Writer;

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

        let cwd = if session.cwd.is_empty() { "unknown" } else { &session.cwd };
        let encoded = encode_claude_path(cwd);
        let project_dir = base.join(&encoded);
        fs::create_dir_all(&project_dir)?;

        let session_uuid = Uuid::new_v4().to_string();
        let out_path = project_dir.join(format!("{}.jsonl", session_uuid));

        let mut losses = LossReport::default();
        let mut output = String::new();
        let mut parent_uuid: Option<String> = None;

        let emit = |output: &mut String, line: Value, parent: &Option<String>| {
            let mut obj = line;
            obj["parentUuid"] = parent.as_deref().map(Value::from).unwrap_or(Value::Null);
            obj["isSidechain"] = json!(false);
            obj["sessionId"] = json!(session_uuid.clone());
            obj["version"] = json!("0.0.0");
            output.push_str(&serde_json::to_string(&obj).unwrap_or_default());
            output.push('\n');
        };

        for turn in &session.turns {
            let user_uuid = Uuid::new_v4().to_string();
            let user_ts = ms_to_iso(turn.user.created_ms);

            let user_line = json!({
                "type": "user",
                "uuid": user_uuid,
                "timestamp": user_ts,
                "userType": "external",
                "entrypoint": "cli",
                "cwd": session.cwd,
                "message": {
                    "role": "user",
                    "content": turn.user.text
                },
                "permissionMode": "auto"
            });
            emit(&mut output, user_line, &parent_uuid);
            parent_uuid = Some(user_uuid.clone());

            let Some(asst) = &turn.assistant else { continue };

            // Split parts at StepBreak boundaries
            let steps = split_at_step_breaks(&asst.parts);

            for (step_idx, step_parts) in steps.iter().enumerate() {
                let asst_uuid = Uuid::new_v4().to_string();
                let asst_ts = ms_to_iso(asst.created_ms + step_idx as i64);

                let mut content_blocks: Vec<Value> = vec![];
                let mut tool_calls_this_step: Vec<(String, String, Value, Option<String>, bool)> = vec![];

                for part in step_parts {
                    match part {
                        AssistantPart::Text(text) => {
                            content_blocks.push(json!({"type": "text", "text": text}));
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
                            tool_calls_this_step.push((
                                tc.call_id.clone(),
                                tc.tool_name.clone(),
                                tc.input.clone(),
                                tc.output.clone(),
                                tc.is_error,
                            ));
                        }
                        AssistantPart::StepBreak => unreachable!(),
                    }
                }

                let has_tools = !tool_calls_this_step.is_empty();
                let stop_reason = if has_tools { "tool_use" } else { "end_turn" };

                let usage = asst.tokens.as_ref().map(|t| {
                    json!({
                        "input_tokens": t.input,
                        "output_tokens": t.output,
                        "cache_read_input_tokens": t.cache_read,
                        "cache_creation_input_tokens": t.cache_write
                    })
                });

                let mut asst_line = json!({
                    "type": "assistant",
                    "uuid": asst_uuid,
                    "timestamp": asst_ts,
                    "cwd": session.cwd,
                    "message": {
                        "role": "assistant",
                        "content": content_blocks,
                        "stop_reason": stop_reason,
                        "model": asst.model.as_deref().unwrap_or("unknown"),
                        "type": "message",
                        "usage": usage
                    }
                });
                emit(&mut output, asst_line.take(), &parent_uuid);
                parent_uuid = Some(asst_uuid.clone());

                // Emit tool_result user lines
                for (call_id, _, _, output_text, is_error) in &tool_calls_this_step {
                    let result_uuid = Uuid::new_v4().to_string();
                    let result_ts = ms_to_iso(asst.created_ms + step_idx as i64 + 1);
                    let result_content = output_text.as_deref().unwrap_or("");

                    let result_line = json!({
                        "type": "user",
                        "uuid": result_uuid,
                        "timestamp": result_ts,
                        "userType": "external",
                        "entrypoint": "cli",
                        "cwd": session.cwd,
                        "toolUseResult": result_content,
                        "message": {
                            "role": "user",
                            "content": [{
                                "type": "tool_result",
                                "tool_use_id": call_id,
                                "content": result_content,
                                "is_error": is_error
                            }]
                        }
                    });
                    emit(&mut output, result_line, &parent_uuid);
                    parent_uuid = Some(result_uuid);
                }
            }
        }

        // Emit last-prompt pointer
        if let Some(ref leaf) = parent_uuid {
            let last_prompt = json!({
                "type": "last-prompt",
                "leafUuid": leaf,
                "sessionId": session_uuid
            });
            output.push_str(&serde_json::to_string(&last_prompt).unwrap_or_default());
            output.push('\n');
        }

        fs::write(&out_path, output.as_bytes())?;

        if !losses.is_empty() {
            losses.print_summary();
        }

        Ok(out_path.to_string_lossy().into_owned())
    }
}

fn split_at_step_breaks(parts: &[AssistantPart]) -> Vec<Vec<AssistantPart>> {
    let mut steps: Vec<Vec<AssistantPart>> = vec![vec![]];
    for part in parts {
        match part {
            AssistantPart::StepBreak => steps.push(vec![]),
            other => steps.last_mut().unwrap().push(other.clone()),
        }
    }
    // Remove empty trailing steps
    while steps.last().map(|s| s.is_empty()).unwrap_or(false) {
        steps.pop();
    }
    if steps.is_empty() {
        steps.push(vec![]);
    }
    steps
}
