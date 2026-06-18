use std::path::PathBuf;

pub fn claude_projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

pub fn opencode_db_path() -> Option<PathBuf> {
    // ~/.local/share/opencode/opencode.db on Linux
    // ~/Library/Application Support/opencode/opencode.db on macOS
    dirs::data_local_dir().map(|d| d.join("opencode").join("opencode.db"))
}

/// Returns all VS Code workspaceStorage directories found on this system.
/// On WSL, also probes Windows AppData through /mnt/c.
pub fn vscode_workspace_storage_dirs() -> Vec<PathBuf> {
    let mut candidates = vec![];

    // Linux native / WSL native
    if let Some(h) = dirs::home_dir() {
        candidates.push(h.join(".config/Code/User/workspaceStorage"));
        // VS Code Insiders
        candidates.push(h.join(".config/Code - Insiders/User/workspaceStorage"));
    }

    // macOS
    if let Some(h) = dirs::home_dir() {
        candidates.push(
            h.join("Library/Application Support/Code/User/workspaceStorage")
        );
    }

    // Windows via WSL: /mnt/c/Users/<name>/AppData/Roaming/Code/User/workspaceStorage
    let mnt_c_users = PathBuf::from("/mnt/c/Users");
    if mnt_c_users.exists() {
        if let Ok(entries) = std::fs::read_dir(&mnt_c_users) {
            for entry in entries.flatten() {
                let p = entry
                    .path()
                    .join("AppData/Roaming/Code/User/workspaceStorage");
                if p.exists() {
                    candidates.push(p);
                }
            }
        }
    }

    candidates.into_iter().filter(|p| p.exists()).collect()
}

/// Decode a VS Code workspace.json folder URI to a local filesystem path.
/// Handles file:/// URIs, Windows drive paths, and WSL UNC paths.
pub fn decode_vscode_folder_uri(uri: &str) -> Option<PathBuf> {
    let without_scheme = uri.strip_prefix("file://")?;
    let decoded = urlencoding::decode(without_scheme).ok()?.into_owned();
    let path_str = decoded.trim_start_matches('/');

    // WSL UNC: file://wsl.localhost/<distro>/<linux-path>
    // decoded looks like "wsl.localhost/Ubuntu/home/ali/myname"
    if path_str.starts_with("wsl.localhost/") {
        let after_host = path_str.splitn(2, '/').nth(1)?; // "Ubuntu/home/ali/myname"
        let after_distro = after_host.splitn(2, '/').nth(1)?; // "home/ali/myname"
        return Some(PathBuf::from(format!("/{}", after_distro)));
    }

    // WSL remote: file://wsl+<distro>/<linux-path>
    // decoded looks like "wsl+Ubuntu/home/ali/myname"
    if path_str.starts_with("wsl+") || path_str.starts_with("wsl%2B") {
        let after_host = path_str.splitn(2, '/').nth(1)?; // "home/ali/myname"
        return Some(PathBuf::from(format!("/{}", after_host)));
    }

    // Windows path: "/c:/Users/..." or "c:/Users/..." → /mnt/<drive>/...
    if path_str.len() >= 2 {
        let mut chars = path_str.chars();
        let first = chars.next()?;
        let second = chars.next()?;
        if first.is_ascii_alphabetic() && second == ':' {
            let drive = first.to_ascii_lowercase();
            let rest = &path_str[2..];
            let unix_rest = rest.replace('\\', "/");
            return Some(PathBuf::from(format!("/mnt/{}{}", drive, unix_rest)));
        }
    }

    // Standard Unix path
    Some(PathBuf::from(decoded))
}

/// Return the WSL distro name if running under WSL, e.g. "Ubuntu".
pub fn wsl_distro_name() -> Option<String> {
    std::env::var("WSL_DISTRO_NAME").ok()
}

/// Encode a cwd path the way Claude Code does:
/// "/mnt/d/foo" → "-mnt-d-foo"
pub fn encode_claude_path(cwd: &str) -> String {
    cwd.replace('/', "-")
}
