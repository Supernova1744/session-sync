#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LossKind {
    HookAttachment,
    FileHistorySnapshot,
    EncryptedThinking,
    ToolInputUnavailable,
    TokenCounts,
    CostData,
    OpenCodeTodos,
    CanceledRequest,
    UnsupportedPartType,
}

impl LossKind {
    fn label(&self) -> &'static str {
        match self {
            LossKind::HookAttachment => "Hook/attachment events",
            LossKind::FileHistorySnapshot => "File history snapshots",
            LossKind::EncryptedThinking => "Encrypted thinking blocks",
            LossKind::ToolInputUnavailable => "Tool call inputs (not stored by source)",
            LossKind::TokenCounts => "Token usage counts",
            LossKind::CostData => "Cost data (USD)",
            LossKind::OpenCodeTodos => "OpenCode todo items",
            LossKind::CanceledRequest => "Canceled requests",
            LossKind::UnsupportedPartType => "Unsupported content part types",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LossReport {
    pub items: Vec<(LossKind, usize, Option<String>)>,
}

impl LossReport {
    pub fn add(&mut self, kind: LossKind, count: usize, note: Option<String>) {
        if count == 0 {
            return;
        }
        // Merge with existing entry of same kind
        for (k, c, _) in &mut self.items {
            if k == &kind {
                *c += count;
                return;
            }
        }
        self.items.push((kind, count, note));
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn count_of(&self, kind: &LossKind) -> usize {
        self.items
            .iter()
            .find(|(k, _, _)| k == kind)
            .map(|(_, c, _)| *c)
            .unwrap_or(0)
    }

    pub fn merge(&mut self, other: LossReport) {
        for (kind, count, note) in other.items {
            self.add(kind, count, note);
        }
    }

    pub fn print_summary(&self) {
        if self.is_empty() {
            return;
        }
        eprintln!("\n⚠ Data lost in conversion:");
        for (kind, count, note) in &self.items {
            let note_str = note.as_deref().unwrap_or("");
            if note_str.is_empty() {
                eprintln!("  - {} ({} dropped)", kind.label(), count);
            } else {
                eprintln!("  - {} ({} dropped) — {}", kind.label(), count, note_str);
            }
        }
        eprintln!();
    }
}
