use crate::error::Result;
use crate::ir::Session;

pub mod claude;
pub mod opencode;
pub mod vscode;

pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_ms: i64,
    pub cwd: String,
}

pub trait Reader {
    fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
    fn read_session(&self, id: &str) -> Result<Session>;
}

/// Validate an opaque session identifier before it is used to build a filesystem
/// path. Accepts only filename-safe characters (1–128 of `[A-Za-z0-9_-]`) so that
/// `format!("{id}.jsonl")` followed by `Path::join` cannot escape the storage dir.
pub fn validate_session_id(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid {
        return Err(crate::error::anyhow!(
            "invalid session id (must be 1–128 of [A-Za-z0-9_-]): {id:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_session_id;

    #[test]
    fn accepts_valid_ids() {
        // Claude / VS Code UUIDs and OpenCode prefixed ids all pass.
        assert!(validate_session_id("f3bce858-af2f-47ac-b8b4-c04ce1d4b29a").is_ok());
        assert!(validate_session_id("ses_cb14c70dd0eb4059a169becde8056ef5").is_ok());
        assert!(validate_session_id("msg_abc-DEF_012").is_ok());
        assert!(validate_session_id(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn rejects_path_traversal_and_separators() {
        // The whole point: these must NOT reach Path::join.
        for bad in ["../../etc/passwd", "..", "a/b", "a\\b", "a\0b", "a:b"] {
            assert!(
                validate_session_id(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_empty_and_overlong() {
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id(&"a".repeat(129)).is_err());
    }
}
