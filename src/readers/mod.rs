use crate::error::Result;
use crate::ir::Session;

pub mod claude;
pub mod opencode;
pub mod vscode;

pub struct SessionSummary {
    pub id:         String,
    pub title:      String,
    pub updated_ms: i64,
    pub cwd:        String,
}

pub trait Reader {
    fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
    fn read_session(&self, id: &str) -> Result<Session>;
}
