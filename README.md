# session-sync

Migrate chat sessions between **Claude Code**, **VS Code Copilot Chat**, and **OpenCode** — in all six directions — without touching the source.

```
Claude Code  ←→  VS Code Copilot Chat
Claude Code  ←→  OpenCode
VS Code      ←→  OpenCode
```

---

## Requirements

- [Rust](https://rustup.rs) 1.70 or later (uses the 2021 edition; `cargo` is all you need)
- The source tool must be installed and have at least one session on disk

No other runtime dependencies — `rusqlite` is bundled and compiled in.

---

## Installation

```bash
git clone https://github.com/Supernova1744/session-sync
cd session-sync
cargo build --release
```

The binary is at `target/release/session-sync`. Copy it anywhere on your `$PATH`:

```bash
cp target/release/session-sync ~/.local/bin/
```

---

## Default storage paths

`session-sync` discovers each tool's sessions automatically:

| Tool | Default path |
|------|-------------|
| **Claude Code** | `~/.claude/projects/<encoded-path>/<uuid>.jsonl` |
| **OpenCode** | `~/.local/share/opencode/opencode.db` (Linux) · `~/Library/Application Support/opencode/opencode.db` (macOS) |
| **VS Code** | `~/.config/Code/User/workspaceStorage/` (Linux) · `~/Library/Application Support/Code/User/workspaceStorage/` (macOS) |

**WSL note:** VS Code sessions stored on the Windows side are also detected automatically at `/mnt/c/Users/<name>/AppData/Roaming/Code/User/workspaceStorage/`. Sessions written by `session-sync` use valid `file:///` URIs that VS Code can open.

---

## Usage

### List sessions

Show all sessions available in a tool, sorted by most recent first:

```bash
session-sync list --from claude
session-sync list --from opencode
session-sync list --from vscode
```

Example output:

```
Sessions in claude (12 total):

  ID:    f3bce858-af2f-47ac-b8b4-c04ce1d4b29a
  Title: Research tool session storage paths
  Dir:   /mnt/d/side-projects/session-sync
  When:  2026-06-17T17:21:46Z

  ID:    7ea9bcb0-fa19-4359-8973-27d213922d69
  Title: Implement project phases with testing and git commits
  Dir:   /mnt/d/side-projects/ctx
  When:  2026-06-15T11:42:08Z
```

Override the default storage path with `--dir`:

```bash
# Point at a specific Claude projects directory
session-sync list --from claude --dir /path/to/.claude/projects

# Point at a specific OpenCode database
session-sync list --from opencode --dir /path/to/opencode.db

# Point at a specific VS Code workspaceStorage directory
session-sync list --from vscode --dir /path/to/workspaceStorage
```

---

### Convert a session

Copy a session from one tool to another. The source is never modified.

```bash
session-sync convert --from <tool> --to <tool> [--session <id>] [--out-dir <path>]
```

**Interactive picker** (omit `--session`):

```bash
session-sync convert --from claude --to opencode
```

An interactive menu appears; arrow keys to select, Enter to confirm.

**Non-interactive** (pass the session ID):

```bash
session-sync convert --from claude --to opencode \
  --session 7ea9bcb0-fa19-4359-8973-27d213922d69
```

---

### All six directions — examples

#### Claude Code → OpenCode

```bash
session-sync convert --from claude --to opencode \
  --session 7ea9bcb0-fa19-4359-8973-27d213922d69
# Done! Written to: ses_cb14c70dd0eb4059a169becde8056ef5
```

The session appears in OpenCode under the same working directory.

#### Claude Code → VS Code

```bash
session-sync convert --from claude --to vscode \
  --session 7ea9bcb0-fa19-4359-8973-27d213922d69
# Done! Written to: .../workspaceStorage/imported-87134db9/chatSessions/cf6b349d....json
```

Restart VS Code and the session appears in the Copilot Chat history panel.

#### OpenCode → Claude Code

```bash
session-sync convert --from opencode --to claude \
  --session ses_23b422269ffePQUTD5luxhHHJ6
# Done! Written to: ~/.claude/projects/-mnt-c-Users-.../78b06917....jsonl
```

#### OpenCode → VS Code

```bash
session-sync convert --from opencode --to vscode \
  --session ses_23b422269ffePQUTD5luxhHHJ6
# Done! Written to: .../workspaceStorage/imported-73e03387/chatSessions/73f00807....json
```

#### VS Code → Claude Code

```bash
session-sync convert --from vscode --to claude \
  --session 55447895-a563-43ab-af57-117040e93fb0
# Done! Written to: ~/.claude/projects/unknown/28b09c0a....jsonl
```

#### VS Code → OpenCode

```bash
session-sync convert --from vscode --to opencode \
  --session b5e7ad01-def2-4937-bad8-a7a7e16f4784
# Done! Written to: ses_ff402b8f6ddc48ec9cd97594256d22b9
```

---

### Output location

By default each tool writes to its native location. Use `--out-dir` to redirect:

```bash
# Write to a specific directory (useful for testing or manual review)
session-sync convert --from claude --to vscode \
  --session <id> --out-dir /tmp/my-export

# For VS Code: out-dir is the workspaceStorage parent;
# session-sync creates an imported-<uuid> hash dir inside it
ls /tmp/my-export/
# imported-a1b2c3d4/
#   chatSessions/<uuid>.json
#   state.vscdb
#   workspace.json
```

---

## Loss reporting

Not every field in one tool has an equivalent in another. After each conversion, a summary of dropped data is printed to stderr:

```
⚠ Data lost in conversion:
  - Hook/attachment events (261 dropped)
  - File history snapshots (60 dropped)
  - Token usage counts (6 dropped)
```

| Loss kind | When it occurs |
|-----------|---------------|
| Hook/attachment events | Claude Code attachment and file-history-snapshot lines have no equivalent |
| File history snapshots | Same as above |
| Encrypted thinking blocks | VS Code stores thinking as opaque base64; dropped when writing to Claude or OpenCode |
| Tool call inputs | VS Code doesn't store structured tool inputs; target gets `null` input |
| Token usage counts | VS Code doesn't store per-request token counts |
| Cost data | Not all tools store USD cost |
| OpenCode todos | The `todo` table in OpenCode has no equivalent in other tools |
| Canceled requests | VS Code `isCanceled` turns are skipped |

Losses are informational — the conversion still succeeds and the readable content (messages, tool outputs, reasoning) is always preserved.

---

## How it works

```
Source reader  →  Canonical IR  →  Target writer
```

Each tool has a dedicated reader and writer. Between them sits a canonical intermediate representation (IR) that captures turns, messages, tool calls, thinking blocks, and step boundaries. This means adding a fourth tool only requires one new reader + one new writer, not six new converters.

### Claude Code

Sessions are JSONL files at `~/.claude/projects/<encoded-path>/<uuid>.jsonl`. Each line is a JSON object with `type: "user"` or `type: "assistant"`. Tool calls are stitched: `tool_use` blocks in assistant lines are paired with matching `tool_result` blocks in subsequent user lines by `id`.

### VS Code Copilot Chat

Sessions are JSON files at `workspaceStorage/<hash>/chatSessions/<uuid>.json`. The `state.vscdb` SQLite file in the same hash directory holds an index of all sessions for that workspace. `workspace.json` maps the hash to a folder URI.

### OpenCode

A single SQLite database at `~/.local/share/opencode/opencode.db` holds all sessions. Tables: `project`, `session`, `message`, `part`. Parts have types `text`, `reasoning`, `tool`, `step-start`, `step-finish`.

---

## Platform support

| Platform | Status |
|----------|--------|
| Linux (native) | Fully supported |
| macOS | Fully supported |
| WSL2 | Fully supported; Windows VS Code sessions detected via `/mnt/c/Users/` |
| Windows (native) | Not tested; path discovery uses Unix conventions |

---

## Reference

```
session-sync list --from <tool> [--dir <path>]
session-sync convert --from <tool> --to <tool> [--session <id>] [--out-dir <path>]

Tools: claude | vscode | opencode
```
