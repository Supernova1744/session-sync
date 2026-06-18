//! Round-trip integration tests: Session IR -> Writer -> Reader -> Session IR.
//!
//! These exercise each (writer, reader) pair against its own on-disk format so a
//! known session survives a write+read with its core content intact. Together
//! with the live CLI conversions (claude/opencode/vscode sources with real data),
//! this covers the machinery of all six migration directions, since every
//! direction is just `<source>.reader + <target>.writer` operating on the shared
//! IR.

use serde_json::json;
use tempfile::TempDir;

use crate::ir::{
    AssistantMessage, AssistantPart, Session, ThinkingBlock, TokenUsage, ToolCall, Turn,
    UserMessage,
};
use crate::readers::claude::ClaudeReader;
use crate::readers::opencode::OpenCodeReader;
use crate::readers::vscode::VsCodeReader;
use crate::readers::Reader;
use crate::writers::claude::ClaudeWriter;
use crate::writers::opencode::OpenCodeWriter;
use crate::writers::vscode::VsCodeWriter;
use crate::writers::Writer;

/// Build a representative session: 2 turns, user text, assistant text, a tool
/// call WITH a result, plaintext thinking, a step break, tokens, and a model.
fn sample_session() -> Session {
    Session {
        source_id: "test-source".to_string(),
        title: "Round-trip fixture".to_string(),
        cwd: "/tmp/example".to_string(),
        created_ms: 1_700_000_000_000,
        updated_ms: 1_700_000_001_000,
        losses: Default::default(),
        turns: vec![
            Turn {
                user: UserMessage {
                    created_ms: 1_700_000_000_000,
                    text: "Please read README.md and explain it.".to_string(),
                },
                assistant: Some(AssistantMessage {
                    created_ms: 1_700_000_000_500,
                    model: Some("claude-test".to_string()),
                    provider: Some("anthropic".to_string()),
                    tokens: Some(TokenUsage {
                        input: 120,
                        output: 80,
                        reasoning: 0,
                        cache_read: 0,
                        cache_write: 0,
                    }),
                    cost_usd: Some(0.002),
                    parts: vec![
                        AssistantPart::Text("Let me check the file.".to_string()),
                        AssistantPart::ToolCall(ToolCall {
                            call_id: "call_abc123".to_string(),
                            tool_name: "read".to_string(),
                            input: json!({ "filePath": "/tmp/example/README.md" }),
                            output: Some("# Example\n\nA project.".to_string()),
                            is_error: false,
                        }),
                    ],
                }),
            },
            Turn {
                user: UserMessage {
                    created_ms: 1_700_000_001_000,
                    text: "Now summarize it in one line.".to_string(),
                },
                assistant: Some(AssistantMessage {
                    created_ms: 1_700_000_001_500,
                    model: Some("claude-test".to_string()),
                    provider: None,
                    tokens: None,
                    cost_usd: None,
                    parts: vec![
                        AssistantPart::Thinking(ThinkingBlock::Plaintext(
                            "Thinking about the summary.".to_string(),
                        )),
                        AssistantPart::Text("It is an example project.".to_string()),
                        AssistantPart::StepBreak,
                        AssistantPart::Text("With a second step.".to_string()),
                    ],
                }),
            },
        ],
    }
}

/// Invariants that survive all three formats: structure, user prompts, assistant
/// text, and the tool call's name + output (the readable content the tool
/// guarantees to preserve).
fn assert_common(s: &Session, label: &str) {
    assert_eq!(s.turns.len(), 2, "{label}: turn count preserved");
    assert_eq!(
        s.turns[0].user.text, "Please read README.md and explain it.",
        "{label}: first user prompt preserved",
    );
    assert_eq!(
        s.turns[1].user.text, "Now summarize it in one line.",
        "{label}: second user prompt preserved",
    );

    let asst1 = s.turns[0]
        .assistant
        .as_ref()
        .unwrap_or_else(|| panic!("{label}: turn 1 assistant preserved"));
    let tools: Vec<&ToolCall> = asst1
        .parts
        .iter()
        .filter_map(|p| match p {
            AssistantPart::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 1, "{label}: one tool call preserved");
    assert_eq!(tools[0].tool_name, "read", "{label}: tool name preserved");
    assert_eq!(
        tools[0].output.as_deref(),
        Some("# Example\n\nA project."),
        "{label}: tool output preserved",
    );
    assert!(!tools[0].is_error, "{label}: tool error flag preserved");
    assert!(
        asst1
            .parts
            .iter()
            .any(|p| matches!(p, AssistantPart::Text(t) if t == "Let me check the file.")),
        "{label}: assistant text preserved",
    );
    // NOTE: model is intentionally not asserted here — the VS Code writer always
    // emits modelId "copilot/auto" (Copilot Chat doesn't carry the source model).
}

#[test]
fn claude_round_trip() {
    let dir = TempDir::new().unwrap();

    let writer = ClaudeWriter::new(dir.path().to_path_buf());
    let out_path = writer
        .write_session(&sample_session(), Some(dir.path()))
        .expect("claude write");
    let id = std::path::Path::new(&out_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("claude output has a uuid stem");

    // Read and validate raw JSONL lines to ensure newly added metadata fields are present
    let raw_content = std::fs::read_to_string(&out_path).expect("read raw session file");
    let lines: Vec<serde_json::Value> = raw_content
        .lines()
        .map(|l| serde_json::from_str(l).expect("parse line as json"))
        .collect();

    assert!(!lines.is_empty(), "claude session should not be empty");

    let mut last_prompt_found = false;
    let mut user_turn_prompt_ids = Vec::new();

    for line in &lines {
        let t = line["type"].as_str().unwrap_or("");
        if t == "last-prompt" {
            last_prompt_found = true;
            assert_eq!(line["lastPrompt"], serde_json::json!("Now summarize it in one line."));
            assert!(!line["leafUuid"].as_str().unwrap_or("").is_empty());
        } else {
            assert_eq!(line["version"].as_str(), Some("2.1.143"));
            assert_eq!(line["gitBranch"].as_str(), Some("main"));

            if t == "user" {
                let prompt_id = line["promptId"].as_str().expect("user message has promptId");
                assert!(!prompt_id.is_empty());
                user_turn_prompt_ids.push(prompt_id.to_string());
            } else if t == "assistant" {
                let msg_id = line["message"]["id"].as_str().expect("assistant message has message.id");
                assert!(!msg_id.is_empty());
            }
        }
    }

    assert!(last_prompt_found, "last-prompt entry must be present");
    // There are 2 turns. The first turn has:
    // 1 user message, 1 assistant message, and 1 tool result user message.
    // The second turn has:
    // 1 user message, 1 assistant message.
    // Total user messages = 3 (2 user prompts + 1 tool result).
    // Prompt IDs for user prompt 1 and tool result 1 must be identical.
    assert_eq!(user_turn_prompt_ids.len(), 3);
    assert_eq!(user_turn_prompt_ids[0], user_turn_prompt_ids[1], "tool result must share promptId with user prompt in same turn");
    assert_ne!(user_turn_prompt_ids[0], user_turn_prompt_ids[2], "different turns must have different promptIds");

    let reader = ClaudeReader::new(dir.path().to_path_buf());
    let back = reader.read_session(id).expect("claude read-back");

    assert_common(&back, "claude");
    // Claude preserves tool input and plaintext thinking.
    assert_eq!(
        back.turns[0].assistant.as_ref().unwrap().model.as_deref(),
        Some("claude-test")
    );
    let asst1 = back.turns[0].assistant.as_ref().unwrap();
    let tool = asst1
        .parts
        .iter()
        .find_map(|p| {
            if let AssistantPart::ToolCall(tc) = p {
                Some(tc)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        tool.input["filePath"], "/tmp/example/README.md",
        "claude: tool input preserved"
    );
    let t2 = back.turns[1].assistant.as_ref().unwrap();
    let has_thinking = t2
        .parts
        .iter()
        .any(|p| matches!(p, AssistantPart::Thinking(ThinkingBlock::Plaintext(_))));
    assert!(has_thinking, "claude: plaintext thinking preserved");
}

#[test]
fn opencode_round_trip() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("opencode.db");

    // The writer assumes the OpenCode schema already exists (it never CREATEs),
    // so bootstrap it from the documented DDL before writing.
    bootstrap_opencode_schema(&db_path);

    let writer = OpenCodeWriter::new(db_path.clone());
    let ses_id = writer
        .write_session(&sample_session(), Some(dir.path()))
        .expect("opencode write");

    let reader = OpenCodeReader::new(db_path);
    let back = reader.read_session(&ses_id).expect("opencode read-back");

    assert_common(&back, "opencode");
    // OpenCode preserves tool input and plaintext thinking.
    assert_eq!(
        back.turns[0].assistant.as_ref().unwrap().model.as_deref(),
        Some("claude-test")
    );
    let asst1 = back.turns[0].assistant.as_ref().unwrap();
    let tool = asst1
        .parts
        .iter()
        .find_map(|p| {
            if let AssistantPart::ToolCall(tc) = p {
                Some(tc)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        tool.input["filePath"], "/tmp/example/README.md",
        "opencode: tool input preserved"
    );
    let t2 = back.turns[1].assistant.as_ref().unwrap();
    let has_thinking = t2
        .parts
        .iter()
        .any(|p| matches!(p, AssistantPart::Thinking(ThinkingBlock::Plaintext(_))));
    assert!(has_thinking, "opencode: plaintext thinking preserved");
}

#[test]
fn vscode_round_trip() {
    let dir = TempDir::new().unwrap();

    let writer = VsCodeWriter::new(vec![dir.path().to_path_buf()]);
    let out_path = writer
        .write_session(&sample_session(), Some(dir.path()))
        .expect("vscode write");
    let id = std::path::Path::new(&out_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("vscode output has a uuid stem");

    let reader = VsCodeReader::new(vec![dir.path().to_path_buf()]);
    let back = reader.read_session(id).expect("vscode read-back");

    assert_common(&back, "vscode");
    // VS Code round-trips: the index was updated and the session lists.
    let listed = reader.list_sessions().expect("vscode list");
    assert!(
        listed.iter().any(|s| s.id == id),
        "vscode: session appears in index"
    );
}

/// Create the OpenCode tables the reader/writer touch (DDL from SESSION_FORMATS.md).
fn bootstrap_opencode_schema(db_path: &std::path::Path) {
    use rusqlite::Connection;
    let conn = Connection::open(db_path).expect("open test opencode db");
    conn.execute_batch(
        "CREATE TABLE project (
            id TEXT PRIMARY KEY, worktree TEXT NOT NULL, vcs TEXT, name TEXT,
            icon_url TEXT, icon_color TEXT, time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL, time_initialized INTEGER,
            sandboxes TEXT NOT NULL, commands TEXT, icon_url_override TEXT
         );
         CREATE TABLE session (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
            parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
            title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT,
            summary_additions INTEGER, summary_deletions INTEGER,
            summary_files INTEGER, summary_diffs TEXT, revert TEXT,
            permission TEXT, time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL, time_compacting INTEGER,
            time_archived INTEGER, workspace_id TEXT, path TEXT, agent TEXT,
            model TEXT, cost REAL DEFAULT 0 NOT NULL,
            tokens_input INTEGER DEFAULT 0 NOT NULL,
            tokens_output INTEGER DEFAULT 0 NOT NULL,
            tokens_reasoning INTEGER DEFAULT 0 NOT NULL,
            tokens_cache_read INTEGER DEFAULT 0 NOT NULL,
            tokens_cache_write INTEGER DEFAULT 0 NOT NULL, metadata TEXT
         );
         CREATE TABLE message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
         );
         CREATE TABLE part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
            session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL, data TEXT NOT NULL
         );
         CREATE TABLE session_message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
            type TEXT NOT NULL, time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL, data TEXT NOT NULL
         );
         CREATE TABLE todo (
            session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
            content TEXT NOT NULL, status TEXT NOT NULL, priority TEXT NOT NULL,
            position INTEGER NOT NULL, time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL, PRIMARY KEY (session_id, position)
         );",
    )
    .expect("create opencode schema");
}
