use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{params, Connection};
use serde_json::json;

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

    fn open(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("opening OpenCode DB: {}", self.db_path.display()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        Ok(conn)
    }
}

impl Writer for OpenCodeWriter {
    fn write_session(&self, session: &Session, _out_dir: Option<&Path>) -> Result<String> {
        let mut conn = self.open()?;
        let mut losses = LossReport::default();

        // Normalize empty cwd to "/" so session.directory matches the global project's worktree
        let directory = if session.cwd.is_empty() { "/" } else { session.cwd.as_str() };

        // Aggregate totals for session row
        let total_tokens_input: i64 = session
            .turns
            .iter()
            .flat_map(|t| t.assistant.as_ref())
            .filter_map(|a| a.tokens.as_ref())
            .map(|t| t.input)
            .sum();
        let total_tokens_output: i64 = session
            .turns
            .iter()
            .flat_map(|t| t.assistant.as_ref())
            .filter_map(|a| a.tokens.as_ref())
            .map(|t| t.output)
            .sum();
        let total_tokens_reasoning: i64 = session
            .turns
            .iter()
            .flat_map(|t| t.assistant.as_ref())
            .filter_map(|a| a.tokens.as_ref())
            .map(|t| t.reasoning)
            .sum();
        let total_cache_read: i64 = session
            .turns
            .iter()
            .flat_map(|t| t.assistant.as_ref())
            .filter_map(|a| a.tokens.as_ref())
            .map(|t| t.cache_read)
            .sum();
        let total_cache_write: i64 = session
            .turns
            .iter()
            .flat_map(|t| t.assistant.as_ref())
            .filter_map(|a| a.tokens.as_ref())
            .map(|t| t.cache_write)
            .sum();
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
                total_tokens_input,
                total_tokens_output,
                total_tokens_reasoning,
                total_cache_read,
                total_cache_write,
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
            tx.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1,?2,?3,?3,?4)",
                params![user_msg_id, ses_id, turn.user.created_ms, user_data.to_string()],
            )?;

            // User text part
            let mut t = turn.user.created_ms;
            let user_part_id = new_part_id(t);
            t += 1;
            let user_part_data = json!({"type": "text", "text": turn.user.text});
            tx.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1,?2,?3,?4,?4,?5)",
                params![user_part_id, user_msg_id, ses_id, turn.user.created_ms, user_part_data.to_string()],
            )?;

            let Some(asst) = &turn.assistant else { continue };

            let asst_msg_id = new_message_id(asst.created_ms);
            let finish_reason = "end-turn";
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
                "finish": finish_reason
            });
            tx.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1,?2,?3,?3,?4)",
                params![asst_msg_id, ses_id, asst.created_ms, asst_data.to_string()],
            )?;

            // Write parts
            t = asst.created_ms;

            // Open first step
            let step_start_id = new_part_id(t);
            t += 1;
            tx.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1,?2,?3,?4,?4,?5)",
                params![step_start_id, asst_msg_id, ses_id, t - 1, json!({"type":"step-start"}).to_string()],
            )?;

            let mut last_had_tools = false;

            for part in &asst.parts {
                match part {
                    AssistantPart::StepBreak => {
                        let finish_id = new_part_id(t);
                        t += 1;
                        let finish_data = json!({
                            "type": "step-finish",
                            "reason": if last_had_tools { "tool-calls" } else { "end-turn" }
                        });
                        tx.execute(
                            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                             VALUES (?1,?2,?3,?4,?4,?5)",
                            params![finish_id, asst_msg_id, ses_id, t - 1, finish_data.to_string()],
                        )?;
                        let start_id = new_part_id(t);
                        t += 1;
                        tx.execute(
                            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                             VALUES (?1,?2,?3,?4,?4,?5)",
                            params![start_id, asst_msg_id, ses_id, t - 1, json!({"type":"step-start"}).to_string()],
                        )?;
                        last_had_tools = false;
                    }
                    AssistantPart::Text(text) => {
                        let pid = new_part_id(t);
                        t += 1;
                        let data = json!({"type": "text", "text": text, "time": {"start": t-1, "end": t}});
                        tx.execute(
                            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                             VALUES (?1,?2,?3,?4,?4,?5)",
                            params![pid, asst_msg_id, ses_id, t - 1, data.to_string()],
                        )?;
                    }
                    AssistantPart::Thinking(ThinkingBlock::Plaintext(text)) => {
                        let pid = new_part_id(t);
                        t += 1;
                        let data = json!({"type": "reasoning", "text": text, "time": {"start": t-1, "end": t}});
                        tx.execute(
                            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                             VALUES (?1,?2,?3,?4,?4,?5)",
                            params![pid, asst_msg_id, ses_id, t - 1, data.to_string()],
                        )?;
                    }
                    AssistantPart::Thinking(ThinkingBlock::Opaque { .. }) => {
                        losses.add(LossKind::EncryptedThinking, 1, None);
                    }
                    AssistantPart::ToolCall(tc) => {
                        last_had_tools = true;
                        let pid = new_part_id(t);
                        t += 1;
                        let status = if tc.is_error { "error" } else { "completed" };
                        let state = if tc.is_error {
                            json!({
                                "status": status,
                                "input": tc.input,
                                "error": tc.output.as_deref().unwrap_or(""),
                                "time": {"start": t-1, "end": t}
                            })
                        } else {
                            json!({
                                "status": status,
                                "input": tc.input,
                                "output": tc.output.as_deref().unwrap_or(""),
                                "time": {"start": t-1, "end": t}
                            })
                        };
                        let data = json!({
                            "type": "tool",
                            "tool": tc.tool_name,
                            "callID": tc.call_id,
                            "state": state
                        });
                        tx.execute(
                            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                             VALUES (?1,?2,?3,?4,?4,?5)",
                            params![pid, asst_msg_id, ses_id, t - 1, data.to_string()],
                        )?;
                    }
                }
            }

            // Close final step
            let finish_id = new_part_id(t);
            t += 1;
            let finish_data = json!({
                "type": "step-finish",
                "reason": if last_had_tools { "tool-calls" } else { "end-turn" }
            });
            tx.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1,?2,?3,?4,?4,?5)",
                params![finish_id, asst_msg_id, ses_id, t - 1, finish_data.to_string()],
            )?;
        }

        tx.commit()?;
        losses.print_summary();
        Ok(ses_id)
    }
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
