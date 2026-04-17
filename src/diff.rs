use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use owo_colors::OwoColorize;
use serde::Serialize;
use similar::{ChangeTag, TextDiff};
use walkdir::WalkDir;

use crate::error::{ForkspaceError, Result};
use crate::filter::{ChangeClass, ChangeFilter};
use crate::manifest::{self, ManifestKind, ManifestRow, ManifestStatus};
use crate::overlay::Whiteout;
use crate::state::State;
use crate::util::WalkDirExt;
use crate::workspace::Workspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub class: ChangeClass,
    pub additions: Option<usize>,
    pub deletions: Option<usize>,
    pub is_binary: bool,
    #[serde(skip)]
    pub old_content: OldContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OldContent {
    Available,
    BaseAdvanced,
    AncestorUnknown,
}

impl FileChange {
    pub(crate) fn deletion(path: PathBuf) -> Self {
        Self {
            class: ChangeFilter::class(&path),
            path,
            kind: ChangeKind::Deleted,
            additions: None,
            deletions: None,
            is_binary: false,
            old_content: OldContent::Available,
        }
    }
}

fn whiteout_target_path(rel: &Path, original_name: &str) -> PathBuf {
    if let Some(parent) = rel.parent() {
        parent.join(original_name)
    } else {
        PathBuf::from(original_name)
    }
}

fn valid_whiteout(rel: &Path, file_name: &str, base: &Path) -> Option<Whiteout> {
    match Whiteout::parse(file_name)? {
        Whiteout::Opaque => Some(Whiteout::Opaque),
        Whiteout::Deletion(original_name) => {
            let target = whiteout_target_path(rel, &original_name);
            fs::symlink_metadata(base.join(target))
                .is_ok()
                .then_some(Whiteout::Deletion(original_name))
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DiffOutput {
    pub workspace: String,
    pub base: PathBuf,
    pub changes: Vec<FileChange>,
}

pub enum DiffFormat {
    Default,
    Files,
    Stat,
}

pub fn is_binary(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 8192];
    let Ok(n) = file.read(&mut buf) else {
        return false;
    };
    buf[..n].contains(&0)
}

fn compute_line_diff(old: &Path, new: &Path) -> (usize, usize) {
    let old_content = fs::read_to_string(old).unwrap_or_default();
    let new_content = fs::read_to_string(new).unwrap_or_default();

    let diff = TextDiff::from_lines(&old_content, &new_content);
    let mut additions = 0usize;
    let mut deletions = 0usize;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }

    (additions, deletions)
}

fn files_differ(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let (Ok(meta_a), Ok(meta_b)) = (fs::symlink_metadata(a), fs::symlink_metadata(b)) else {
        return true;
    };
    if meta_a.file_type().is_symlink() || meta_b.file_type().is_symlink() {
        return meta_a.file_type().is_symlink() != meta_b.file_type().is_symlink()
            || fs::read_link(a).ok() != fs::read_link(b).ok();
    }
    if meta_a.is_dir() != meta_b.is_dir()
        || meta_a.len() != meta_b.len()
        || meta_a.mode() & 0o7777 != meta_b.mode() & 0o7777
    {
        return true;
    }

    let (Ok(mut fa), Ok(mut fb)) = (fs::File::open(a), fs::File::open(b)) else {
        return true;
    };
    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];
    loop {
        let na = match fa.read(&mut buf_a) {
            Ok(n) => n,
            Err(_) => return true,
        };
        let nb = match fb.read(&mut buf_b) {
            Ok(n) => n,
            Err(_) => return true,
        };
        if na != nb || buf_a[..na] != buf_b[..nb] {
            return true;
        }
        if na == 0 {
            return false;
        }
    }
}

fn classify_file(
    rel_path: PathBuf,
    upper_path: &Path,
    base_path: &Path,
    need_line_counts: bool,
) -> Option<FileChange> {
    if fs::symlink_metadata(base_path).is_ok() {
        if need_line_counts {
            let symlink = upper_path.is_symlink() || base_path.is_symlink();
            let binary = !symlink && is_binary(upper_path);
            let (additions, deletions) = if binary || symlink {
                if !files_differ(base_path, upper_path) {
                    return None;
                }
                (None, None)
            } else {
                let (a, d) = compute_line_diff(base_path, upper_path);
                if a == 0 && d == 0 && !files_differ(base_path, upper_path) {
                    return None;
                }
                (Some(a), Some(d))
            };
            Some(FileChange {
                class: ChangeFilter::class(&rel_path),
                path: rel_path,
                kind: ChangeKind::Modified,
                additions,
                deletions,
                is_binary: binary,
                old_content: OldContent::Available,
            })
        } else {
            if !files_differ(base_path, upper_path) {
                return None;
            }
            Some(FileChange {
                class: ChangeFilter::class(&rel_path),
                path: rel_path,
                kind: ChangeKind::Modified,
                additions: None,
                deletions: None,
                is_binary: false,
                old_content: OldContent::Available,
            })
        }
    } else if need_line_counts {
        let binary = !upper_path.is_symlink() && is_binary(upper_path);
        let additions = if binary || upper_path.is_symlink() {
            None
        } else {
            Some(
                fs::read_to_string(upper_path)
                    .unwrap_or_default()
                    .lines()
                    .count(),
            )
        };
        Some(FileChange {
            class: ChangeFilter::class(&rel_path),
            path: rel_path,
            kind: ChangeKind::Added,
            additions,
            deletions: None,
            is_binary: binary,
            old_content: OldContent::Available,
        })
    } else {
        Some(FileChange {
            class: ChangeFilter::class(&rel_path),
            path: rel_path,
            kind: ChangeKind::Added,
            additions: None,
            deletions: None,
            is_binary: false,
            old_content: OldContent::Available,
        })
    }
}

pub fn collect_changes(workspace: &Workspace) -> Result<Vec<FileChange>> {
    collect_changes_inner(workspace, true)
}

pub fn collect_changes_fast(workspace: &Workspace) -> Result<Vec<FileChange>> {
    collect_changes_inner(workspace, false)
}

fn collect_changes_inner(workspace: &Workspace, need_line_counts: bool) -> Result<Vec<FileChange>> {
    let upper = workspace.upper_path()?;
    if !upper.exists() {
        return Ok(Vec::new());
    }

    let mut changes = Vec::new();

    for entry in WalkDir::new(upper).min_depth(1) {
        let entry = entry.walk_context(upper)?;

        let rel = entry
            .path()
            .strip_prefix(upper)
            .map_err(|_| ForkspaceError::Io {
                context: format!(
                    "entry {} is not under upper dir {}",
                    entry.path().display(),
                    upper.display()
                ),
                source: std::io::Error::other("prefix mismatch"),
            })?;

        let file_name = match entry.file_name().to_str() {
            Some(n) => n,
            None => continue,
        };

        if entry.metadata().is_ok_and(|metadata| metadata.len() == 0) {
            if let Some(whiteout) = valid_whiteout(rel, file_name, &workspace.base) {
                match whiteout {
                    Whiteout::Opaque => continue,
                    Whiteout::Deletion(original_name) => {
                        changes.push(FileChange::deletion(whiteout_target_path(
                            rel,
                            &original_name,
                        )));
                        continue;
                    }
                }
            }
        }

        if entry.file_type().is_dir() {
            continue;
        }

        let rel_path = rel.to_path_buf();
        let base_path = workspace.base.join(&rel_path);
        let upper_path = entry.path().to_path_buf();

        if let Some(change) = classify_file(rel_path, &upper_path, &base_path, need_line_counts) {
            changes.push(change);
        }
    }

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

fn metadata_timestamp_ns(metadata: &fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
}

fn metadata_ctime_ns(metadata: &fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec())
}

fn metadata_kind(metadata: &fs::Metadata) -> ManifestKind {
    if metadata.file_type().is_symlink() {
        ManifestKind::Symlink
    } else if metadata.is_dir() {
        ManifestKind::Dir
    } else {
        ManifestKind::File
    }
}

fn base_matches_manifest(base: &Path, row: &ManifestRow) -> bool {
    let Ok(metadata) = fs::symlink_metadata(base) else {
        return false;
    };
    if metadata_kind(&metadata) != row.kind
        || metadata.len() != row.size
        || metadata_timestamp_ns(&metadata) != row.mtime_ns
    {
        return false;
    }
    if row.kind == ManifestKind::Symlink {
        return fs::read_link(base)
            .ok()
            .is_some_and(|target| Some(manifest::encode_path(&target)) == row.target);
    }
    true
}

fn clone_change(
    workspace: &Workspace,
    path: PathBuf,
    current_path: Option<&Path>,
    baseline: Option<&ManifestRow>,
    need_line_counts: bool,
) -> FileChange {
    let base_path = workspace.base.join(&path);
    let kind = if current_path.is_none() {
        ChangeKind::Deleted
    } else if baseline.is_none()
        || baseline.is_some_and(|row| row.dirty && fs::symlink_metadata(&base_path).is_err())
    {
        ChangeKind::Added
    } else {
        ChangeKind::Modified
    };
    let old_content = match baseline {
        Some(row) if row.dirty => OldContent::AncestorUnknown,
        Some(row) if base_matches_manifest(&base_path, row) => OldContent::Available,
        Some(_) => OldContent::BaseAdvanced,
        None if matches!(kind, ChangeKind::Added) => OldContent::Available,
        None => OldContent::AncestorUnknown,
    };
    let binary =
        need_line_counts && current_path.is_some_and(|path| !path.is_symlink() && is_binary(path));
    let (additions, deletions) = match (&kind, current_path, old_content, need_line_counts) {
        (_, _, _, false) => (None, None),
        (ChangeKind::Added, Some(path), _, true)
            if !binary && !path.is_symlink() && path.is_file() =>
        {
            (
                Some(fs::read_to_string(path).unwrap_or_default().lines().count()),
                None,
            )
        }
        (ChangeKind::Modified, Some(path), OldContent::Available, true)
            if !binary && !path.is_symlink() && path.is_file() =>
        {
            let (additions, deletions) = compute_line_diff(&base_path, path);
            (Some(additions), Some(deletions))
        }
        _ => (None, None),
    };
    FileChange {
        class: ChangeFilter::class(&path),
        path,
        kind,
        additions,
        deletions,
        is_binary: binary,
        old_content,
    }
}

pub fn collect_clone_changes(
    workspace: &Workspace,
    include_ignored: bool,
    need_line_counts: bool,
) -> Result<Vec<FileChange>> {
    let filter = (!include_ignored).then(|| ChangeFilter::build(&workspace.base));
    let baseline = if workspace.manifest_status == ManifestStatus::Ready {
        let streamed = manifest::stream(&workspace.name).and_then(|rows| {
            rows.map(|row| {
                let row = row?;
                Ok((manifest::decode_path(&row.path)?, row))
            })
            .collect::<Result<HashMap<_, _>>>()
        });
        match streamed {
            Ok(rows) => Some(rows),
            Err(_) => {
                let _ = manifest::change_count(workspace, include_ignored)?;
                None
            }
        }
    } else {
        None
    };
    let baseline_known = baseline.is_some();
    let baseline = baseline.unwrap_or_default();
    let fork_time_ns = i128::try_from(workspace.fork_time_ns).unwrap_or(i128::MAX);
    let mut seen = BTreeSet::new();
    let mut pruned = BTreeSet::new();
    let root = &workspace.view_path;
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 || !entry.file_type().is_dir() || include_ignored {
                return true;
            }
            let Ok(relative) = entry.path().strip_prefix(root) else {
                return true;
            };
            let visible = filter
                .as_ref()
                .is_none_or(|filter| filter.is_visible(relative, true));
            if !visible {
                pruned.insert(relative.to_path_buf());
            }
            visible
        });
    let mut changes = Vec::new();
    for entry in walker.skip(1) {
        let entry = entry.walk_context(root)?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| {
                ForkspaceError::StateFailed("clone scan escaped the workspace".to_string())
            })?
            .to_path_buf();
        seen.insert(relative.clone());
        let visible = filter
            .as_ref()
            .is_none_or(|filter| filter.is_visible(&relative, entry.file_type().is_dir()));
        if !visible {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|source| ForkspaceError::Io {
            context: format!("scan clone metadata for {}", entry.path().display()),
            source,
        })?;
        let baseline_row = baseline.get(&relative);
        let changed = match baseline_row {
            Some(row) => {
                row.dirty
                    || metadata_ctime_ns(&metadata) > fork_time_ns
                    || metadata_kind(&metadata) != row.kind
                    || metadata.len() != row.size
                    || metadata_timestamp_ns(&metadata) != row.mtime_ns
                    || (row.kind == ManifestKind::Symlink
                        && fs::read_link(entry.path()).ok().is_none_or(|target| {
                            Some(manifest::encode_path(&target)) != row.target
                        }))
            }
            None => baseline_known || metadata_ctime_ns(&metadata) > fork_time_ns,
        };
        if changed
            && (!metadata.is_dir() || baseline_row.is_some_and(|row| row.kind != ManifestKind::Dir))
        {
            changes.push(clone_change(
                workspace,
                relative,
                Some(entry.path()),
                baseline_row,
                need_line_counts,
            ));
        }
    }

    let missing_directories: BTreeSet<&Path> = baseline
        .iter()
        .filter(|(path, row)| {
            row.kind == ManifestKind::Dir
                && !seen.contains(*path)
                && !pruned.iter().any(|prefix| path.starts_with(prefix))
                && filter
                    .as_ref()
                    .is_none_or(|filter| filter.is_visible(path, true))
        })
        .map(|(path, _)| path.as_path())
        .collect();
    for (path, row) in &baseline {
        if seen.contains(path)
            || pruned.iter().any(|prefix| path.starts_with(prefix))
            || path
                .ancestors()
                .skip(1)
                .any(|ancestor| missing_directories.contains(ancestor))
            || filter
                .as_ref()
                .is_some_and(|filter| !filter.is_visible(path, row.kind == ManifestKind::Dir))
        {
            continue;
        }
        changes.push(clone_change(
            workspace,
            path.clone(),
            None,
            Some(row),
            need_line_counts,
        ));
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
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

/// Replays layer stacking across the parent chain to compute cumulative changes
/// relative to the root base directory.
pub(crate) fn collect_cumulative_changes_fast(
    workspace: &Workspace,
    state: &State,
) -> Result<Vec<FileChange>> {
    collect_cumulative_changes_inner(workspace, state, false)
}

pub(crate) fn collect_cumulative_changes(
    workspace: &Workspace,
    state: &State,
) -> Result<Vec<FileChange>> {
    collect_cumulative_changes_inner(workspace, state, true)
}

fn collect_cumulative_changes_inner(
    workspace: &Workspace,
    state: &State,
    need_line_counts: bool,
) -> Result<Vec<FileChange>> {
    let chain = state.parent_chain(&workspace.name);
    if chain.is_empty() {
        return Ok(Vec::new());
    }

    let original_base = &chain[0].base;

    let uppers: Vec<PathBuf> = chain
        .iter()
        .map(|ws| ws.upper_path().map(Path::to_path_buf))
        .collect::<Result<_>>()?;

    let mut file_map: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    let mut deleted: BTreeSet<PathBuf> = BTreeSet::new();

    for upper in &uppers {
        if !upper.exists() {
            continue;
        }

        for entry in WalkDir::new(upper).min_depth(1) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let rel = match entry.path().strip_prefix(upper) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            };

            let file_name = match entry.file_name().to_str() {
                Some(n) => n,
                None => continue,
            };

            if entry.metadata().is_ok_and(|metadata| metadata.len() == 0) {
                if let Some(whiteout) = valid_whiteout(&rel, file_name, original_base) {
                    match whiteout {
                        Whiteout::Opaque => {
                            let opaque_dir = rel.parent().unwrap_or(Path::new(""));
                            let to_remove: Vec<PathBuf> = file_map
                                .keys()
                                .filter(|p| p.starts_with(opaque_dir))
                                .cloned()
                                .collect();
                            for p in to_remove {
                                file_map.remove(&p);
                            }
                            continue;
                        }
                        Whiteout::Deletion(original_name) => {
                            let deleted_path = whiteout_target_path(&rel, &original_name);
                            file_map.remove(&deleted_path);
                            if original_base.join(&deleted_path).exists() {
                                deleted.insert(deleted_path);
                            }
                            continue;
                        }
                    }
                }
            }

            if entry.file_type().is_dir() {
                continue;
            }

            let abs_path = entry.path().to_path_buf();
            deleted.remove(&rel);
            file_map.insert(rel, abs_path);
        }
    }

    let mut changes = Vec::new();

    for (rel_path, upper_path) in &file_map {
        let base_path = original_base.join(rel_path);
        if let Some(change) =
            classify_file(rel_path.clone(), upper_path, &base_path, need_line_counts)
        {
            changes.push(change);
        }
    }

    for del_path in &deleted {
        changes.push(FileChange::deletion(del_path.clone()));
    }

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}
