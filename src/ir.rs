use serde_json::Value;

use crate::loss::LossReport;

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub source_id:  String,
    pub title:      String,
    pub cwd:        String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub turns:      Vec<Turn>,
    pub losses:     LossReport,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub user:      UserMessage,
    pub assistant: Option<AssistantMessage>,
}

#[derive(Debug, Clone)]
pub struct UserMessage {
    pub created_ms: i64,
    pub text:       String,
}

#[derive(Debug, Clone, Default)]
pub struct AssistantMessage {
    pub created_ms: i64,
    pub model:      Option<String>,
    pub provider:   Option<String>,
    pub tokens:     Option<TokenUsage>,
    pub cost_usd:   Option<f64>,
    pub parts:      Vec<AssistantPart>,
}

#[derive(Debug, Clone)]
pub enum AssistantPart {
    Text(String),
    Thinking(ThinkingBlock),
    ToolCall(ToolCall),
    /// Boundary between successive model generations within one Turn.
    /// Claude writes a separate assistant JSONL line per generation;
    /// OpenCode uses step-start/step-finish part rows.
    StepBreak,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub call_id:   String,
    pub tool_name: String,
    /// Structured input; Value::Null when not available (VS Code)
    pub input:     Value,
    pub output:    Option<String>,
    pub is_error:  bool,
}

#[derive(Debug, Clone)]
pub enum ThinkingBlock {
    Plaintext(String),
    /// VS Code encrypted base64 — preserved opaque for round-trip
    Opaque { id: String, value: String },
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input:       i64,
    pub output:      i64,
    pub reasoning:   i64,
    pub cache_read:  i64,
    pub cache_write: i64,
}

impl Session {
    pub fn total_tokens(&self) -> i64 {
        self.turns
            .iter()
            .flat_map(|t| t.assistant.as_ref())
            .filter_map(|a| a.tokens.as_ref())
            .map(|tok| tok.input + tok.output)
            .sum()
    }

    pub fn total_cost(&self) -> f64 {
        self.turns
            .iter()
            .flat_map(|t| t.assistant.as_ref())
            .filter_map(|a| a.cost_usd)
            .sum()
    }
}
