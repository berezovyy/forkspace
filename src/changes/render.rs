//! Human and JSON renderings of a change list.

use std::fs;
use std::path::PathBuf;

use owo_colors::OwoColorize;
use serde::Serialize;
use similar::TextDiff;

use super::{ChangeKind, FileChange, OldContent};
use crate::workspace::Workspace;

/// The `forkspace diff --json` document.
#[derive(Debug, Serialize)]
pub struct DiffOutput {
    pub workspace: String,
    pub base: PathBuf,
    pub changes: Vec<FileChange>,
    pub signals: crate::review::signals::Signals,
}

/// Summary layouts for [`format_diff`].
pub enum DiffFormat {
    /// One `A`/`M`/`D` line per change, with line counts.
    Default,
    /// Bare paths.
    Files,
    /// A single totals line.
    Stat,
}

pub fn format_diff(changes: &[FileChange], mode: &DiffFormat) -> String {
    match mode {
        DiffFormat::Default => changes
            .iter()
            .map(status_line)
            .collect::<Vec<_>>()
            .join("\n"),
        DiffFormat::Files => changes
            .iter()
            .map(|change| change.path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        DiffFormat::Stat => {
            let count = |kind| changes.iter().filter(|change| change.kind == kind).count();
            format!(
                "{} files changed, {} added, {} modified, {} deleted",
                changes.len(),
                count(ChangeKind::Added),
                count(ChangeKind::Modified),
                count(ChangeKind::Deleted),
            )
        }
    }
}

fn status_line(change: &FileChange) -> String {
    let icon = match change.kind {
        ChangeKind::Added => "A".green().to_string(),
        ChangeKind::Modified => "M".yellow().to_string(),
        ChangeKind::Deleted => "D".red().to_string(),
    };
    let detail = match (change.kind, change.additions, change.deletions) {
        (ChangeKind::Added, Some(additions), _) => {
            format!(" {}", format!("(+{additions})").green())
        }
        (ChangeKind::Modified, Some(additions), Some(deletions)) => format!(
            " {} {}",
            format!("(+{additions})").green(),
            format!("(-{deletions})").red()
        ),
        (ChangeKind::Deleted, _, _) => String::new(),
        _ if change.is_binary => " [binary]".to_string(),
        _ => String::new(),
    };
    format!("{icon}  {}{detail}", change.path.display())
}

/// A colored unified diff of one change against its base, or a one-line note when the
/// content is binary or the old side is unavailable.
pub fn generate_unified_diff(workspace: &Workspace, change: &FileChange) -> String {
    // Overlay workspaces keep new content in the upper dir; snapshots in the view itself.
    let new_path = workspace.upper_path().map_or_else(
        |_| workspace.view_path.join(&change.path),
        |upper| upper.join(&change.path),
    );
    let old_path = workspace.ancestor_dir().join(&change.path);
    let display = change.path.display();

    if change.kind != ChangeKind::Added {
        if let Some(reason) = unavailable_reason(change.old_content) {
            return format!("diff unavailable ({reason}): {display}\n");
        }
    }
    if change.is_binary {
        let verb = match change.kind {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
        };
        return format!("Binary file {display} {verb}\n");
    }

    let old_label = format!("a/{display}");
    let new_label = format!("b/{display}");
    match change.kind {
        ChangeKind::Added => {
            let content = fs::read_to_string(&new_path).unwrap_or_default();
            one_sided_diff(
                "--- /dev/null",
                &format!("+++ {new_label}"),
                &content,
                Side::New,
            )
        }
        ChangeKind::Deleted => {
            let content = fs::read_to_string(&old_path).unwrap_or_default();
            one_sided_diff(
                &format!("--- {old_label}"),
                "+++ /dev/null",
                &content,
                Side::Old,
            )
        }
        ChangeKind::Modified => {
            let old_content = fs::read_to_string(&old_path).unwrap_or_default();
            let new_content = fs::read_to_string(&new_path).unwrap_or_default();
            let diff = TextDiff::from_lines(&old_content, &new_content)
                .unified_diff()
                .context_radius(3)
                .header(&old_label, &new_label)
                .to_string();
            if diff.is_empty() {
                // No line differences (e.g. a mode-only change): show just the header.
                return format!(
                    "{}\n{}\n",
                    format!("--- {old_label}").bold(),
                    format!("+++ {new_label}").bold()
                );
            }
            colorize_unified_diff(&diff)
        }
    }
}

fn unavailable_reason(old_content: OldContent) -> Option<&'static str> {
    match old_content {
        OldContent::Available => None,
        OldContent::BaseAdvanced => Some("base advanced"),
        OldContent::AncestorUnknown => Some("ancestor unknown; pending manifest"),
    }
}

/// Which side of a one-sided diff holds the content.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

/// Diff of an added or deleted file: one hunk in which every line is inserted or removed.
fn one_sided_diff(old_header: &str, new_header: &str, content: &str, side: Side) -> String {
    let mut out = format!("{}\n{}\n", old_header.bold(), new_header.bold());
    if content.is_empty() {
        return out;
    }
    let lines: Vec<&str> = content.lines().collect();
    let count = lines.len();
    let hunk = match side {
        Side::New => format!("@@ -0,0 +1,{count} @@"),
        Side::Old => format!("@@ -1,{count} +0,0 @@"),
    };
    out.push_str(&format!("{}\n", hunk.cyan()));
    out.extend(lines.iter().map(|line| match side {
        Side::New => format!("{}\n", format!("+{line}").green()),
        Side::Old => format!("{}\n", format!("-{line}").red()),
    }));
    out
}

fn colorize_unified_diff(plain: &str) -> String {
    plain
        .lines()
        .map(|line| format!("{}\n", colorize_line(line)))
        .collect()
}

fn colorize_line(line: &str) -> String {
    if line.starts_with("---") || line.starts_with("+++") {
        line.bold().to_string()
    } else if line.starts_with("@@") {
        line.cyan().to_string()
    } else if line.starts_with('+') {
        line.green().to_string()
    } else if line.starts_with('-') {
        line.red().to_string()
    } else {
        line.to_string()
    }
}
