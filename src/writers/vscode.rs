use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::Connection;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::discover::{decode_vscode_folder_uri, wsl_distro_name};
use crate::error::{anyhow, Result};
use crate::ir::{AssistantPart, Session, ThinkingBlock};
use crate::loss::{LossKind, LossReport};
use crate::writers::{atomic_write, Writer};

pub struct VsCodeWriter {
    workspace_storage_dirs: Vec<PathBuf>,
}

impl VsCodeWriter {
    pub fn new(workspace_storage_dirs: Vec<PathBuf>) -> Self {
        Self {
            workspace_storage_dirs,
        }
    }

    /// Find the workspace hash dir whose workspace.json folder matches `cwd`.
    fn find_hash_dir_for_cwd(&self, cwd: &str) -> Option<PathBuf> {
        for ws_dir in &self.workspace_storage_dirs {
            let Ok(entries) = fs::read_dir(ws_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let hash_dir = entry.path();
                let workspace_json = hash_dir.join("workspace.json");
                if !workspace_json.exists() {
                    continue;
                }
                let Ok(content) = fs::read_to_string(&workspace_json) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&content) else {
                    continue;
                };
                if let Some(folder_uri) = v["folder"].as_str() {
                    if let Some(folder_path) = decode_vscode_folder_uri(folder_uri) {
                        if folder_path.to_string_lossy() == cwd {
                            return Some(hash_dir);
                        }
                    }
                }
            }
        }
        None
    }

    /// Encode a filesystem path as a `file://` URI in the format VS Code expects.
    /// On WSL, VS Code uses file://wsl.localhost/<distro>/... for Linux paths.
    fn path_to_file_uri(path: &str) -> String {
        let encode_segments = |p: &str| -> String {
            p.split('/')
                .map(|seg| urlencoding::encode(seg).into_owned())
                .collect::<Vec<_>>()
                .join("/")
        };

        if let Some(distro) = wsl_distro_name() {
            let without_slash = path.trim_start_matches('/');
            return format!(
                "file://wsl.localhost/{}/{}",
                distro,
                encode_segments(without_slash)
            );
        }

        format!("file://{}", encode_segments(path))
    }

    /// Create an `imported-<uuid8>` hash dir inside `ws_dir` and write workspace.json.
    fn create_imported_hash_dir(ws_dir: &Path, cwd: &str) -> Result<PathBuf> {
        let hash_dir = ws_dir.join(format!("imported-{}", &Uuid::new_v4().to_string()[..8]));
        fs::create_dir_all(&hash_dir)?;
        if !cwd.is_empty() {
            let workspace_json = json!({ "folder": Self::path_to_file_uri(cwd) });
            atomic_write(
                &hash_dir.join("workspace.json"),
                serde_json::to_string_pretty(&workspace_json)?.as_bytes(),
            )?;
        }
        Ok(hash_dir)
    }

    /// Pick the target hash dir: out_dir parent, matched existing workspace, or new imported dir.
    fn resolve_hash_dir(&self, session: &Session, out_dir: Option<&Path>) -> Result<PathBuf> {
        if let Some(ws_dir) = out_dir {
            return Self::create_imported_hash_dir(ws_dir, &session.cwd);
        }

        if !session.cwd.is_empty() {
            if let Some(dir) = self.find_hash_dir_for_cwd(&session.cwd) {
                return Ok(dir);
            }
        }

        for ws_dir in &self.workspace_storage_dirs {
            if ws_dir.exists() {
                return Self::create_imported_hash_dir(ws_dir, &session.cwd);
            }
        }

        Err(anyhow!(
            "No VS Code workspaceStorage directory found. Is VS Code installed?"
        ))
    }
}

impl Writer for VsCodeWriter {
    fn write_session(&self, session: &Session, out_dir: Option<&Path>) -> Result<String> {
        let hash_dir = self.resolve_hash_dir(session, out_dir)?;
        let chat_sessions_dir = hash_dir.join("chatSessions");
        fs::create_dir_all(&chat_sessions_dir)?;

        let new_uuid = Uuid::new_v4().to_string();
        let mut losses = LossReport::default();
        let mut requests: Vec<Value> = vec![];

        for turn in &session.turns {
            let request_uuid = Uuid::new_v4().to_string();
            let response_uuid = Uuid::new_v4().to_string();
            let mut response_items: Vec<Value> = vec![];

            if let Some(asst) = &turn.assistant {
                for part in &asst.parts {
                    match part {
                        AssistantPart::Text(text) => {
                            response_items.push(json!({
                                "value": text,
                                "supportThemeIcons": false,
                                "supportHtml": false
                            }));
                        }
                        AssistantPart::Thinking(ThinkingBlock::Opaque { id, value }) => {
                            response_items.push(json!({
                                "kind": "thinking",
                                "id": id,
                                "value": value
                            }));
                        }
                        AssistantPart::Thinking(ThinkingBlock::Plaintext(text)) => {
                            response_items.push(json!({
                                "value": format!("[Thinking]\n{}", text),
                                "supportThemeIcons": false,
                                "supportHtml": false
                            }));
                        }
                        AssistantPart::ToolCall(tc) => {
                            losses.add(LossKind::ToolInputUnavailable, 0, None);
                            response_items.push(json!({
                                "kind": "prepareToolInvocation",
                                "toolName": tc.tool_name
                            }));
                            response_items.push(json!({
                                "kind": "toolInvocationSerialized",
                                "invocationMessage": tc.tool_name,
                                "pastTenseMessage": {
                                    "value": tc.output.as_deref().unwrap_or(""),
                                    "isTrusted": false,
                                    "supportThemeIcons": false,
                                    "supportHtml": false
                                },
                                "isConfirmed": {"type": 1},
                                "isComplete": true,
                                "source": {"type": "internal", "label": "Built-In"},
                                "toolCallId": tc.call_id,
                                "toolSpecificData": {
                                    "kind": "generic",
                                    "input": tc.input,
                                    "output": tc.output
                                }
                            }));
                        }
                        AssistantPart::StepBreak => {}
                    }
                }

                if asst.tokens.is_some() {
                    losses.add(LossKind::TokenCounts, 1, None);
                }
            }

            let completed_at = if let Some(asst) = &turn.assistant {
                if asst.created_ms > 0 {
                    asst.created_ms
                } else {
                    turn.user.created_ms
                }
            } else {
                turn.user.created_ms
            };

            // VS Code / Monaco positions are UTF-16 code-unit based and 1-based
            // line numbers, so measure in UTF-16 (not chars) and end on the line
            // that actually contains the last character (not always line 1).
            let utf16_len = turn.user.text.encode_utf16().count();
            let line_count = turn.user.text.lines().count().max(1);
            let last_line_utf16 = turn
                .user
                .text
                .rsplit('\n')
                .next()
                .unwrap_or("")
                .encode_utf16()
                .count();
            requests.push(json!({
                "requestId": format!("request_{}", request_uuid),
                "timestamp": turn.user.created_ms,
                "agent": {
                    "extensionId": {"value": "GitHub.copilot-chat", "_lower": "github.copilot-chat"},
                    "extensionDisplayName": "GitHub Copilot Chat",
                    "id": "github.copilot.editsAgent",
                    "name": "agent",
                    "fullName": "GitHub Copilot"
                },
                "modelId": "copilot/auto",
                "responseId": format!("response_{}", response_uuid),
                "result": {
                    "timings": {"firstProgress": 0, "totalElapsed": 0},
                    "metadata": {"codeBlocks": []}
                },
                "responseMarkdownInfo": [],
                "followups": [],
                "modelState": {"value": 1, "completedAt": completed_at},
                "contentReferences": [],
                "codeCitations": [],
                "timeSpentWaiting": 0,
                "completionTokens": 0,
                "elapsedMs": 0,
                "modeInfo": {
                    "kind": "agent",
                    "isBuiltin": true,
                    "modeId": "agent",
                    "modeName": "agent",
                    "permissionLevel": "default"
                },
                "response": response_items,
                "message": {
                    "text": turn.user.text,
                    "parts": [{
                        "range": {"start": 0, "endExclusive": utf16_len},
                        "editorRange": {
                            "startLineNumber": 1, "startColumn": 1,
                            "endLineNumber": line_count as i64,
                            "endColumn": (last_line_utf16 + 1) as i64
                        },
                        "text": turn.user.text,
                        "kind": "text"
                    }]
                },
                "variableData": {"variables": []}
            }));
        }

        // VS Code Copilot Chat 0.53.0 format: single JSONL line, all requests in header
        let session_line = serde_json::to_string(&json!({
            "kind": 0,
            "v": {
                "version": 3,
                "creationDate": session.created_ms,
                "customTitle": session.title,
                "initialLocation": "panel",
                "responderUsername": "GitHub Copilot",
                "sessionId": new_uuid,
                "hasPendingEdits": false,
                "requests": requests,
                "pendingRequests": [],
                "inputState": {
                    "attachments": [],
                    "mode": {"id": "agent", "kind": "agent"},
                    "selectedModel": {
                        "identifier": "copilot/auto",
                        "metadata": {
                            "id": "auto",
                            "vendor": "copilot",
                            "name": "Auto",
                            "isUserSelectable": true,
                            "capabilities": {
                                "vision": true,
                                "toolCalling": true,
                                "agentMode": true
                            }
                        }
                    },
                    "inputText": "",
                    "selections": [{
                        "startLineNumber": 1, "startColumn": 1,
                        "endLineNumber": 1, "endColumn": 1,
                        "selectionStartLineNumber": 1, "selectionStartColumn": 1,
                        "positionLineNumber": 1, "positionColumn": 1
                    }],
                    "permissionLevel": "default",
                    "contrib": {"chatDynamicVariableModel": []}
                }
            }
        }))?;

        let session_file = chat_sessions_dir.join(format!("{}.jsonl", new_uuid));
        atomic_write(&session_file, session_line.as_bytes())
            .with_context(|| format!("writing session file: {}", session_file.display()))?;

        // Update state.vscdb index. If this fails (e.g. the DB is corrupt or
        // SQLite stays locked past the busy timeout), remove the orphan session
        // file so we never leave a file that no index references.
        let db_path = hash_dir.join("state.vscdb");
        if let Err(e) = update_vscdb_index(&db_path, &new_uuid, session) {
            // On Windows the session file may still be locked by VS Code, so
            // don't claim it was removed unless remove_file actually succeeded.
            let removed = fs::remove_file(&session_file).is_ok();
            let msg = if removed {
                "updating state.vscdb index failed; orphan session file removed"
            } else {
                "updating state.vscdb index failed; orphan session file could not be removed"
            };
            return Err(e).context(msg);
        }

        losses.print_summary();
        Ok(session_file.to_string_lossy().into_owned())
    }
}

fn update_vscdb_index(db_path: &Path, session_uuid: &str, session: &Session) -> Result<()> {
    let mut conn = Connection::open(db_path)
        .with_context(|| format!("opening state.vscdb: {}", db_path.display()))?;
    // busy_timeout: VS Code itself usually has this DB open. Without a timeout
    // we'd get an immediate SQLITE_BUSY and — worse — used to fall through to
    // overwriting the index with an empty one (silent history wipe).
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
    )?;

    // Read-modify-write must be a single transaction so VS Code cannot sneak a
    // concurrent write between our read and our write (TOCTOU).
    let tx = conn.transaction()?;

    // Distinguish "no such row" from a real DB error. A real error must propagate;
    // only a missing row legitimately means "no index yet".
    let existing = match tx.query_row::<String, _, _>(
        "SELECT value FROM ItemTable WHERE key = 'chat.ChatSessionStore.index'",
        [],
        |row| row.get(0),
    ) {
        Ok(s) => Some(s),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e).context("reading chat session index from state.vscdb"),
    };

    // Never fall back to an empty index on corrupt JSON — that would overwrite
    // the user's entire chat history. Propagate instead.
    let mut index: Value = match existing {
        Some(s) => serde_json::from_str(&s)
            .context("state.vscdb chat index is corrupt — refusing to overwrite")?,
        None => json!({"version": 1, "entries": {}}),
    };

    let entries = index["entries"]
        .as_object_mut()
        .context("unexpected chat index shape in state.vscdb (entries is not an object)")?;

    let entry = json!({
        "sessionId": session_uuid,
        "title": session.title,
        "lastMessageDate": session.updated_ms,
        "hasPendingEdits": false,
        "isExternal": false,
        "isEmpty": false,
        "lastResponseState": 1,
        "permissionLevel": "default",
        "initialLocation": "panel",
        "timing": {
            "created": session.created_ms,
            "lastRequestStarted": session.updated_ms,
            "lastRequestEnded": session.updated_ms
        }
    });

    entries.insert(session_uuid.to_string(), entry);

    let updated = serde_json::to_string(&index)?;
    tx.execute(
        "INSERT OR REPLACE INTO ItemTable (key, value) VALUES ('chat.ChatSessionStore.index', ?1)",
        rusqlite::params![updated],
    )?;

    tx.commit()?;
    Ok(())
}
