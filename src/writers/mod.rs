use std::io::Write;
use std::path::Path;

use crate::error::Result;
use crate::ir::Session;

pub mod claude;
pub mod opencode;
pub mod vscode;

pub trait Writer {
    fn write_session(&self, session: &Session, out_dir: Option<&Path>) -> Result<String>;
}

/// Atomically write `data` to `path` via a temp file + `fsync` + rename.
///
/// `fs::write` truncates then writes, so an interrupt (SIGINT, power loss) leaves
/// a partial file in the user's live storage. This helper guarantees the target
/// file is either fully replaced or left untouched. The temp file is created in
/// the same directory as `path` (required for `rename` to be atomic, which only
/// holds within a single filesystem).
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        // Best-effort cleanup so a permanent failure (e.g. disk full) doesn't
        // leave a stale .tmp next to the user's live storage.
        let _ = std::fs::remove_file(&tmp);
    }
    res.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::atomic_write;

    #[test]
    fn atomic_write_creates_file_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().join("session.jsonl");

        atomic_write(&path, b"hello world").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        // exactly one entry: the final file (no leftover .tmp sidecar)
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_write_overwrites_existing_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().join("session.jsonl");
        std::fs::write(&path, b"old").unwrap();

        atomic_write(&path, b"new contents").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new contents");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
