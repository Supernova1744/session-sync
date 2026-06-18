use std::path::Path;

use crate::error::Result;
use crate::ir::Session;

pub mod claude;
pub mod opencode;
pub mod vscode;

pub trait Writer {
    fn write_session(&self, session: &Session, out_dir: Option<&Path>) -> Result<String>;
}
