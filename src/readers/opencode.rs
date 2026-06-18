use std::path::PathBuf;

use anyhow::Context;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;

use crate::error::{anyhow, Result};
use crate::ir::{
    AssistantMessage, AssistantPart, Session, ThinkingBlock, TokenUsage, ToolCall, Turn,
    UserMessage,
};
use crate::loss::{LossKind, LossReport};
use crate::readers::{Reader, SessionSummary};

pub struct OpenCodeReader {
    db_path: PathBuf,
}

impl OpenCodeReader {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open(&self) -> Result<Connection> {
        // Open the source DB read-only: we must never mutate the user's data
        // (the README promises the source is untouched), and read-only also
        // avoids creating -wal/-shm sidecars and works on read-only filesystems.
        let conn = Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening OpenCode DB read-only: {}", self.db_path.display()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(conn)
    }
}

impl Reader for OpenCodeReader {
    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let conn = self.open()?;
        let mut stmt = conn.prepare(
            "SELECT id, title, directory, time_updated FROM session
             WHERE time_archived IS NULL
             ORDER BY time_updated DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            // title/directory may be NULL for untitled/uncoded sessions; reading
            // them as Option avoids discarding the whole row (which previously
            // made such sessions silently vanish from the listing).
            Ok(SessionSummary {
                id: row.get::<_, String>(0)?,
                title: row
                    .get::<_, Option<String>>(1)?
                    .unwrap_or_else(|| "(untitled)".to_string()),
                cwd: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                updated_ms: row.get::<_, i64>(3)?,
            })
        })?;
        let summaries = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("reading session list from OpenCode DB")?;
        Ok(summaries)
    }

    fn read_session(&self, id: &str) -> Result<Session> {
        let conn = self.open()?;

        // Load session row
        let (project_id, title, directory, created_ms, updated_ms): (String, String, String, i64, i64) =
            conn.query_row(
                "SELECT project_id, title, directory, time_created, time_updated FROM session WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .with_context(|| format!("session {} not found in OpenCode DB", id))?;

        let _ = project_id; // not needed for IR

        let mut losses = LossReport::default();

        // Load messages ordered by time_created. DB-row errors are propagated;
        // a single message with malformed `data` JSON is recorded as a loss and
        // skipped rather than dropping it silently.
        let mut msg_stmt = conn.prepare(
            "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created ASC",
        )?;
        let mut messages: Vec<(String, Value)> = Vec::new();
        let rows = msg_stmt.query_map(params![id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for r in rows {
            let (mid, data_str) = r.context("reading message row from OpenCode DB")?;
            match serde_json::from_str::<Value>(&data_str) {
                Ok(v) => messages.push((mid, v)),
                Err(_) => losses.add(
                    LossKind::UnsupportedPartType,
                    1,
                    Some(format!("malformed message data JSON: {mid}")),
                ),
            }
        }
        let mut turns: Vec<Turn> = vec![];
        let mut i = 0;

        while i < messages.len() {
            let (user_mid, user_data) = &messages[i];
            let role = user_data["role"].as_str().unwrap_or("");

            if role != "user" {
                i += 1;
                continue;
            }

            // Load user text from parts
            let user_text = load_user_text(&conn, user_mid, id)?;
            let user_created_ms = user_data["time"]["created"].as_i64().unwrap_or(0);

            // Collect consecutive assistant messages for this turn
            let mut asst_msg: Option<AssistantMessage> = None;
            i += 1;

            while i < messages.len() {
                let (asst_mid, asst_data) = &messages[i];
                if asst_data["role"].as_str() != Some("assistant") {
                    break;
                }

                let tokens = parse_tokens(&asst_data["tokens"]);
                let cost = asst_data["cost"].as_f64();
                let model = asst_data["modelID"].as_str().map(String::from);
                let provider = asst_data["providerID"].as_str().map(String::from);
                let asst_created = asst_data["time"]["created"].as_i64().unwrap_or(0);

                let (parts, part_losses) = load_assistant_parts(&conn, asst_mid, id)?;
                losses.merge(part_losses);

                let msg = asst_msg.get_or_insert_with(|| AssistantMessage {
                    created_ms: asst_created,
                    ..Default::default()
                });
                if model.is_some() {
                    msg.model = model;
                }
                if provider.is_some() {
                    msg.provider = provider;
                }
                if tokens.is_some() {
                    msg.tokens = tokens;
                }
                if let Some(c) = cost {
                    msg.cost_usd = Some(msg.cost_usd.unwrap_or(0.0) + c);
                }
                // If there are already parts, add a StepBreak between assistant messages
                if !msg.parts.is_empty() && !parts.is_empty() {
                    msg.parts.push(AssistantPart::StepBreak);
                }
                msg.parts.extend(parts);
                i += 1;
            }

            turns.push(Turn {
                user: UserMessage {
                    created_ms: user_created_ms,
                    text: user_text,
                },
                assistant: asst_msg,
            });
        }

        // Check for todos
        let todo_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM todo WHERE session_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if todo_count > 0 {
            losses.add(LossKind::OpenCodeTodos, todo_count as usize, None);
        }

        Ok(Session {
            source_id: id.to_string(),
            title,
            cwd: directory,
            created_ms,
            updated_ms,
            turns,
            losses,
        })
    }
}

fn load_user_text(conn: &Connection, message_id: &str, session_id: &str) -> Result<String> {
    let mut stmt = conn.prepare(
        "SELECT data FROM part WHERE message_id = ?1 AND session_id = ?2 ORDER BY time_created ASC",
    )?;
    let parts: Vec<Value> = stmt
        .query_map(params![message_id, session_id], |row| {
            row.get::<_, String>(0)
        })?
        .filter_map(|r| r.ok())
        .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
        .collect();

    let text = parts
        .iter()
        .filter_map(|v| {
            if v["type"].as_str() == Some("text") {
                v["text"].as_str().map(String::from)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(text)
}

fn load_assistant_parts(
    conn: &Connection,
    message_id: &str,
    session_id: &str,
) -> Result<(Vec<AssistantPart>, LossReport)> {
    let mut stmt = conn.prepare(
        "SELECT data FROM part WHERE message_id = ?1 AND session_id = ?2 ORDER BY time_created ASC",
    )?;
    let parts: Vec<Value> = stmt
        .query_map(params![message_id, session_id], |row| {
            row.get::<_, String>(0)
        })?
        .filter_map(|r| r.ok())
        .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
        .collect();

    let mut out = vec![];
    let mut losses = LossReport::default();
    let mut first_step = true;

    for part in &parts {
        let ptype = part["type"].as_str().unwrap_or("");
        match ptype {
            "step-start" => {
                if !first_step {
                    out.push(AssistantPart::StepBreak);
                }
                first_step = false;
            }
            "step-finish" => {} // skip
            "text" => {
                let text = part["text"].as_str().unwrap_or("").to_string();
                if !text.is_empty() {
                    out.push(AssistantPart::Text(text));
                }
            }
            "reasoning" => {
                let text = part["text"].as_str().unwrap_or("").to_string();
                out.push(AssistantPart::Thinking(ThinkingBlock::Plaintext(text)));
            }
            "tool" => {
                let call_id = part["callID"].as_str().unwrap_or("").to_string();
                let tool_name = part["tool"].as_str().unwrap_or("").to_string();
                let state = &part["state"];
                let status = state["status"].as_str().unwrap_or("");
                let is_error = status == "error";
                let input = state["input"].clone();
                let output = if is_error {
                    state["error"].as_str().map(String::from)
                } else {
                    state["output"].as_str().map(String::from)
                };
                out.push(AssistantPart::ToolCall(ToolCall {
                    call_id,
                    tool_name,
                    input,
                    output,
                    is_error,
                }));
            }
            "subtask" | "compaction" | "patch" => {
                losses.add(LossKind::UnsupportedPartType, 1, Some(ptype.to_string()));
            }
            _ => {}
        }
    }

    Ok((out, losses))
}

fn parse_tokens(v: &Value) -> Option<TokenUsage> {
    if v.is_null() || !v.is_object() {
        return None;
    }
    Some(TokenUsage {
        input: v["input"].as_i64().unwrap_or(0),
        output: v["output"].as_i64().unwrap_or(0),
        reasoning: v["reasoning"].as_i64().unwrap_or(0),
        cache_read: v["cache"]["read"].as_i64().unwrap_or(0),
        cache_write: v["cache"]["write"].as_i64().unwrap_or(0),
    })
}
