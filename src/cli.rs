use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{Local, TimeZone};
use clap::Parser;

use crate::dedupe::{self, DuplicateGroup, ScanOptions};
use crate::folders::FolderGroup;
use crate::formatting::human_size;
use crate::store;

#[derive(Parser)]
#[command(name = "file-sorter", about = "Find duplicate files across one or more directories.", version)]
struct Cli {
    /// Directories to scan for duplicates (searched recursively)
    directories: Vec<String>,

    /// Ignore files smaller than this many bytes
    #[arg(long, default_value_t = 0, value_name = "BYTES")]
    min_size: u64,

    /// Increase log verbosity (-v for stage info, -vv for per-file debug logs)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Suppress the live progress display
    #[arg(short, long)]
    quiet: bool,

    /// Number of threads to hash files with (default: one per CPU core)
    #[arg(short = 'j', long = "threads", default_value_t = 0, value_name = "N")]
    threads: usize,

    /// Files at or above this size are reported as probable duplicates (matched by size +
    /// partial hash) without being fully compared during the scan -- confirmation is deferred
    /// until deletion. Pass 0 to always fully confirm during the scan.
    #[arg(long, default_value_t = dedupe::LARGE_FILE_THRESHOLD, value_name = "BYTES")]
    large_threshold: u64,

    /// Directory name to skip entirely wherever it's encountered -- repeatable
    #[arg(long = "exclude", value_name = "NAME")]
    exclude: Vec<String>,

    /// Don't skip the built-in default excluded directories/files -- scan everything
    #[arg(long)]
    no_default_excludes: bool,

    /// Only scan files of this type -- repeatable to combine categories
    #[arg(long = "type", value_name = "CATEGORY")]
    file_types: Vec<String>,

    /// Delete duplicates after scanning -- keeps the first copy in each group, moves the rest
    /// to the Trash
    #[arg(long)]
    delete: bool,

    /// Move duplicates into DIR instead of deleting them -- keeps the first copy in each group
    /// in place (same convention as --delete) and moves the rest, mirroring each moved
    /// file's/folder's original absolute path underneath DIR (e.g. a file from
    /// /home/user/a.jpg lands at DIR/home/user/a.jpg) so duplicates from different source
    /// folders never collide by name and stay easy to trace back. DIR is created if it doesn't
    /// exist. Mutually exclusive with --delete.
    #[arg(long, value_name = "DIR")]
    move_to: Option<String>,

    /// Skip the confirmation prompt before deleting/moving (only meaningful with
    /// --delete/--move-to)
    #[arg(short = 'y', long)]
    yes: bool,

    /// Preview what --delete/--move-to would do without touching anything
    #[arg(long)]
    dry_run: bool,

    /// Resume a scan that was cancelled before it finished. With no value, resumes the most
    /// recently stopped run; pass the # index shown by --history to resume a specific one.
    #[arg(long, num_args = 0..=1, default_missing_value = "LAST", value_name = "N")]
    resume: Option<String>,

    /// Show past run history instead of scanning
    #[arg(long)]
    history: bool,

    /// Reload and print the full results of a past run (the # index shown by --history)
    /// instead of scanning
    #[arg(long, value_name = "N")]
    show: Option<usize>,

    /// Export run history to a JSON file instead of scanning
    #[arg(long, value_name = "PATH")]
    export_history: Option<String>,

    /// Import run history from a JSON file instead of scanning (merges with existing)
    #[arg(long, value_name = "PATH")]
    import_history: Option<String>,
}

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
        if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            return if raw == "~" { PathBuf::from(home) } else { PathBuf::from(home).join(&raw[2..]) };
        }
    }
    PathBuf::from(raw)
}

fn resolve_path(raw: &str) -> anyhow::Result<PathBuf> {
    Ok(std::path::absolute(expand_tilde(raw))?)
}

fn format_timestamp(ts: f64) -> String {
    match Local.timestamp_opt(ts as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => "?".to_string(),
    }
}

fn print_history() -> anyhow::Result<()> {
    let mut records = store::load_history()?;
    records.reverse();
    if records.is_empty() {
        println!("No run history yet.");
        return Ok(());
    }
    let resumable = store::resumable_run_ids()?;
    for (i, record) in records.iter().enumerate() {
        let index = i + 1;
        let when = format_timestamp(record.timestamp);
        let run_id = format!("{}", record.timestamp);
        let status = if !record.cancelled {
            "done"
        } else if resumable.contains(&run_id) {
            "cancelled, resumable"
        } else {
            "cancelled"
        };
        println!("#{}  {}  [{}]  {}", index, when, status, record.directories.join(", "));
        println!(
            "    {} duplicate group(s), {} reclaimable, {} skipped, {:.1}s",
            record.groups,
            human_size(record.reclaimable_bytes.max(0) as u64),
            record.skipped,
            record.duration_seconds
        );
    }
    Ok(())
}

/// Turns `--resume`'s value into a resume_runs run_id. `resume_arg` is
/// "LAST" for a bare `--resume`, or the #index string from `--history`'s
/// output for `--resume N`. Returns (run_id, error_message) -- exactly one
/// of the two is Some.
fn resolve_resume_run_id(resume_arg: &str) -> anyhow::Result<(Option<String>, Option<String>)> {
    if resume_arg == "LAST" {
        return match store::latest_resume_run_id()? {
            Some(id) => Ok((Some(id), None)),
            None => Ok((None, Some("No stopped run to resume.".to_string()))),
        };
    }

    let index: usize = match resume_arg.parse() {
        Ok(i) => i,
        Err(_) => {
            return Ok((None, Some(format!("--resume expects the # index shown by --history, got {:?}", resume_arg))))
        }
    };
    let mut records = store::load_history()?;
    records.reverse();
    if index < 1 || index > records.len() {
        return Ok((None, Some(format!("No run #{} in history -- run --history to see valid indexes.", index))));
    }
    let record = &records[index - 1];
    let run_id = format!("{}", record.timestamp);
    let resumable = store::resumable_run_ids()?;
    if !record.cancelled || !resumable.contains(&run_id) {
        return Ok((None, Some(format!("Run #{} has no saved progress to resume.", index))));
    }
    Ok((Some(run_id), None))
}

fn print_scan_results(groups: &[DuplicateGroup], folder_groups: &[FolderGroup], skipped_count: usize, cancelled: bool) {
    if !folder_groups.is_empty() {
        for fg in folder_groups {
            let label = if fg.confirmed {
                format!("({} file(s), {} each)", fg.file_count, human_size(fg.size))
            } else {
                format!("({} file(s), {} each, NOT VERIFIED -- large file(s))", fg.file_count, human_size(fg.size))
            };
            println!("\nFolder duplicate: {} copies {}:", fg.paths.len(), label);
            for path in &fg.paths {
                println!("  {}", path.display());
            }
        }
        println!(
            "\n{} duplicate folder(s) found -- their files are also listed individually below. \
             --delete removes a confirmed one as a single unit; an unverified (large-file) one is \
             handled file by file instead.",
            folder_groups.len()
        );
    }

    if groups.is_empty() {
        println!("{}", if !cancelled { "No duplicates found." } else { "Scan cancelled before any duplicates were confirmed." });
    } else {
        let mut total_wasted: u64 = 0;
        let mut deferred_count = 0;
        for group in groups {
            let wasted = group.size.saturating_mul(group.paths.len().saturating_sub(1) as u64);
            total_wasted += wasted;
            let label = if group.confirmed {
                format!("(sha256 {}...)", &group.file_hash[..group.file_hash.len().min(12)])
            } else {
                deferred_count += 1;
                "(NOT VERIFIED -- large file, matched by size + partial hash only)".to_string()
            };
            println!("\n{} copies, {} each {}:", group.paths.len(), human_size(group.size), label);
            for path in &group.paths {
                println!("  {}", path.display());
            }
        }
        let note = if cancelled { " (scan cancelled -- partial results)" } else { "" };
        println!("\n{} duplicate group(s), {} reclaimable{}.", groups.len(), human_size(total_wasted), note);
        if deferred_count > 0 {
            println!(
                "{} of those group(s) are large files not fully verified -- they will be confirmed \
                 before deletion, and a group could turn out to be a false match (files that only \
                 happen to share a size and partial hash).",
                deferred_count
            );
        }
    }

    if skipped_count > 0 {
        println!("\nSkipped {} unreadable file(s) (permission denied or removed).", skipped_count);
    }
}

fn show_past_run(index: usize) -> anyhow::Result<i32> {
    let mut records = store::load_history()?;
    records.reverse();
    if index < 1 || index > records.len() {
        println!("No run #{} in history -- run --history to see valid indexes.", index);
        return Ok(1);
    }
    let record = &records[index - 1];
    let run_id = format!("{}", record.timestamp);
    let Some((groups, folder_groups)) = store::load_run_groups(&run_id)? else {
        println!("Run #{} has no saved detailed results to show (only its summary is kept).", index);
        return Ok(1);
    };
    let when = format_timestamp(record.timestamp);
    println!("Run #{} -- {} -- {}", index, when, record.directories.join(", "));
    print_scan_results(&groups, &folder_groups, record.skipped as usize, record.cancelled);
    Ok(0)
}

fn under_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

type FoldersToRemove<'a> = Vec<(&'a FolderGroup, &'a PathBuf)>;
type FilesToRemove<'a> = Vec<(&'a DuplicateGroup, &'a PathBuf)>;

/// Picks which folder/file copies `--delete` and `--move-to` both act on.
///
/// Only a *confirmed* folder group can be handled as a single unit -- every
/// file inside one was already individually confirmed, so acting on the
/// whole directory needs no further verification. An unconfirmed
/// (deferred, large-file) folder group is left alone here entirely; its
/// files fall through to the per-file plan below, which already verifies
/// each one before it's touched.
fn plan_duplicates_to_remove<'a>(
    groups: &'a [DuplicateGroup],
    folder_groups: &'a [FolderGroup],
) -> (FoldersToRemove<'a>, FilesToRemove<'a>) {
    let folders_to_remove: Vec<(&FolderGroup, &PathBuf)> =
        folder_groups.iter().filter(|fg| fg.confirmed).flat_map(|fg| fg.paths[1..].iter().map(move |p| (fg, p))).collect();
    let remove_dirs: Vec<PathBuf> = folders_to_remove.iter().map(|(_, p)| (*p).clone()).collect();

    // Re-derive keep/remove among paths not already covered by a folder-level
    // removal, rather than blindly trusting group.paths[0]/[1:] -- otherwise
    // a file whose "kept" copy sits in a folder being bulk-removed could end
    // up with every copy gone.
    let mut files_to_remove: Vec<(&DuplicateGroup, &PathBuf)> = Vec::new();
    for group in groups {
        let remaining: Vec<&PathBuf> = group.paths.iter().filter(|p| !under_any(p, &remove_dirs)).collect();
        if remaining.len() < 2 {
            continue;
        }
        for p in &remaining[1..] {
            files_to_remove.push((group, p));
        }
    }

    (folders_to_remove, files_to_remove)
}

/// Where `path` lands under `dest_root`, preserving its full source
/// hierarchy -- mirrors the drive/root too (e.g. D:\Photos\a.jpg ->
/// dest_root/D/Photos/a.jpg, /home/user/a.jpg -> dest_root/home/user/a.jpg)
/// so duplicates that happen to share a relative path under different
/// scanned directories -- or different drives entirely -- never collide at
/// the destination.
fn mirrored_path(path: &Path, dest_root: &Path) -> PathBuf {
    let mut result = dest_root.to_path_buf();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => {
                let raw = prefix.as_os_str().to_string_lossy();
                result.push(raw.trim_end_matches(':'));
            }
            std::path::Component::RootDir | std::path::Component::CurDir | std::path::Component::ParentDir => {}
            std::path::Component::Normal(part) => result.push(part),
        }
    }
    result
}

fn move_path(src: &Path, dest: &Path) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) if src.is_dir() => {
            copy_dir_all(src, dest)?;
            std::fs::remove_dir_all(src)
        }
        Err(_) => {
            std::fs::copy(src, dest)?;
            std::fs::remove_file(src)
        }
    }
}

fn copy_dir_all(src: &Path, dest: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dest_path = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

fn delete_duplicates(groups: &[DuplicateGroup], folder_groups: &[FolderGroup], skip_confirmation: bool, dry_run: bool) -> anyhow::Result<()> {
    let (folders_to_delete, files_to_delete) = plan_duplicates_to_remove(groups, folder_groups);

    if folders_to_delete.is_empty() && files_to_delete.is_empty() {
        return Ok(());
    }

    if dry_run {
        println!("\nDry run -- nothing will actually be deleted.");
    } else if !skip_confirmation {
        let mut parts = Vec::new();
        if !folders_to_delete.is_empty() {
            parts.push(format!("{} folder(s)", folders_to_delete.len()));
        }
        if !files_to_delete.is_empty() {
            parts.push(format!("{} file(s)", files_to_delete.len()));
        }
        print!("\nMove {} to Trash? [y/N] ", parts.join(" and "));
        io::stdout().flush().ok();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            answer.clear();
        }
        let answer = answer.trim().to_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Aborted -- nothing deleted.");
            return Ok(());
        }
    }

    let mut deleted_folders = 0;
    let mut deleted_files = 0;
    let mut failures: Vec<String> = Vec::new();

    for (_fg, path) in &folders_to_delete {
        if dry_run {
            println!("  would delete folder: {}", path.display());
            deleted_folders += 1;
            continue;
        }
        match trash::delete(path) {
            Ok(()) => deleted_folders += 1,
            Err(e) => failures.push(format!("{}: {}", path.display(), e)),
        }
    }

    for (group, path) in &files_to_delete {
        if !group.confirmed {
            match dedupe::files_equal(&group.paths[0], path) {
                Ok(true) => {}
                Ok(false) => {
                    let skip_verb = if dry_run { "would be skipped" } else { "skipped" };
                    failures.push(format!(
                        "{}: not verified as an actual duplicate of the kept file -- {} rather than risk deleting a non-duplicate",
                        path.display(),
                        skip_verb
                    ));
                    continue;
                }
                Err(e) => {
                    failures.push(format!("{}: could not verify against kept file: {}", path.display(), e));
                    continue;
                }
            }
        }
        if dry_run {
            println!("  would delete: {}", path.display());
            deleted_files += 1;
            continue;
        }
        match trash::delete(path) {
            Ok(()) => deleted_files += 1,
            Err(e) => failures.push(format!("{}: {}", path.display(), e)),
        }
    }

    let verb = if dry_run { "Would delete" } else { "Deleted" };
    println!("\n{} {} folder(s) and {} file(s) to Trash.", verb, deleted_folders, deleted_files);
    if !failures.is_empty() {
        let label = if dry_run { "would not be deleted" } else { "were not deleted" };
        println!("{} item(s) {}:", failures.len(), label);
        for line in &failures {
            println!("  {}", line);
        }
    }

    Ok(())
}

fn move_duplicates(
    groups: &[DuplicateGroup],
    folder_groups: &[FolderGroup],
    dest_root: &Path,
    skip_confirmation: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let (folders_to_move, files_to_move) = plan_duplicates_to_remove(groups, folder_groups);

    if folders_to_move.is_empty() && files_to_move.is_empty() {
        return Ok(());
    }

    if dry_run {
        println!("\nDry run -- nothing will actually be moved to {}.", dest_root.display());
    } else if !skip_confirmation {
        let mut parts = Vec::new();
        if !folders_to_move.is_empty() {
            parts.push(format!("{} folder(s)", folders_to_move.len()));
        }
        if !files_to_move.is_empty() {
            parts.push(format!("{} file(s)", files_to_move.len()));
        }
        print!("\nMove {} to {}? [y/N] ", parts.join(" and "), dest_root.display());
        io::stdout().flush().ok();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            answer.clear();
        }
        let answer = answer.trim().to_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Aborted -- nothing moved.");
            return Ok(());
        }
    }

    if !dry_run {
        std::fs::create_dir_all(dest_root)?;
    }

    let mut moved_folders = 0;
    let mut moved_files = 0;
    let mut failures: Vec<String> = Vec::new();

    for (_fg, path) in &folders_to_move {
        let dest = mirrored_path(path, dest_root);
        if dest.exists() {
            failures.push(format!("{}: destination {} already exists -- skipped", path.display(), dest.display()));
            continue;
        }
        if dry_run {
            println!("  would move folder: {} -> {}", path.display(), dest.display());
            moved_folders += 1;
            continue;
        }
        match move_path(path, &dest) {
            Ok(()) => moved_folders += 1,
            Err(e) => failures.push(format!("{}: {}", path.display(), e)),
        }
    }

    for (group, path) in &files_to_move {
        if !group.confirmed {
            match dedupe::files_equal(&group.paths[0], path) {
                Ok(true) => {}
                Ok(false) => {
                    let skip_verb = if dry_run { "would be skipped" } else { "skipped" };
                    failures.push(format!(
                        "{}: not verified as an actual duplicate of the kept file -- {} rather than risk moving a non-duplicate",
                        path.display(),
                        skip_verb
                    ));
                    continue;
                }
                Err(e) => {
                    failures.push(format!("{}: could not verify against kept file: {}", path.display(), e));
                    continue;
                }
            }
        }
        let dest = mirrored_path(path, dest_root);
        if dest.exists() {
            failures.push(format!("{}: destination {} already exists -- skipped", path.display(), dest.display()));
            continue;
        }
        if dry_run {
            println!("  would move: {} -> {}", path.display(), dest.display());
            moved_files += 1;
            continue;
        }
        match move_path(path, &dest) {
            Ok(()) => moved_files += 1,
            Err(e) => failures.push(format!("{}: {}", path.display(), e)),
        }
    }

    let verb = if dry_run { "Would move" } else { "Moved" };
    println!("\n{} {} folder(s) and {} file(s) to {}.", verb, moved_folders, moved_files, dest_root.display());
    if !failures.is_empty() {
        let label = if dry_run { "would not be moved" } else { "were not moved" };
        println!("{} item(s) {}:", failures.len(), label);
        for line in &failures {
            println!("  {}", line);
        }
    }

    Ok(())
}

pub fn run() -> anyhow::Result<i32> {
    let cli = Cli::parse();
    let _ = cli.verbose; // verbosity only ever gated internal logging in the Python original; no-op here.

    if cli.delete && cli.move_to.is_some() {
        println!("Error: --delete and --move-to are mutually exclusive");
        return Ok(1);
    }

    if cli.history {
        print_history()?;
        return Ok(0);
    }

    if let Some(path) = &cli.export_history {
        let path = resolve_path(path)?;
        let count = store::export_history(&path)?;
        println!("Exported {} run(s) to {}", count, path.display());
        return Ok(0);
    }

    if let Some(path) = &cli.import_history {
        let path = resolve_path(path)?;
        if !path.is_file() {
            println!("Error: {} is not a file", path.display());
            return Ok(1);
        }
        return match store::import_history(&path) {
            Ok(added) => {
                println!("Imported {} new run(s) from {}", added, path.display());
                Ok(0)
            }
            Err(e) => {
                println!("Error importing history: {}", e);
                Ok(1)
            }
        };
    }

    if let Some(idx) = cli.show {
        return show_past_run(idx);
    }

    let mut file_types_set: Option<HashSet<String>> = None;
    if !cli.file_types.is_empty() {
        let valid: HashSet<&str> = dedupe::valid_file_types().into_iter().collect();
        for t in &cli.file_types {
            if !valid.contains(t.as_str()) {
                let mut v: Vec<&str> = dedupe::valid_file_types();
                v.sort();
                println!("Error: invalid --type value {:?} -- valid categories: {}", t, v.join(", "));
                return Ok(2);
            }
        }
        file_types_set = Some(cli.file_types.iter().cloned().collect());
    }

    let mut resume_state: Option<store::ResumeState> = None;
    let mut resume_run_id: Option<String> = None;
    let directories: Vec<PathBuf>;

    if let Some(resume_arg) = &cli.resume {
        if !cli.directories.is_empty() {
            println!("Error: --resume picks up a stopped scan and doesn't take directories");
            return Ok(1);
        }
        let (run_id, error) = resolve_resume_run_id(resume_arg)?;
        if let Some(err) = error {
            println!("{}", err);
            return Ok(1);
        }
        let run_id = run_id.unwrap();
        let Some(state) = store::load_resume_state(&run_id)? else {
            println!("No stopped run to resume.");
            return Ok(1);
        };
        let dirs: Vec<PathBuf> = state.directories.iter().map(PathBuf::from).collect();
        for p in &dirs {
            if !p.is_dir() {
                println!("Error: cannot resume -- {} is no longer a directory", p.display());
                return Ok(1);
            }
        }
        directories = dirs;
        resume_run_id = Some(run_id);
        resume_state = Some(state);
    } else {
        if cli.directories.is_empty() {
            println!("Error: the following arguments are required: directories");
            return Ok(2);
        }
        let mut dirs = Vec::new();
        for raw in &cli.directories {
            let path = resolve_path(raw)?;
            if !path.is_dir() {
                println!("Error: {} is not a directory", path.display());
                return Ok(1);
            }
            dirs.push(path);
        }
        directories = dirs;
    }

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = Arc::clone(&cancel);
        ctrlc::set_handler(move || {
            if cancel.swap(true, Ordering::SeqCst) {
                eprintln!("\nForce quitting.");
                std::process::exit(130);
            }
            eprintln!("\nCancelling... (press Ctrl+C again to force quit)");
        })?;
    }

    // Reused from --resume when picking up a stopped run, or freshly minted
    // otherwise, so mid-scan checkpoints have somewhere to save to from the
    // very start.
    let run_id = resume_run_id.clone().unwrap_or_else(|| format!("{}", store::now_secs()));

    let exclude_dirs: HashSet<String> = if cli.no_default_excludes {
        cli.exclude.iter().cloned().collect()
    } else {
        dedupe::DEFAULT_EXCLUDED_DIR_NAMES.iter().map(|s| s.to_string()).chain(cli.exclude.iter().cloned()).collect()
    };
    let exclude_temp_files = !cli.no_default_excludes;

    let opts = ScanOptions {
        show_progress: !cli.quiet,
        workers: cli.threads,
        large_file_threshold: cli.large_threshold,
        exclude_dirs,
        exclude_temp_files,
        file_types: file_types_set,
    };

    let start = std::time::Instant::now();
    let result = dedupe::scan_or_reuse(&directories, &run_id, &opts, &cancel, resume_state)?;
    let duration = start.elapsed().as_secs_f64();

    if result.reused_run_id.is_some() && !cli.quiet {
        eprintln!("Nothing changed since the last scan of these directories -- reused those results.");
    }

    store::record_run(&directories, &result, duration, &run_id)?;
    store::save_run_groups(&run_id, &result)?;
    store::clear_resume_state(&run_id)?;
    if result.cancelled {
        if let Some(rs) = &result.resume_state {
            store::save_resume_state(&run_id, rs)?;
        }
    }

    let mut folder_groups = result.folder_groups.clone();
    if cli.min_size > 0 {
        folder_groups.retain(|g| g.size >= cli.min_size);
    }
    let mut groups = result.groups.clone();
    if cli.min_size > 0 {
        groups.retain(|g| g.size >= cli.min_size);
    }

    print_scan_results(&groups, &folder_groups, result.skipped.len(), result.cancelled);

    if result.cancelled && result.resume_state.is_some() {
        println!("\nRun 'file-sorter --resume' to continue this scan where it left off.");
    }

    if cli.delete && (!groups.is_empty() || !result.folder_groups.is_empty()) {
        delete_duplicates(&groups, &result.folder_groups, cli.yes, cli.dry_run)?;
    }

    if let Some(move_to) = &cli.move_to {
        if !groups.is_empty() || !result.folder_groups.is_empty() {
            let dest_root = resolve_path(move_to)?;
            move_duplicates(&groups, &result.folder_groups, &dest_root, cli.yes, cli.dry_run)?;
        }
    }

    Ok(0)
}
