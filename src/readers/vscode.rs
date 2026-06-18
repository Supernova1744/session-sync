use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::Connection;
use serde_json::Value;

use crate::discover::decode_vscode_folder_uri;
use crate::error::{anyhow, Result};
use crate::ir::{AssistantMessage, AssistantPart, Session, ThinkingBlock, ToolCall, Turn, UserMessage};
use crate::loss::{LossKind, LossReport};
use crate::readers::{Reader, SessionSummary};

pub struct VsCodeReader {
    workspace_storage_dirs: Vec<PathBuf>,
}

impl VsCodeReader {
    pub fn new(workspace_storage_dirs: Vec<PathBuf>) -> Self {
        Self { workspace_storage_dirs }
    }

    /// Find which hash dir contains the given session UUID (checks .jsonl then .json).
    fn find_hash_dir(&self, session_id: &str) -> Option<PathBuf> {
        for ws_dir in &self.workspace_storage_dirs {
            for entry in fs::read_dir(ws_dir).ok()?.flatten() {
                let chat_sessions_dir = entry.path().join("chatSessions");
                if chat_sessions_dir.join(format!("{}.jsonl", session_id)).exists()
                    || chat_sessions_dir.join(format!("{}.json", session_id)).exists()
                {
                    return Some(entry.path());
                }
            }
        }
        None
    }
}

impl Reader for VsCodeReader {
    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let mut summaries = vec![];

        for ws_dir in &self.workspace_storage_dirs {
            let entries = match fs::read_dir(ws_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for entry in entries.flatten() {
                let hash_dir = entry.path();
                if !hash_dir.is_dir() {
                    continue;
                }

                let workspace_json = hash_dir.join("workspace.json");
                let cwd = if workspace_json.exists() {
                    let content = fs::read_to_string(&workspace_json).unwrap_or_default();
                    let v: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
                    v["folder"]
                        .as_str()
                        .and_then(|uri| decode_vscode_folder_uri(uri))
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    String::new()
                };

                let db_path = hash_dir.join("state.vscdb");
                if !db_path.exists() {
                    continue;
                }

                let chat_dir = hash_dir.join("chatSessions");
                let sessions = read_session_index(&db_path).unwrap_or_default();
                for mut s in sessions {
                    // Accept sessions with either .jsonl or .json file
                    let has_file = chat_dir.join(format!("{}.jsonl", s.id)).exists()
                        || chat_dir.join(format!("{}.json", s.id)).exists();
                    if !has_file {
                        continue;
                    }
                    if s.cwd.is_empty() {
                        s.cwd = cwd.clone();
                    }
                    summaries.push(s);
                }
            }
        }

        summaries.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(summaries)
    }

    fn read_session(&self, id: &str) -> Result<Session> {
        let hash_dir = self
            .find_hash_dir(id)
            .ok_or_else(|| anyhow!("VS Code session not found: {}", id))?;

        let cwd = {
            let workspace_json = hash_dir.join("workspace.json");
            let content = fs::read_to_string(&workspace_json).unwrap_or_default();
            let v: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
            v["folder"]
                .as_str()
                .and_then(|uri| decode_vscode_folder_uri(uri))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        };

        let chat_dir = hash_dir.join("chatSessions");

        // Prefer .jsonl (native format), fall back to .json (legacy format)
        let jsonl_path = chat_dir.join(format!("{}.jsonl", id));
        if jsonl_path.exists() {
            let content = fs::read_to_string(&jsonl_path)
                .with_context(|| format!("reading VS Code session: {}", jsonl_path.display()))?;
            return parse_vscode_jsonl(&content, id, &cwd);
        }

        let json_path = chat_dir.join(format!("{}.json", id));
        let content = fs::read_to_string(&json_path)
            .with_context(|| format!("reading VS Code session: {}", json_path.display()))?;
        let v: Value = serde_json::from_str(&content)?;
        parse_vscode_session_json(&v, id, &cwd)
    }
}

fn read_session_index(db_path: &Path) -> Result<Vec<SessionSummary>> {
    let conn = Connection::open(db_path)?;
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'chat.ChatSessionStore.index'",
            [],
            |row| row.get(0),
        )
        .ok();

    let Some(json_str) = value else {
        return Ok(vec![]);
    };

    let index: Value = serde_json::from_str(&json_str)?;
    let entries = match index["entries"].as_object() {
        Some(e) => e,
        None => return Ok(vec![]),
    };

    Ok(entries
        .values()
        .map(|e| SessionSummary {
            id:         e["sessionId"].as_str().unwrap_or("").to_string(),
            title:      e["title"].as_str().unwrap_or("(untitled)").to_string(),
            updated_ms: e["lastMessageDate"].as_i64().unwrap_or(0),
            cwd:        String::new(),
        })
        .filter(|s| !s.id.is_empty())
        .collect())
}

/// Parse the native JSONL format.
/// Supports both:
/// - 0.53.0+: single kind=0 line with all requests in header's `requests` array
/// - 0.52.x:  kind=0 header with `requests:[]`, plus one kind=2 line per turn
fn parse_vscode_jsonl(content: &str, session_id: &str, cwd: &str) -> Result<Session> {
    let mut title = String::from("(untitled)");
    let mut created_ms: i64 = 0;
    let mut updated_ms: i64 = 0;
    let mut losses = LossReport::default();
    let mut turns = vec![];
    let mut seen_request_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: Value = serde_json::from_str(line)?;
        let kind = obj["kind"].as_i64().unwrap_or(-1);

        match kind {
            0 => {
                let v = &obj["v"];
                if let Some(t) = v["customTitle"].as_str().filter(|s| !s.is_empty()) {
                    title = t.to_string();
                } else if let Some(t) = v["title"].as_str().filter(|s| !s.is_empty()) {
                    title = t.to_string();
                }
                created_ms = v["creationDate"].as_i64().unwrap_or(0);

                // 0.53.0+ format: requests embedded in header
                if let Some(reqs) = v["requests"].as_array() {
                    for req in reqs {
                        if req["isCanceled"].as_bool() == Some(true) {
                            losses.add(LossKind::CanceledRequest, 1, None);
                            continue;
                        }
                        let req_id = req["requestId"].as_str().unwrap_or("").to_string();
                        if !req_id.is_empty() {
                            seen_request_ids.insert(req_id);
                        }
                        if let Some(turn) = parse_request_turn(req, created_ms, &mut losses) {
                            if turn.user.created_ms > updated_ms {
                                updated_ms = turn.user.created_ms;
                            }
                            turns.push(turn);
                        }
                    }
                }
            }
            1 => {
                if let Some(t) = obj["v"].as_str().filter(|s| !s.is_empty()) {
                    if t != "GitHub Copilot" {
                        title = t.to_string();
                    }
                }
            }
            2 => {
                // 0.52.x format: each turn is a kind=2 line
                let items = match obj["v"].as_array() {
                    Some(a) if !a.is_empty() => a,
                    _ => continue,
                };
                for item in items {
                    if item["isCanceled"].as_bool() == Some(true) {
                        losses.add(LossKind::CanceledRequest, 1, None);
                        continue;
                    }
                    // Skip if already parsed from header (avoid duplicates)
                    let req_id = item["requestId"].as_str().unwrap_or("");
                    if !req_id.is_empty() && seen_request_ids.contains(req_id) {
                        continue;
                    }
                    if let Some(turn) = parse_request_turn(item, created_ms, &mut losses) {
                        if turn.user.created_ms > updated_ms {
                            updated_ms = turn.user.created_ms;
                        }
                        turns.push(turn);
                    }
                }
            }
            _ => {}
        }
    }

    if !turns.is_empty() {
        losses.add(LossKind::TokenCounts, 1, Some("VS Code doesn't store raw token counts".to_string()));
    }

    Ok(Session {
        source_id: session_id.to_string(),
        title,
        cwd: cwd.to_string(),
        created_ms,
        updated_ms,
        turns,
        losses,
    })
}

fn parse_request_turn(req: &Value, fallback_ms: i64, losses: &mut LossReport) -> Option<Turn> {
    let user_text = req["message"]["text"].as_str().unwrap_or("").to_string();
    if user_text.is_empty() {
        return None;
    }
    let user_created_ms = req["timestamp"].as_i64().unwrap_or(fallback_ms);
    let model = req["modelId"].as_str().map(String::from);

    let (asst_parts, part_losses) = parse_response(req["response"].as_array());
    losses.merge(part_losses);

    let assistant = if asst_parts.is_empty() {
        None
    } else {
        Some(AssistantMessage {
            created_ms: req["modelState"]["completedAt"]
                .as_i64()
                .unwrap_or(user_created_ms),
            model,
            provider: Some("copilot".to_string()),
            tokens: None,
            cost_usd: None,
            parts: asst_parts,
        })
    };

    Some(Turn {
        user: UserMessage { created_ms: user_created_ms, text: user_text },
        assistant,
    })
}

/// Parse the legacy JSON format: top-level `requests` array.
fn parse_vscode_session_json(v: &Value, session_id: &str, cwd: &str) -> Result<Session> {
    let title = v["customTitle"]
        .as_str()
        .or_else(|| v["title"].as_str())
        .unwrap_or("(untitled)")
        .to_string();
    let created_ms = v["creationDate"].as_i64().unwrap_or(0);
    let updated_ms = v["lastMessageDate"].as_i64().unwrap_or(0);

    let requests = v["requests"].as_array().cloned().unwrap_or_default();
    let mut losses = LossReport::default();
    let mut turns = vec![];

    for req in &requests {
        if req["isCanceled"].as_bool() == Some(true) {
            losses.add(LossKind::CanceledRequest, 1, None);
            continue;
        }

        let user_text = req["message"]["text"].as_str().unwrap_or("").to_string();
        if user_text.is_empty() {
            continue;
        }
        let user_created_ms = req["timestamp"].as_i64().unwrap_or(created_ms);
        let model = req["modelId"].as_str().map(String::from);

        let (asst_parts, part_losses) = parse_response(req["response"].as_array());
        losses.merge(part_losses);

        let assistant = if asst_parts.is_empty() {
            None
        } else {
            Some(AssistantMessage {
                created_ms: user_created_ms,
                model,
                provider: Some("copilot".to_string()),
                tokens: None,
                cost_usd: None,
                parts: asst_parts,
            })
        };

        turns.push(Turn {
            user: UserMessage { created_ms: user_created_ms, text: user_text },
            assistant,
        });
    }

    if !turns.is_empty() {
        losses.add(LossKind::TokenCounts, 1, Some("VS Code doesn't store raw token counts".to_string()));
    }

    Ok(Session {
        source_id: session_id.to_string(),
        title,
        cwd: cwd.to_string(),
        created_ms,
        updated_ms,
        turns,
        losses,
    })
}

fn parse_response(response: Option<&Vec<Value>>) -> (Vec<AssistantPart>, LossReport) {
    let mut parts = vec![];
    let mut losses = LossReport::default();
    let Some(items) = response else {
        return (parts, losses);
    };

    for item in items {
        let kind = item["kind"].as_str();
        match kind {
            None => {
                if let Some(text) = item["value"].as_str() {
                    if !text.is_empty() {
                        parts.push(AssistantPart::Text(text.to_string()));
                    }
                }
            }
            Some("thinking") => {
                let id = item["id"].as_str().unwrap_or("").to_string();
                let value = item["value"].as_str().unwrap_or("").to_string();
                parts.push(AssistantPart::Thinking(ThinkingBlock::Opaque { id, value }));
            }
            Some("prepareToolInvocation") => {}
            Some("toolInvocationSerialized") => {
                let tool_name = item["toolId"]
                    .as_str()
                    .or_else(|| item["invocationMessage"].as_str())
                    .unwrap_or("unknown_tool")
                    .to_string();

                losses.add(LossKind::ToolInputUnavailable, 1, None);

                let output = item["pastTenseMessage"]["value"]
                    .as_str()
                    .map(String::from);

                let call_id = item["toolCallId"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

                parts.push(AssistantPart::ToolCall(ToolCall {
                    call_id,
                    tool_name,
                    input: Value::Null,
                    output,
                    is_error: false,
                }));
            }
            Some("inlineReference") | Some("terminal") | Some("confirmation")
            | Some("mcpServersStarting") | Some("agent") | Some("progressTaskSerialized") => {}
            Some("text") => {
                if let Some(text) = item["value"].as_str() {
                    if !text.is_empty() {
                        parts.push(AssistantPart::Text(text.to_string()));
                    }
                }
            }
            _ => {}
        }
    }

    (parts, losses)
}
