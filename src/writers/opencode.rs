use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{params, Connection, Transaction};
use serde_json::{json, Value};

use crate::error::Result;
use crate::id::{new_message_id, new_part_id, new_session_id, new_slug, now_ms};
use crate::ir::{AssistantPart, Session, ThinkingBlock};
use crate::loss::{LossKind, LossReport};
use crate::writers::Writer;

pub struct OpenCodeWriter {
    db_path: PathBuf,
}

impl OpenCodeWriter {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open_at(db_path: &Path) -> Result<Connection> {
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening OpenCode DB: {}", db_path.display()))?;
        // busy_timeout: if OpenCode itself is running and holds the DB, wait up
        // to 5s for the lock instead of failing immediately with SQLITE_BUSY.
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(conn)
    }
}

impl Writer for OpenCodeWriter {
    fn write_session(&self, session: &Session, out_dir: Option<&Path>) -> Result<String> {
        // Resolve the target DB path. `out_dir` may be either a directory that
        // should contain `opencode.db` (mirrors the reader's `--dir` handling in
        // main.rs) or the DB file path itself. When omitted, write to the user's
        // native OpenCode DB.
        let db_path = match out_dir {
            Some(p) if p.file_name().and_then(|n| n.to_str()) == Some("opencode.db") => {
                p.to_path_buf()
            }
            Some(dir) => dir.join("opencode.db"),
            None => self.db_path.clone(),
        };
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut conn = Self::open_at(&db_path)?;
        let mut losses = LossReport::default();

        // Normalize empty cwd to "/" so session.directory matches the global project's worktree
        let directory = if session.cwd.is_empty() {
            "/"
        } else {
            session.cwd.as_str()
        };

        // Aggregate token usage across every assistant message in one pass.
        let (tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write) =
            session
                .turns
                .iter()
                .filter_map(|t| t.assistant.as_ref())
                .filter_map(|a| a.tokens.as_ref())
                .fold((0i64, 0, 0, 0, 0), |(i, o, r, cr, cw), t| {
                    (
                        i + t.input,
                        o + t.output,
                        r + t.reasoning,
                        cr + t.cache_read,
                        cw + t.cache_write,
                    )
                });
        let total_cost: f64 = session.total_cost();

        let ses_id = new_session_id(session.created_ms);
        let slug = new_slug();

        let tx = conn.transaction()?;
        ensure_global_project(&tx)?;

        // OpenCode stores all sessions under the global project and uses session.directory
        // for the path association. Non-global project IDs are invisible to the session list.
        let path = directory.strip_prefix('/').unwrap_or(directory);

        tx.execute(
            "INSERT INTO session (id, project_id, parent_id, slug, directory, path, title, version,
             agent, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write,
             time_created, time_updated)
             VALUES (?1,'global',NULL,?2,?3,?4,?5,'0.0.0','build',?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                ses_id,
                slug,
                directory,
                path,
                session.title,
                total_cost,
                tokens_input,
                tokens_output,
                tokens_reasoning,
                tokens_cache_read,
                tokens_cache_write,
                session.created_ms,
                session.updated_ms,
            ],
        )?;

        for turn in &session.turns {
            let user_msg_id = new_message_id(turn.user.created_ms);
            let user_data = json!({
                "role": "user",
                "time": { "created": turn.user.created_ms },
                "agent": "build",
                "model": { "providerID": "unknown", "modelID": "unknown" },
                "summary": { "diffs": [] }
            });
            insert_message(&tx, &user_msg_id, &ses_id, turn.user.created_ms, user_data)?;

            // User text part. (time_created is the user message's timestamp; the
            // legacy `t += 1` here was dead — `t` is reset below before reuse.)
            let user_part_data = json!({ "type": "text", "text": turn.user.text });
            insert_part(
                &tx,
                &user_msg_id,
                &ses_id,
                turn.user.created_ms,
                user_part_data,
            )?;

            let Some(asst) = &turn.assistant else {
                continue;
            };

            let asst_msg_id = new_message_id(asst.created_ms);
            let asst_data = json!({
                "parentID": user_msg_id,
                "role": "assistant",
                "mode": "build",
                "agent": "build",
                "path": { "cwd": directory, "root": "/" },
                "cost": asst.cost_usd.unwrap_or(0.0),
                "tokens": asst.tokens.as_ref().map(|tok| json!({
                    "total": tok.input + tok.output,
                    "input": tok.input,
                    "output": tok.output,
                    "reasoning": tok.reasoning,
                    "cache": { "write": tok.cache_write, "read": tok.cache_read }
                })),
                "modelID": asst.model.as_deref().unwrap_or("unknown"),
                "providerID": asst.provider.as_deref().unwrap_or("unknown"),
                "time": { "created": asst.created_ms, "completed": asst.created_ms },
                "finish": "end-turn"
            });
            insert_message(&tx, &asst_msg_id, &ses_id, asst.created_ms, asst_data)?;

            // `t` is a monotonic tick giving each part a distinct id and
            // time_created within this assistant message. Each insert consumes
            // the current tick, then we advance.
            let mut t = asst.created_ms;

            // Open the first step.
            insert_part(
                &tx,
                &asst_msg_id,
                &ses_id,
                t,
                json!({ "type": "step-start" }),
            )?;
            t += 1;

            let mut last_had_tools = false;

            for part in &asst.parts {
                match part {
                    AssistantPart::StepBreak => {
                        let reason = if last_had_tools {
                            "tool-calls"
                        } else {
                            "end-turn"
                        };
                        insert_part(
                            &tx,
                            &asst_msg_id,
                            &ses_id,
                            t,
                            json!({ "type": "step-finish", "reason": reason }),
                        )?;
                        t += 1;
                        insert_part(
                            &tx,
                            &asst_msg_id,
                            &ses_id,
                            t,
                            json!({ "type": "step-start" }),
                        )?;
                        t += 1;
                        last_had_tools = false;
                    }
                    AssistantPart::Text(text) => {
                        insert_part(
                            &tx,
                            &asst_msg_id,
                            &ses_id,
                            t,
                            json!({ "type": "text", "text": text, "time": { "start": t, "end": t + 1 } }),
                        )?;
                        t += 1;
                    }
                    AssistantPart::Thinking(ThinkingBlock::Plaintext(text)) => {
                        insert_part(
                            &tx,
                            &asst_msg_id,
                            &ses_id,
                            t,
                            json!({
                                "type": "reasoning",
                                "text": text,
                                "time": { "start": t, "end": t + 1 }
                            }),
                        )?;
                        t += 1;
                    }
                    AssistantPart::Thinking(ThinkingBlock::Opaque { .. }) => {
                        losses.add(LossKind::EncryptedThinking, 1, None);
                    }
                    AssistantPart::ToolCall(tc) => {
                        last_had_tools = true;
                        let status = if tc.is_error { "error" } else { "completed" };
                        let state = if tc.is_error {
                            json!({
                                "status": status,
                                "input": tc.input,
                                "error": tc.output.as_deref().unwrap_or(""),
                                "time": { "start": t, "end": t + 1 }
                            })
                        } else {
                            json!({
                                "status": status,
                                "input": tc.input,
                                "output": tc.output.as_deref().unwrap_or(""),
                                "time": { "start": t, "end": t + 1 }
                            })
                        };
                        let data = json!({
                            "type": "tool",
                            "tool": tc.tool_name,
                            "callID": tc.call_id,
                            "state": state
                        });
                        insert_part(&tx, &asst_msg_id, &ses_id, t, data)?;
                        t += 1;
                    }
                }
            }

            // Close the final step.
            let reason = if last_had_tools {
                "tool-calls"
            } else {
                "end-turn"
            };
            insert_part(
                &tx,
                &asst_msg_id,
                &ses_id,
                t,
                json!({ "type": "step-finish", "reason": reason }),
            )?;
        }

        tx.commit()?;
        losses.print_summary();
        Ok(ses_id)
    }
}

/// Insert one row into the `message` table. `time_updated` mirrors `time_created`.
fn insert_message(
    tx: &Transaction,
    msg_id: &str,
    ses_id: &str,
    created_ms: i64,
    data: Value,
) -> Result<()> {
    tx.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1,?2,?3,?3,?4)",
        params![msg_id, ses_id, created_ms, data.to_string()],
    )?;
    Ok(())
}

/// Insert one row into the `part` table. `seq` is used for both the part id and
/// `time_created`; `time_updated` mirrors it. Caller advances `seq` between calls.
fn insert_part(tx: &Transaction, msg_id: &str, ses_id: &str, seq: i64, data: Value) -> Result<()> {
    tx.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
         VALUES (?1,?2,?3,?4,?4,?5)",
        params![new_part_id(seq), msg_id, ses_id, seq, data.to_string()],
    )?;
    Ok(())
}

fn ensure_global_project(conn: &Connection) -> Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT OR IGNORE INTO project (id, worktree, sandboxes, time_created, time_updated)
         VALUES ('global', '/', '[]', ?1, ?1)",
        params![now],
    )?;
    Ok(())
}
