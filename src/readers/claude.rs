use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::Value;

use crate::error::{anyhow, Result};
use crate::ir::{AssistantMessage, AssistantPart, Session, ThinkingBlock, TokenUsage, ToolCall, Turn, UserMessage};
use crate::loss::{LossKind, LossReport};
use crate::readers::{Reader, SessionSummary};

pub struct ClaudeReader {
    projects_dir: PathBuf,
}

impl ClaudeReader {
    pub fn new(projects_dir: PathBuf) -> Self {
        Self { projects_dir }
    }

    /// Locate the .jsonl file for a given session UUID anywhere under projects_dir.
    fn find_jsonl(&self, session_id: &str) -> Option<PathBuf> {
        let filename = format!("{}.jsonl", session_id);
        for entry in fs::read_dir(&self.projects_dir).ok()?.flatten() {
            let candidate = entry.path().join(&filename);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }
}

impl Reader for ClaudeReader {
    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let mut summaries = vec![];

        for project_entry in fs::read_dir(&self.projects_dir)
            .context("reading claude projects dir")?
            .flatten()
        {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            for file_entry in fs::read_dir(&project_path).into_iter().flatten().flatten() {
                let p = file_entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let id = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                // Quick-scan first few lines for metadata
                let summary = scan_jsonl_for_summary(&p, &id);
                summaries.push(summary);
            }
        }

        summaries.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(summaries)
    }

    fn read_session(&self, id: &str) -> Result<Session> {
        let path = self
            .find_jsonl(id)
            .ok_or_else(|| anyhow!("Claude session not found: {}", id))?;
        parse_jsonl(&path, id)
    }
}

fn scan_jsonl_for_summary(path: &Path, id: &str) -> SessionSummary {
    let content = fs::read_to_string(path).unwrap_or_default();
    let mut title = String::from("(untitled)");
    let mut updated_ms = 0i64;
    let mut cwd = String::new();

    for line in content.lines().take(50) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let t = v["type"].as_str().unwrap_or("");
        if t == "ai-title" {
            if let Some(s) = v["aiTitle"].as_str() {
                title = s.to_string();
            }
        }
        if t == "user" || t == "assistant" {
            if let Some(ts) = v["timestamp"].as_str() {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                    let ms = dt.timestamp_millis();
                    if ms > updated_ms {
                        updated_ms = ms;
                    }
                }
            }
            if cwd.is_empty() {
                if let Some(c) = v["cwd"].as_str() {
                    cwd = c.to_string();
                }
            }
        }
    }

    // Fallback title from first user message
    if title == "(untitled)" {
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if v["type"].as_str() == Some("user") {
                let msg_content = &v["message"]["content"];
                let text = if let Some(s) = msg_content.as_str() {
                    s.to_string()
                } else if let Some(arr) = msg_content.as_array() {
                    arr.iter()
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    String::new()
                };
                if !text.is_empty() {
                    let truncated: String = text.chars().take(60).collect();
                    title = if text.len() > 60 {
                        format!("{}…", truncated)
                    } else {
                        truncated
                    };
                    break;
                }
            }
        }
    }

    SessionSummary { id: id.to_string(), title, updated_ms, cwd }
}

fn parse_jsonl(path: &Path, session_id: &str) -> Result<Session> {
    let content = fs::read_to_string(path)?;
    let mut losses = LossReport::default();

    let mut turns: Vec<Turn> = vec![];
    // Current pending user message
    let mut current_user: Option<UserMessage> = None;
    // Parts accumulating for the current assistant response
    let mut asst_parts: Vec<AssistantPart> = vec![];
    // Metadata for the current assistant
    let mut asst_meta: Option<AssistantMessage> = None;
    // call_id → index in asst_parts (for stitching tool results)
    let mut pending_calls: HashMap<String, usize> = HashMap::new();
    // How many assistant lines we've seen in the current turn (for StepBreak)
    let mut asst_step_count = 0usize;

    let mut session_cwd = String::new();
    let mut session_created_ms = i64::MAX;
    let mut session_updated_ms = 0i64;
    let mut session_title = String::from("(untitled)");

    let flush_turn = |turns: &mut Vec<Turn>,
                      current_user: &mut Option<UserMessage>,
                      asst_parts: &mut Vec<AssistantPart>,
                      asst_meta: &mut Option<AssistantMessage>,
                      pending_calls: &mut HashMap<String, usize>,
                      asst_step_count: &mut usize| {
        if let Some(user) = current_user.take() {
            let assistant = asst_meta.take().map(|mut a| {
                a.parts = asst_parts.drain(..).collect();
                a
            });
            if assistant.is_none() {
                asst_parts.clear();
            }
            turns.push(Turn { user, assistant });
        }
        pending_calls.clear();
        *asst_step_count = 0;
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Skip side-chain messages (background sub-tasks)
        if v["isSidechain"].as_bool() == Some(true) {
            continue;
        }

        let msg_type = v["type"].as_str().unwrap_or("");

        // Track session-level metadata
        if let Some(ts) = v["timestamp"].as_str() {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                let ms = dt.timestamp_millis();
                if ms < session_created_ms {
                    session_created_ms = ms;
                }
                if ms > session_updated_ms {
                    session_updated_ms = ms;
                }
            }
        }
        if session_cwd.is_empty() {
            if let Some(c) = v["cwd"].as_str() {
                session_cwd = c.to_string();
            }
        }
        if msg_type == "ai-title" {
            if let Some(t) = v["aiTitle"].as_str() {
                session_title = t.to_string();
            }
        }

        match msg_type {
            "user" => {
                let content = &v["message"]["content"];

                // Check if this is a tool_result message
                let tool_results = extract_tool_results(content);
                if !tool_results.is_empty() {
                    // Stitch results into pending ToolCall parts
                    for (call_id, output, is_error) in tool_results {
                        if let Some(&idx) = pending_calls.get(&call_id) {
                            if let Some(AssistantPart::ToolCall(tc)) = asst_parts.get_mut(idx) {
                                tc.output = Some(output);
                                tc.is_error = is_error;
                            }
                        } else {
                            losses.add(LossKind::UnsupportedPartType, 1,
                                Some("tool result with no matching tool_use in current turn".to_string()));
                        }
                    }
                    continue;
                }

                // New user prompt — flush previous turn
                flush_turn(
                    &mut turns,
                    &mut current_user,
                    &mut asst_parts,
                    &mut asst_meta,
                    &mut pending_calls,
                    &mut asst_step_count,
                );

                // Any asst_parts still present after flush means they preceded the first user message
                if !asst_parts.is_empty() {
                    losses.add(LossKind::UnsupportedPartType, asst_parts.len(),
                        Some("assistant content before first user message".to_string()));
                }
                let text = extract_user_text(content);
                let ts_ms = parse_timestamp_ms(v["timestamp"].as_str());
                current_user = Some(UserMessage { created_ms: ts_ms, text });
                asst_parts.clear();
                asst_meta = None;
            }

            "assistant" => {
                if asst_step_count > 0 {
                    asst_parts.push(AssistantPart::StepBreak);
                }
                asst_step_count += 1;

                let ts_ms = parse_timestamp_ms(v["timestamp"].as_str());
                let msg = &v["message"];

                // Parse token usage
                let tokens = parse_usage(&msg["usage"]);
                let model = msg["model"].as_str().map(String::from);

                // Initialize or update assistant metadata
                let meta = asst_meta.get_or_insert_with(|| AssistantMessage {
                    created_ms: ts_ms,
                    ..Default::default()
                });
                meta.model = model.or(meta.model.clone());
                if let Some(t) = tokens {
                    meta.tokens = Some(t);
                }

                // Parse content blocks
                if let Some(blocks) = msg["content"].as_array() {
                    for block in blocks {
                        let btype = block["type"].as_str().unwrap_or("");
                        match btype {
                            "text" => {
                                let text = block["text"].as_str().unwrap_or("").to_string();
                                if !text.is_empty() {
                                    asst_parts.push(AssistantPart::Text(text));
                                }
                            }
                            "thinking" => {
                                let text = block["thinking"].as_str().unwrap_or("").to_string();
                                asst_parts.push(AssistantPart::Thinking(
                                    ThinkingBlock::Plaintext(text),
                                ));
                            }
                            "tool_use" => {
                                let call_id =
                                    block["id"].as_str().unwrap_or("").to_string();
                                let tool_name =
                                    block["name"].as_str().unwrap_or("").to_string();
                                let input = block["input"].clone();
                                let idx = asst_parts.len();
                                asst_parts.push(AssistantPart::ToolCall(ToolCall {
                                    call_id: call_id.clone(),
                                    tool_name,
                                    input,
                                    output: None,
                                    is_error: false,
                                }));
                                pending_calls.insert(call_id, idx);
                            }
                            _ => {}
                        }
                    }
                }
            }

            "attachment" => {
                losses.add(LossKind::HookAttachment, 1, None);
            }
            "file-history-snapshot" => {
                losses.add(LossKind::FileHistorySnapshot, 1, None);
            }
            // Silently skip: mode, permission-mode, last-prompt, system, ai-title
            _ => {}
        }
    }

    // Flush final turn
    flush_turn(
        &mut turns,
        &mut current_user,
        &mut asst_parts,
        &mut asst_meta,
        &mut pending_calls,
        &mut asst_step_count,
    );

    if session_created_ms == i64::MAX {
        session_created_ms = 0;
    }

    Ok(Session {
        source_id: session_id.to_string(),
        title: session_title,
        cwd: session_cwd,
        created_ms: session_created_ms,
        updated_ms: session_updated_ms,
        turns,
        losses,
    })
}

fn extract_user_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|b| {
                if b["type"].as_str() == Some("text") {
                    b["text"].as_str().map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn extract_tool_results(content: &Value) -> Vec<(String, String, bool)> {
    let mut results = vec![];
    if let Some(arr) = content.as_array() {
        for block in arr {
            if block["type"].as_str() == Some("tool_result") {
                let call_id = block["tool_use_id"].as_str().unwrap_or("").to_string();
                let is_error = block["is_error"].as_bool().unwrap_or(false);
                let output = if let Some(s) = block["content"].as_str() {
                    s.to_string()
                } else if let Some(arr) = block["content"].as_array() {
                    arr.iter()
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                };
                if !call_id.is_empty() {
                    results.push((call_id, output, is_error));
                }
            }
        }
    }
    results
}

fn parse_timestamp_ms(ts: Option<&str>) -> i64 {
    ts.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn parse_usage(v: &Value) -> Option<TokenUsage> {
    if v.is_null() || !v.is_object() {
        return None;
    }
    Some(TokenUsage {
        input: v["input_tokens"].as_i64().unwrap_or(0)
            + v["cache_creation_input_tokens"].as_i64().unwrap_or(0),
        output: v["output_tokens"].as_i64().unwrap_or(0),
        reasoning: 0,
        cache_read: v["cache_read_input_tokens"].as_i64().unwrap_or(0),
        cache_write: v["cache_creation_input_tokens"].as_i64().unwrap_or(0),
    })
}
