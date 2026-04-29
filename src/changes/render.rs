//! Human and JSON renderings of a change list.

use std::fs;
use std::path::PathBuf;

use owo_colors::OwoColorize;
use serde::Serialize;
use similar::TextDiff;

use super::{ChangeKind, FileChange, OldContent};
use crate::workspace::Workspace;

#[derive(Debug, Serialize)]
pub struct DiffOutput {
    pub workspace: String,
    pub base: PathBuf,
    pub changes: Vec<FileChange>,
    pub signals: crate::review::signals::Signals,
}

pub enum DiffFormat {
    Default,
    Files,
    Stat,
}

pub fn format_diff(changes: &[FileChange], mode: &DiffFormat) -> String {
    match mode {
        DiffFormat::Default => {
            let mut lines = Vec::new();
            for c in changes {
                let icon = match c.kind {
                    ChangeKind::Added => "A".green().to_string(),
                    ChangeKind::Modified => "M".yellow().to_string(),
                    ChangeKind::Deleted => "D".red().to_string(),
                };
                let counts = match (&c.kind, c.additions, c.deletions) {
                    (ChangeKind::Added, Some(a), _) => {
                        format!(" {}", format!("(+{a})").green())
                    }
                    (ChangeKind::Modified, Some(a), Some(d)) => {
                        format!(" {} {}", format!("(+{a})").green(), format!("(-{d})").red())
                    }
                    (ChangeKind::Deleted, _, _) => String::new(),
                    _ => {
                        if c.is_binary {
                            " [binary]".to_string()
                        } else {
                            String::new()
                        }
                    }
                };
                lines.push(format!("{}  {}{}", icon, c.path.display(), counts));
            }
            lines.join("\n")
        }
        DiffFormat::Files => changes
            .iter()
            .map(|c| c.path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        DiffFormat::Stat => {
            let total = changes.len();
            let added = changes
                .iter()
                .filter(|c| matches!(c.kind, ChangeKind::Added))
                .count();
            let modified = changes
                .iter()
                .filter(|c| matches!(c.kind, ChangeKind::Modified))
                .count();
            let deleted = changes
                .iter()
                .filter(|c| matches!(c.kind, ChangeKind::Deleted))
                .count();
            format!("{total} files changed, {added} added, {modified} modified, {deleted} deleted")
        }
    }
}

fn colorize_unified_diff(plain: &str) -> String {
    let mut out = String::with_capacity(plain.len() + 128);
    for line in plain.lines() {
        if line.starts_with("---") || line.starts_with("+++") {
            out.push_str(&format!("{}\n", line.bold()));
        } else if line.starts_with("@@") {
            out.push_str(&format!("{}\n", line.cyan()));
        } else if line.starts_with('+') {
            out.push_str(&format!("{}\n", line.green()));
        } else if line.starts_with('-') {
            out.push_str(&format!("{}\n", line.red()));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

pub fn generate_unified_diff(workspace: &Workspace, change: &FileChange) -> String {
    let upper_path = workspace.upper_path().map_or_else(
        |_| workspace.view_path.join(&change.path),
        |upper| upper.join(&change.path),
    );
    let base_path = workspace.base.join(&change.path);

    let a_label = format!("a/{}", change.path.display());
    let b_label = format!("b/{}", change.path.display());

    if !matches!(change.kind, ChangeKind::Added) {
        match change.old_content {
            OldContent::BaseAdvanced => {
                return format!(
                    "diff unavailable (base advanced): {}\n",
                    change.path.display()
                );
            }
            OldContent::AncestorUnknown => {
                return format!(
                    "diff unavailable (ancestor unknown; pending manifest): {}\n",
                    change.path.display()
                );
            }
            OldContent::Available => {}
        }
    }

    match change.kind {
        ChangeKind::Added => {
            if change.is_binary {
                return format!("Binary file {} added\n", change.path.display());
            }
            let content = fs::read_to_string(&upper_path).unwrap_or_default();
            let mut out = String::new();
            out.push_str(&format!("{}\n", "--- /dev/null".bold()));
            out.push_str(&format!("{}\n", format!("+++ {b_label}").bold()));
            if !content.is_empty() {
                let lines: Vec<&str> = content.lines().collect();
                out.push_str(&format!(
                    "{}\n",
                    format!("@@ -0,0 +1,{} @@", lines.len()).cyan()
                ));
                for line in &lines {
                    out.push_str(&format!("{}\n", format!("+{line}").green()));
                }
            }
            out
        }
        ChangeKind::Deleted => {
            if change.is_binary {
                return format!("Binary file {} deleted\n", change.path.display());
            }
            let content = fs::read_to_string(&base_path).unwrap_or_default();
            let mut out = String::new();
            out.push_str(&format!("{}\n", format!("--- {a_label}").bold()));
            out.push_str(&format!("{}\n", "+++ /dev/null".bold()));
            if !content.is_empty() {
                let lines: Vec<&str> = content.lines().collect();
                out.push_str(&format!(
                    "{}\n",
                    format!("@@ -1,{} +0,0 @@", lines.len()).cyan()
                ));
                for line in &lines {
                    out.push_str(&format!("{}\n", format!("-{line}").red()));
                }
            }
            out
        }
        ChangeKind::Modified => {
            if change.is_binary {
                return format!("Binary file {} modified\n", change.path.display());
            }
            let old_content = fs::read_to_string(&base_path).unwrap_or_default();
            let new_content = fs::read_to_string(&upper_path).unwrap_or_default();

            let diff = TextDiff::from_lines(&old_content, &new_content);
            let udiff = diff
                .unified_diff()
                .context_radius(3)
                .header(&a_label, &b_label)
                .to_string();
            if !udiff.is_empty() {
                return colorize_unified_diff(&udiff);
            }
            format!(
                "{}\n{}\n",
                format!("--- {a_label}").bold(),
                format!("+++ {b_label}").bold()
            )
        }
    }
}
