mod cli;
mod discover;
mod error;
mod id;
mod ir;
mod loss;
mod readers;
mod writers;

#[cfg(test)]
mod e2e_tests;

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

use cli::{Cli, Command, Tool};
use discover::{claude_projects_dir, opencode_db_path, vscode_workspace_storage_dirs};
use error::{anyhow, Result};
use id::ms_to_iso;
use readers::claude::ClaudeReader;
use readers::opencode::OpenCodeReader;
use readers::vscode::VsCodeReader;
use readers::{Reader, SessionSummary};
use writers::claude::ClaudeWriter;
use writers::opencode::OpenCodeWriter;
use writers::vscode::VsCodeWriter;
use writers::Writer;

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List { from, dir, all } => cmd_list(&from, dir, all),
        Command::Convert {
            from,
            to,
            session,
            out_dir,
        } => cmd_convert(&from, &to, session, out_dir),
    }
}

fn cmd_list(tool: &Tool, dir: Option<PathBuf>, all: bool) -> Result<()> {
    let reader = make_reader(tool, dir.as_deref())?;
    let all_sessions = reader.list_sessions()?;

    let (sessions, cwd_filter): (Vec<SessionSummary>, Option<String>) = if all {
        (all_sessions, None)
    } else {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(normalize_path))
            .unwrap_or_default();
        let filtered: Vec<_> = all_sessions
            .into_iter()
            .filter(|s| normalize_path(&s.cwd) == cwd)
            .collect();
        (filtered, Some(cwd))
    };

    if sessions.is_empty() {
        if let Some(ref cwd) = cwd_filter {
            println!("No {} sessions found for {}.", tool, cwd);
            println!("Run with --all to list sessions from all directories.");
        } else {
            println!("No sessions found for {}.", tool);
        }
        return Ok(());
    }

    let header = if let Some(ref cwd) = cwd_filter {
        format!(
            "Sessions in {} for {} ({} total):",
            tool,
            cwd,
            sessions.len()
        )
    } else {
        format!("Sessions in {} ({} total):", tool, sessions.len())
    };
    println!("{}\n", header);

    for s in &sessions {
        let ts = if s.updated_ms > 0 {
            ms_to_iso(s.updated_ms)
        } else {
            "unknown time".to_string()
        };
        let cwd = if s.cwd.is_empty() {
            "(no path)"
        } else {
            &s.cwd
        };
        println!("  ID:    {}", s.id);
        println!("  Title: {}", s.title);
        println!("  Dir:   {}", cwd);
        println!("  When:  {}", ts);
        println!();
    }
    Ok(())
}

/// Normalise a path string for comparison: trim trailing slashes.
fn normalize_path(p: &str) -> String {
    p.trim_end_matches('/').to_string()
}

fn cmd_convert(
    from: &Tool,
    to: &Tool,
    session_id: Option<String>,
    out_dir: Option<PathBuf>,
) -> Result<()> {
    if from == to {
        return Err(anyhow!("Source and target tools must be different"));
    }

    let reader = make_reader(from, None)?;
    let sessions = reader.list_sessions()?;

    if sessions.is_empty() {
        return Err(anyhow!("No sessions found in {}", from));
    }

    let id = if let Some(id) = session_id {
        id
    } else {
        interactive_pick(&sessions)?
    };

    println!("Reading session {} from {}…", id, from);
    let session = reader.read_session(&id)?;

    println!(
        "  Title: {}\n  Turns: {}\n  CWD:   {}",
        session.title,
        session.turns.len(),
        session.cwd
    );

    if !session.losses.is_empty() {
        session.losses.print_summary();
    }

    println!("\nWriting to {}…", to);
    let writer = make_writer(to)?;
    let output_location = writer.write_session(&session, out_dir.as_deref())?;

    println!("Done! Written to: {}", output_location);
    Ok(())
}

fn make_reader(tool: &Tool, dir: Option<&std::path::Path>) -> Result<Box<dyn Reader>> {
    match tool {
        Tool::Claude => {
            let projects_dir = dir
                .map(PathBuf::from)
                .or_else(claude_projects_dir)
                .context("Cannot locate Claude projects directory (~/.claude/projects)")?;
            Ok(Box::new(ClaudeReader::new(projects_dir)))
        }
        Tool::Opencode => {
            let db_path = dir
                .map(|d| d.join("opencode.db"))
                .or_else(opencode_db_path)
                .context("Cannot locate OpenCode database (~/.local/share/opencode/opencode.db)")?;
            Ok(Box::new(OpenCodeReader::new(db_path)))
        }
        Tool::Vscode => {
            let dirs = if let Some(d) = dir {
                vec![d.to_path_buf()]
            } else {
                vscode_workspace_storage_dirs()
            };
            if dirs.is_empty() {
                return Err(anyhow!("No VS Code workspaceStorage directories found"));
            }
            Ok(Box::new(VsCodeReader::new(dirs)))
        }
    }
}

fn make_writer(tool: &Tool) -> Result<Box<dyn Writer>> {
    match tool {
        Tool::Claude => {
            let projects_dir =
                claude_projects_dir().context("Cannot locate Claude projects directory")?;
            Ok(Box::new(ClaudeWriter::new(projects_dir)))
        }
        Tool::Opencode => {
            let db_path = opencode_db_path().context("Cannot locate OpenCode database")?;
            Ok(Box::new(OpenCodeWriter::new(db_path)))
        }
        Tool::Vscode => {
            let dirs = vscode_workspace_storage_dirs();
            Ok(Box::new(VsCodeWriter::new(dirs)))
        }
    }
}

fn interactive_pick(sessions: &[SessionSummary]) -> Result<String> {
    let options: Vec<String> = sessions
        .iter()
        .map(|s| {
            let short_id = if s.id.len() > 12 { &s.id[..12] } else { &s.id };
            let cwd = if s.cwd.is_empty() { "?" } else { &s.cwd };
            format!("[{}…] {}", short_id, s.title)
        })
        .collect();

    let selection = inquire::Select::new("Select a session to convert:", options)
        .prompt()
        .map_err(|e| anyhow!("Selection canceled: {}", e))?;

    // Find the index of the selected option
    let idx = sessions
        .iter()
        .enumerate()
        .find(|(i, s)| {
            let short_id = if s.id.len() > 12 { &s.id[..12] } else { &s.id };
            selection.starts_with(&format!("[{}…]", short_id))
                || selection.starts_with(&format!("[{}]", short_id))
        })
        .map(|(i, _)| i)
        .unwrap_or(0);

    Ok(sessions[idx].id.clone())
}
