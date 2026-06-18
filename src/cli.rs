use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "session-sync",
    version,
    about = "Migrate AI coding-assistant sessions between Claude Code, VS Code, and OpenCode"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List sessions available in a tool
    List {
        #[arg(long, value_enum)]
        from: Tool,
        /// Override the source tool's storage location. Per --from: claude
        /// expects the ~/.claude/projects dir; opencode expects a dir containing
        /// opencode.db; vscode expects the workspaceStorage dir.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Show sessions from all directories instead of only the current working directory
        #[arg(long)]
        all: bool,
    },
    /// Convert (copy) a session from one tool to another
    Convert {
        #[arg(long, value_enum)]
        from: Tool,
        #[arg(long, value_enum)]
        to: Tool,
        /// Session ID to convert; omit for interactive picker
        #[arg(long)]
        session: Option<String>,
        /// Override the target tool's output location (defaults to its native
        /// storage). Per --to: claude expects the ~/.claude/projects dir; opencode
        /// expects a dir containing opencode.db (or the opencode.db file path);
        /// vscode expects the workspaceStorage parent (an imported-<uuid> dir is
        /// created inside it).
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
}

#[derive(ValueEnum, Clone, Debug, PartialEq)]
pub enum Tool {
    Claude,
    Vscode,
    Opencode,
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tool::Claude => write!(f, "claude"),
            Tool::Vscode => write!(f, "vscode"),
            Tool::Opencode => write!(f, "opencode"),
        }
    }
}
