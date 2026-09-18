use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::folders::{find_duplicate_folders, FolderGroup};
use crate::progress::Progress;
use crate::store::{self, CheckpointDelta, ResumeState};

pub const PARTIAL_CHUNK_SIZE: usize = 8192;
pub const FULL_READ_CHUNK_SIZE: usize = 1024 * 1024;
pub const LARGE_FILE_THRESHOLD: u64 = 500 * 1024 * 1024; // 500MB

/// How many files a hash/confirm stage processes between checkpoint saves --
/// frequent enough that a crash loses at most this many files' worth of
/// work, infrequent enough that the checkpoint write itself never becomes
/// the bottleneck.
pub const CHECKPOINT_INTERVAL: usize = 2000;

/// Unlike the Python original, stage 2/3 hashing here is parallelized
/// unconditionally via a rayon thread pool regardless of item size: rayon's
/// work-stealing pool has none of the GIL-driven overhead that made a
/// Python thread pool lose to sequential hashing on small files, so there's
/// no equivalent of Python's PARALLEL_IO_THRESHOLD carve-out to port.
/// Candidates are still processed in batches of this size so progress and
/// checkpointing stay responsive on a huge scan.
const BATCH_SIZE: usize = 1000;

/// Directories excluded from the walk by default -- matched case-
/// insensitively against a directory's own name (basename), so this applies
/// no matter how deep it's nested.
pub static DEFAULT_EXCLUDED_DIR_NAMES: &[&str] = &[
    "node_modules",
    "venv",
    ".venv",
    "env",
    ".env",
    "virtualenv",
    ".virtualenv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".cache",
];

static DEFAULT_EXCLUDED_FILE_NAMES: &[&str] = &[".ds_store", ".localized", "thumbs.db", "desktop.ini", ".gitkeep"];
static DEFAULT_EXCLUDED_FILE_EXTENSIONS: &[&str] = &[".tmp", ".temp", ".swp", ".swo", ".bak"];

pub const MISC_FILE_TYPE: &str = "misc";

pub fn file_type_categories() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        (
            "images",
            &[
                ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".tiff", ".tif", ".webp", ".heic", ".heif", ".svg", ".ico",
                ".raw", ".cr2", ".nef", ".arw", ".dng",
            ][..],
        ),
        ("audio", &[".mp3", ".wav", ".flac", ".aac", ".ogg", ".m4a", ".wma", ".aiff", ".alac", ".opus"][..]),
        ("video", &[".mp4", ".mov", ".avi", ".mkv", ".wmv", ".flv", ".webm", ".m4v", ".mpg", ".mpeg", ".3gp"][..]),
        (
            "documents",
            &[
                ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".txt", ".rtf", ".odt", ".ods", ".odp",
                ".md", ".csv", ".pages", ".key", ".numbers",
            ][..],
        ),
        ("archives", &[".zip", ".rar", ".7z", ".tar", ".gz", ".bz2", ".xz", ".tgz", ".tbz2"][..]),
        (
            "programs",
            &[".exe", ".msi", ".dmg", ".pkg", ".apk", ".deb", ".rpm", ".appimage", ".bat", ".sh", ".bin", ".jar"][..],
        ),
    ]
}

pub fn valid_file_types() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = file_type_categories().into_iter().map(|(k, _)| k).collect();
    v.push(MISC_FILE_TYPE);
    v
}

fn all_categorized_extensions() -> HashSet<&'static str> {
    file_type_categories().into_iter().flat_map(|(_, exts)| exts.iter().copied()).collect()
}

fn extension_of(path: &Path) -> String {
    match path.extension() {
        Some(e) => format!(".{}", e.to_string_lossy().to_lowercase()),
        None => String::new(),
    }
}

/// True if `path`'s extension falls into any of the named `categories` --
/// the same rule a scan's `file_types` filter applies, exposed here so a
/// caller can apply it to an already-finished scan's results without
/// re-scanning.
#[allow(dead_code)]
pub fn file_matches_types(path: &Path, categories: &HashSet<String>) -> bool {
    let ext = extension_of(path);
    let all_cats = all_categorized_extensions();
    if categories.contains(MISC_FILE_TYPE) && !all_cats.contains(ext.as_str()) {
        return true;
    }
    file_type_categories()
        .into_iter()
        .any(|(name, exts)| categories.contains(name) && exts.contains(&ext.as_str()))
}

fn is_default_excluded_file(name: &str) -> bool {
    let lowered = name.to_lowercase();
    if DEFAULT_EXCLUDED_FILE_NAMES.contains(&lowered.as_str()) {
        return true;
    }
    if lowered.ends_with('~') {
        return true;
    }
    let ext = match Path::new(&lowered).extension() {
        Some(e) => format!(".{}", e.to_string_lossy()),
        None => String::new(),
    };
    DEFAULT_EXCLUDED_FILE_EXTENSIONS.contains(&ext.as_str())
}

type ExtensionFilter = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// Builds an extension-filter predicate from `file_types`, shared by the
/// scanner and the manifest walker so both apply identical filtering.
pub fn build_extension_filter(file_types: Option<&HashSet<String>>) -> anyhow::Result<Option<ExtensionFilter>> {
    let categories = match file_types {
        None => return Ok(None),
        Some(c) => c.clone(),
    };
    let valid: HashSet<&str> = valid_file_types().into_iter().collect();
    let mut unknown: Vec<String> = categories.iter().filter(|c| !valid.contains(c.as_str())).cloned().collect();
    unknown.sort();
    if !unknown.is_empty() {
        let mut all_valid: Vec<&str> = valid_file_types();
        all_valid.sort();
        anyhow::bail!(
            "Unknown file type categor{}: {} -- valid categories: {}",
            if unknown.len() == 1 { "y" } else { "ies" },
            unknown.join(", "),
            all_valid.join(", ")
        );
    }
    let wants_misc = categories.contains(MISC_FILE_TYPE);
    let mut concrete_extensions: HashSet<String> = HashSet::new();
    for (name, exts) in file_type_categories() {
        if categories.contains(name) {
            concrete_extensions.extend(exts.iter().map(|s| s.to_string()));
        }
    }
    let all_cats: HashSet<String> = all_categorized_extensions().into_iter().map(|s| s.to_string()).collect();
    Ok(Some(Box::new(move |ext: &str| {
        if concrete_extensions.contains(ext) {
            return true;
        }
        wants_misc && !all_cats.contains(ext)
    })))
}

pub fn default_workers() -> usize {
    num_cpus::get().max(1)
}

fn read_chunk(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match f.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn partial_hash(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; PARTIAL_CHUNK_SIZE];
    let n = read_chunk(&mut f, &mut buf)?;
    let mut hasher = Sha256::new();
    hasher.update(&buf[..n]);
    Ok(format!("{:x}", hasher.finalize()))
}

fn full_hash(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; FULL_READ_CHUNK_SIZE];
    loop {
        let n = read_chunk(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// True if two files -- already known to be the same size -- are
/// byte-for-byte identical, comparing chunk by chunk with an early exit at
/// the first difference.
pub fn files_equal(a: &Path, b: &Path) -> io::Result<bool> {
    let mut fa = File::open(a)?;
    let mut fb = File::open(b)?;
    let mut ba = vec![0u8; FULL_READ_CHUNK_SIZE];
    let mut bb = vec![0u8; FULL_READ_CHUNK_SIZE];
    loop {
        let na = read_chunk(&mut fa, &mut ba)?;
        let nb = read_chunk(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

fn dir_identity(path: &Path) -> String {
    std::fs::canonicalize(path).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| path.to_string_lossy().to_string())
}

enum WalkEvent {
    File(PathBuf),
    DirDone(String),
}

fn try_push(
    dir: &Path,
    completed_dirs: &HashSet<String>,
    stack: &mut Vec<(std::fs::ReadDir, String)>,
    visited: &mut HashSet<String>,
) {
    let key = dir_identity(dir);
    if visited.contains(&key) {
        return;
    }
    visited.insert(key.clone());
    if completed_dirs.contains(&key) {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        stack.push((rd, key));
    }
}

/// Recursively yields every file under `directories`, resumable at directory
/// granularity. Follows directory symlinks but never re-enters a real
/// directory already visited (cycle-safe); file symlinks are skipped
/// entirely (a symlink to a file elsewhere is the same file, not a "copy").
///
/// A subdirectory whose name (lowercased) is in `excluded_names` is never
/// descended into; a root in `directories` itself is always scanned
/// regardless of its name. `on_event` returning `false` stops the walk
/// immediately (used for cancellation).
#[allow(clippy::too_many_arguments)]
fn walk_checkpointed(
    directories: &[PathBuf],
    completed_dirs: &HashSet<String>,
    already_seen: &HashSet<PathBuf>,
    excluded_names: &HashSet<String>,
    extension_filter: Option<&ExtensionFilter>,
    exclude_temp_files: bool,
    mut on_event: impl FnMut(WalkEvent) -> bool,
) {
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<(std::fs::ReadDir, String)> = Vec::new();

    for root in directories {
        try_push(root, completed_dirs, &mut stack, &mut visited);
    }

    while let Some((entries, _)) = stack.last_mut() {
        let next = entries.next();
        match next {
            None => {
                let (_, key) = stack.pop().unwrap();
                if !on_event(WalkEvent::DirDone(key)) {
                    return;
                }
            }
            Some(Err(_)) => continue,
            Some(Ok(entry)) => {
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if file_type.is_symlink() {
                    match std::fs::metadata(&path) {
                        Ok(md) if md.is_dir() => {
                            let name = entry.file_name().to_string_lossy().to_lowercase();
                            if excluded_names.contains(&name) {
                                continue;
                            }
                            try_push(&path, completed_dirs, &mut stack, &mut visited);
                        }
                        _ => continue,
                    }
                } else if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    if excluded_names.contains(&name) {
                        continue;
                    }
                    try_push(&path, completed_dirs, &mut stack, &mut visited);
                } else if file_type.is_file() {
                    if already_seen.contains(&path) {
                        continue;
                    }
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if exclude_temp_files && is_default_excluded_file(&fname) {
                        continue;
                    }
                    if let Some(filt) = extension_filter {
                        if !filt(&extension_of(&path)) {
                            continue;
                        }
                    }
                    if !on_event(WalkEvent::File(path)) {
                        return;
                    }
                }
            }
        }
    }
}

/// `confirmed` is false for a very-large-file group whose members are only
/// known to share a size and partial hash -- full confirmation was deferred
/// rather than paying its cost during the scan. `file_hash` for such a group
/// is a "size:partial_hash" string, not a SHA-256 digest.
#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    pub file_hash: String,
    pub size: u64,
    pub paths: Vec<PathBuf>,
    pub confirmed: bool,
}

pub struct ScanResult {
    pub groups: Vec<DuplicateGroup>,
    pub skipped: Vec<PathBuf>,
    pub cancelled: bool,
    pub resume_state: Option<ResumeState>,
    pub folder_groups: Vec<FolderGroup>,
    /// Set to the run_id whose saved results were replayed instead of
    /// rescanning, when `scan_or_reuse` finds nothing changed since that run.
    pub reused_run_id: Option<String>,
}

fn partial_key(size: u64, partial_hash: &str) -> String {
    format!("{}:{}", size, partial_hash)
}

fn serialize_by_size(by_size: &HashMap<u64, Vec<PathBuf>>) -> HashMap<String, Vec<String>> {
    by_size
        .iter()
        .map(|(s, ps)| (s.to_string(), ps.iter().map(|p| p.to_string_lossy().to_string()).collect()))
        .collect()
}

fn take_once<T: Clone>(sent: &mut bool, value: &T) -> Option<T> {
    if *sent {
        None
    } else {
        *sent = true;
        Some(value.clone())
    }
}

fn resume_roots_match(resume_state: &ResumeState, directories: &[PathBuf]) -> bool {
    for directory in directories {
        if let Some(recorded) = resume_state.root_keys.get(&directory.to_string_lossy().to_string()) {
            if &dir_identity(directory) != recorded {
                return false;
            }
        }
    }
    true
}

/// Scan configuration shared by `find_duplicates`/`quick_scan_manifest`.
pub struct ScanOptions {
    pub show_progress: bool,
    /// 0 means "use `default_workers()`".
    pub workers: usize,
    /// 0 disables deferral -- every candidate is fully confirmed during the scan.
    pub large_file_threshold: u64,
    /// Lower-cased directory names to never descend into.
    pub exclude_dirs: HashSet<String>,
    pub exclude_temp_files: bool,
    pub file_types: Option<HashSet<String>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            show_progress: true,
            workers: 0,
            large_file_threshold: LARGE_FILE_THRESHOLD,
            exclude_dirs: DEFAULT_EXCLUDED_DIR_NAMES.iter().map(|s| s.to_string()).collect(),
            exclude_temp_files: true,
            file_types: None,
        }
    }
}

struct BucketResult {
    groups: Vec<(String, Vec<PathBuf>)>,
    skipped: Vec<PathBuf>,
    processed_count: usize,
}

fn resolve_bucket(paths: &[PathBuf]) -> BucketResult {
    let mut remaining: Vec<PathBuf> = paths.to_vec();
    let mut groups: Vec<(String, Vec<PathBuf>)> = Vec::new();
    let mut skipped: Vec<PathBuf> = Vec::new();
    let mut processed_count = 0usize;

    while remaining.len() >= 2 {
        let representative = remaining.remove(0);
        if File::open(&representative).is_err() {
            skipped.push(representative);
            processed_count += 1;
            continue;
        }
        processed_count += 1;
        let rest = std::mem::take(&mut remaining);
        let mut leftover = Vec::new();
        let mut matched = Vec::new();
        for other in rest {
            match files_equal(&representative, &other) {
                Ok(true) => {
                    matched.push(other);
                    processed_count += 1;
                }
                Ok(false) => leftover.push(other),
                Err(_) => {
                    skipped.push(other);
                    processed_count += 1;
                }
            }
        }
        if !matched.is_empty() {
            match full_hash(&representative) {
                Ok(digest) => {
                    let mut group_paths = vec![representative];
                    group_paths.extend(matched);
                    groups.push((digest, group_paths));
                }
                Err(_) => {
                    skipped.push(representative);
                    skipped.extend(matched);
                }
            }
        }
        remaining = leftover;
    }
    if remaining.len() == 1 {
        processed_count += 1;
    }

    BucketResult { groups, skipped, processed_count }
}

/// Find duplicate files across directories using a staged lookup table: size
/// -> size+partial-hash -> confirmed-by-content. Only buckets with 2+ files
/// carry forward at each stage, so most files drop out after the free
/// `stat()` call.
///
/// Files that can't be read are skipped rather than aborting the scan.
/// `cancel`, if set (checked between batches), stops the current stage
/// early; whichever groups were already confirmed are still returned, along
/// with a `ResumeState` a later call can pass back in via `resume_state` to
/// continue instead of redoing already-hashed files.
///
/// Every checkpoint-worthy stage boundary is persisted to the store under
/// `run_id` as it's reached, so a hard crash loses at most a checkpoint
/// interval's worth of work, not just whatever a clean Ctrl+C would have
/// saved.
pub fn find_duplicates(
    directories: &[PathBuf],
    opts: &ScanOptions,
    cancel: &AtomicBool,
    resume_state: Option<ResumeState>,
    run_id: &str,
) -> anyhow::Result<ScanResult> {
    let mut skipped: Vec<PathBuf> = Vec::new();
    let mut cancelled = false;
    let mut cancelled_stage: Option<&'static str> = None;
    let workers = if opts.workers > 0 { opts.workers } else { default_workers() };
    let excluded_names: HashSet<String> = opts.exclude_dirs.iter().map(|s| s.to_lowercase()).collect();
    let extension_filter = build_extension_filter(opts.file_types.as_ref())?;

    let mut resume_state = resume_state;
    if let Some(rs) = &resume_state {
        if !resume_roots_match(rs, directories) {
            eprintln!(
                "warning: ignoring saved progress -- a directory's identity has changed since it was \
                 last checkpointed (e.g. a different drive now mounted at the same path); starting a \
                 fresh scan instead of risking wrong results."
            );
            resume_state = None;
        }
    }
    let resume_stage: Option<String> = resume_state.as_ref().map(|s| s.stage.clone());

    let directories_str: Vec<String> = directories.iter().map(|d| d.to_string_lossy().to_string()).collect();
    let root_keys_map: HashMap<String, String> =
        directories.iter().map(|d| (d.to_string_lossy().to_string(), dir_identity(d))).collect();
    let mut directories_sent = false;
    let mut root_keys_sent = false;
    let mut by_size_sent = false;

    // -- Stage 1: scanning --------------------------------------------------

    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    let mut completed_dirs: HashSet<String> = HashSet::new();

    if matches!(resume_stage.as_deref(), Some("quick_hash") | Some("full_hash")) {
        let rs = resume_state.as_ref().unwrap();
        for (size_str, paths) in &rs.by_size {
            if let Ok(size) = size_str.parse::<u64>() {
                by_size.insert(size, paths.iter().map(PathBuf::from).collect());
            }
        }
        skipped.extend(rs.skipped.iter().map(PathBuf::from));
    } else {
        let mut already_seen: HashSet<PathBuf> = HashSet::new();
        if resume_stage.as_deref() == Some("scanning") {
            let rs = resume_state.as_ref().unwrap();
            for (size_str, paths) in &rs.by_size {
                if let Ok(size) = size_str.parse::<u64>() {
                    by_size.insert(size, paths.iter().map(PathBuf::from).collect());
                }
            }
            already_seen = by_size.values().flatten().cloned().collect();
            completed_dirs = rs.completed_dirs.iter().cloned().collect();
            skipped.extend(rs.skipped.iter().map(PathBuf::from));
            already_seen.extend(skipped.iter().cloned());
        }

        let completed_snapshot = completed_dirs.clone();
        let progress = Progress::new("Scanning", None, opts.show_progress);
        let mut since_flush: usize = 0;
        let mut pending_entries: Vec<(String, String)> = Vec::new();
        let mut pending_completed: Vec<String> = Vec::new();
        let mut pending_skipped: Vec<String> = Vec::new();

        walk_checkpointed(
            directories,
            &completed_snapshot,
            &already_seen,
            &excluded_names,
            extension_filter.as_ref(),
            opts.exclude_temp_files,
            |event| {
                if cancel.load(Ordering::Relaxed) {
                    return false;
                }
                match event {
                    WalkEvent::DirDone(key) => {
                        completed_dirs.insert(key.clone());
                        pending_completed.push(key);
                        since_flush += 1;
                    }
                    WalkEvent::File(path) => match std::fs::metadata(&path) {
                        Ok(md) => {
                            let size = md.len();
                            by_size.entry(size).or_default().push(path.clone());
                            pending_entries.push((size.to_string(), path.to_string_lossy().to_string()));
                            progress.update(1);
                            since_flush += 1;
                        }
                        Err(_) => {
                            skipped.push(path.clone());
                            pending_skipped.push(path.to_string_lossy().to_string());
                            since_flush += 1;
                        }
                    },
                }
                if since_flush >= CHECKPOINT_INTERVAL {
                    flush_stage1(
                        run_id,
                        &mut pending_entries,
                        &mut pending_completed,
                        &mut pending_skipped,
                        &mut directories_sent,
                        &mut root_keys_sent,
                        &directories_str,
                        &root_keys_map,
                    );
                    since_flush = 0;
                }
                true
            },
        );
        flush_stage1(
            run_id,
            &mut pending_entries,
            &mut pending_completed,
            &mut pending_skipped,
            &mut directories_sent,
            &mut root_keys_sent,
            &directories_str,
            &root_keys_map,
        );
        progress.close();
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            cancelled_stage = Some("scanning");
        }
    }

    // -- Stage 2: quick (partial) hash --------------------------------------

    let pool = rayon::ThreadPoolBuilder::new().num_threads(workers.max(1)).build()?;
    let mut by_partial: HashMap<(u64, String), Vec<PathBuf>> = HashMap::new();

    if !cancelled {
        let mut partial_candidates: Vec<(u64, PathBuf)> = Vec::new();
        if resume_stage.as_deref() == Some("full_hash") {
            let rs = resume_state.as_ref().unwrap();
            for (key, paths) in &rs.by_partial {
                if let Some((size_str, hash_str)) = key.split_once(':') {
                    if let Ok(size) = size_str.parse::<u64>() {
                        by_partial.insert((size, hash_str.to_string()), paths.iter().map(PathBuf::from).collect());
                    }
                }
            }
        } else {
            let mut already_processed: HashSet<PathBuf> = HashSet::new();
            if resume_stage.as_deref() == Some("quick_hash") {
                let rs = resume_state.as_ref().unwrap();
                for (key, paths) in &rs.by_partial {
                    if let Some((size_str, hash_str)) = key.split_once(':') {
                        if let Ok(size) = size_str.parse::<u64>() {
                            by_partial.insert((size, hash_str.to_string()), paths.iter().map(PathBuf::from).collect());
                        }
                    }
                }
                already_processed = rs.processed.iter().map(PathBuf::from).collect();
            }
            for (size, paths) in &by_size {
                if paths.len() < 2 {
                    continue;
                }
                for p in paths {
                    if !already_processed.contains(p) {
                        partial_candidates.push((*size, p.clone()));
                    }
                }
            }
        }

        let progress = Progress::new("Quick hash", Some(partial_candidates.len() as u64), opts.show_progress);
        let mut idx = 0;
        while idx < partial_candidates.len() {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                cancelled_stage = Some("quick_hash");
                break;
            }
            let end = (idx + BATCH_SIZE).min(partial_candidates.len());
            let batch = &partial_candidates[idx..end];
            let results: Vec<io::Result<String>> = pool.install(|| batch.par_iter().map(|(_, path)| partial_hash(path)).collect());

            let mut new_entries: Vec<(String, String)> = Vec::new();
            let mut new_skipped: Vec<String> = Vec::new();
            for ((size, path), res) in batch.iter().zip(results) {
                match res {
                    Ok(digest) => {
                        by_partial.entry((*size, digest.clone())).or_default().push(path.clone());
                        new_entries.push((partial_key(*size, &digest), path.to_string_lossy().to_string()));
                    }
                    Err(_) => {
                        skipped.push(path.clone());
                        new_skipped.push(path.to_string_lossy().to_string());
                    }
                }
            }
            progress.update((end - idx) as u64);
            if !new_entries.is_empty() || !new_skipped.is_empty() {
                let delta = CheckpointDelta {
                    stage: "quick_hash".to_string(),
                    new_entries,
                    new_skipped,
                    new_completed_dirs: Vec::new(),
                    by_size: if by_size_sent { None } else { by_size_sent = true; Some(serialize_by_size(&by_size)) },
                    directories: take_once(&mut directories_sent, &directories_str),
                    root_keys: take_once(&mut root_keys_sent, &root_keys_map),
                };
                store::checkpoint_progress(run_id, &delta);
            }
            idx = end;
        }
        progress.close();
    }

    // -- Stage 3: confirm duplicates by content -----------------------------

    let mut by_full: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut deferred_groups: Vec<DuplicateGroup> = Vec::new();

    if !cancelled {
        let mut resolved_paths: HashSet<PathBuf> = HashSet::new();
        let mut full_key_by_path: HashMap<PathBuf, String> = HashMap::new();
        if resume_stage.as_deref() == Some("full_hash") {
            let rs = resume_state.as_ref().unwrap();
            for (full_digest, paths) in &rs.by_full {
                let loaded: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
                for p in &loaded {
                    full_key_by_path.insert(p.clone(), full_digest.clone());
                }
                by_full.insert(full_digest.clone(), loaded);
            }
            resolved_paths = full_key_by_path.keys().cloned().collect();
        }

        let mut buckets: Vec<(u64, Vec<PathBuf>)> = Vec::new();
        for ((size, partial_digest), paths) in by_partial.iter() {
            if paths.len() < 2 {
                continue;
            }
            let bucket_set: HashSet<PathBuf> = paths.iter().cloned().collect();
            if resume_stage.as_deref() == Some("full_hash") {
                if bucket_set.is_subset(&resolved_paths) {
                    continue;
                }
                let stale_keys: HashSet<String> = bucket_set.iter().filter_map(|p| full_key_by_path.get(p).cloned()).collect();
                for key in stale_keys {
                    by_full.remove(&key);
                }
            }
            if opts.large_file_threshold > 0 && *size >= opts.large_file_threshold {
                deferred_groups.push(DuplicateGroup {
                    file_hash: partial_key(*size, partial_digest),
                    size: *size,
                    paths: paths.clone(),
                    confirmed: false,
                });
            } else {
                buckets.push((*size, paths.clone()));
            }
        }

        let full_total: usize = buckets.iter().map(|(_, p)| p.len()).sum();
        let progress = Progress::new("Confirm duplicates", Some(full_total as u64), opts.show_progress);

        let mut batches: Vec<Vec<Vec<PathBuf>>> = Vec::new();
        let mut current_batch: Vec<Vec<PathBuf>> = Vec::new();
        let mut current_count = 0usize;
        for (_, paths) in buckets {
            current_count += paths.len();
            current_batch.push(paths);
            if current_count >= BATCH_SIZE {
                batches.push(std::mem::take(&mut current_batch));
                current_count = 0;
            }
        }
        if !current_batch.is_empty() {
            batches.push(current_batch);
        }

        for batch in batches {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                cancelled_stage = Some("full_hash");
                break;
            }
            let results: Vec<BucketResult> = pool.install(|| batch.par_iter().map(|paths| resolve_bucket(paths)).collect());

            let mut new_entries: Vec<(String, String)> = Vec::new();
            let mut new_skipped: Vec<String> = Vec::new();
            let mut batch_processed = 0usize;
            for r in results {
                for (digest, paths) in r.groups {
                    for p in &paths {
                        new_entries.push((digest.clone(), p.to_string_lossy().to_string()));
                    }
                    by_full.entry(digest).or_default().extend(paths);
                }
                for p in r.skipped {
                    new_skipped.push(p.to_string_lossy().to_string());
                    skipped.push(p);
                }
                batch_processed += r.processed_count;
            }
            progress.update(batch_processed as u64);
            if !new_entries.is_empty() || !new_skipped.is_empty() {
                let delta = CheckpointDelta {
                    stage: "full_hash".to_string(),
                    new_entries,
                    new_skipped,
                    new_completed_dirs: Vec::new(),
                    by_size: if by_size_sent { None } else { by_size_sent = true; Some(serialize_by_size(&by_size)) },
                    directories: take_once(&mut directories_sent, &directories_str),
                    root_keys: take_once(&mut root_keys_sent, &root_keys_map),
                };
                store::checkpoint_progress(run_id, &delta);
            }
        }
        progress.close();
    }

    // Every path in by_full first passed through by_partial keyed by (size,
    // partial_hash) -- reuse that already-known size instead of a fresh stat.
    let mut size_by_path: HashMap<PathBuf, u64> = HashMap::new();
    for ((size, _), paths) in &by_partial {
        for p in paths {
            size_by_path.insert(p.clone(), *size);
        }
    }

    let mut groups: Vec<DuplicateGroup> = by_full
        .iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(file_hash, paths)| {
            let size = paths.first().and_then(|p| size_by_path.get(p)).copied().unwrap_or(0);
            DuplicateGroup { file_hash: file_hash.clone(), size, paths: paths.clone(), confirmed: true }
        })
        .collect();
    groups.extend(deferred_groups);
    groups.sort_by_key(|g| std::cmp::Reverse(g.size.saturating_mul(g.paths.len() as u64)));

    let mut new_resume_state: Option<ResumeState> = None;
    if cancelled {
        let final_root_keys: HashMap<String, String> =
            directories.iter().map(|d| (d.to_string_lossy().to_string(), dir_identity(d))).collect();
        match cancelled_stage {
            Some(stage @ ("quick_hash" | "full_hash")) => {
                new_resume_state = Some(ResumeState {
                    directories: directories_str.clone(),
                    stage: stage.to_string(),
                    by_size: serialize_by_size(&by_size),
                    by_partial: by_partial
                        .iter()
                        .map(|((s, h), ps)| (partial_key(*s, h), ps.iter().map(|p| p.to_string_lossy().to_string()).collect()))
                        .collect(),
                    by_full: by_full.iter().map(|(h, ps)| (h.clone(), ps.iter().map(|p| p.to_string_lossy().to_string()).collect())).collect(),
                    processed: Vec::new(),
                    skipped: skipped.iter().map(|p| p.to_string_lossy().to_string()).collect(),
                    completed_dirs: Vec::new(),
                    root_keys: final_root_keys,
                });
            }
            _ => {
                new_resume_state = Some(ResumeState {
                    directories: directories_str.clone(),
                    stage: "scanning".to_string(),
                    by_size: serialize_by_size(&by_size),
                    by_partial: HashMap::new(),
                    by_full: HashMap::new(),
                    processed: Vec::new(),
                    skipped: skipped.iter().map(|p| p.to_string_lossy().to_string()).collect(),
                    completed_dirs: completed_dirs.into_iter().collect(),
                    root_keys: final_root_keys,
                });
            }
        }
    }

    let mut folder_groups: Vec<FolderGroup> = Vec::new();
    if !cancelled {
        let all_files: Vec<PathBuf> = by_size.values().flatten().cloned().collect();
        folder_groups = find_duplicate_folders(&all_files, &skipped, &groups, directories, opts.show_progress);
    }

    Ok(ScanResult { groups, skipped, cancelled, resume_state: new_resume_state, folder_groups, reused_run_id: None })
}

#[allow(clippy::too_many_arguments)]
fn flush_stage1(
    run_id: &str,
    pending_entries: &mut Vec<(String, String)>,
    pending_completed: &mut Vec<String>,
    pending_skipped: &mut Vec<String>,
    directories_sent: &mut bool,
    root_keys_sent: &mut bool,
    directories_str: &[String],
    root_keys_map: &HashMap<String, String>,
) {
    if pending_entries.is_empty() && pending_completed.is_empty() && pending_skipped.is_empty() {
        return;
    }
    let delta = CheckpointDelta {
        stage: "scanning".to_string(),
        new_entries: std::mem::take(pending_entries),
        new_skipped: std::mem::take(pending_skipped),
        new_completed_dirs: std::mem::take(pending_completed),
        by_size: None,
        directories: take_once(directories_sent, &directories_str.to_vec()),
        root_keys: take_once(root_keys_sent, root_keys_map),
    };
    store::checkpoint_progress(run_id, &delta);
}

/// Cheaply stat every file under `directories` (same filtering rules as
/// `find_duplicates`) without reading or hashing any file content, returning
/// `{path: (size, mtime_secs)}`. Used by `scan_or_reuse` to cheaply check
/// whether a directory tree has changed at all since a previous scan.
///
/// Returns `None` if `cancel` fires before the walk finishes.
pub fn quick_scan_manifest(
    directories: &[PathBuf],
    opts: &ScanOptions,
    cancel: &AtomicBool,
) -> anyhow::Result<Option<HashMap<String, (u64, f64)>>> {
    let excluded_names: HashSet<String> = opts.exclude_dirs.iter().map(|s| s.to_lowercase()).collect();
    let extension_filter = build_extension_filter(opts.file_types.as_ref())?;
    let mut manifest: HashMap<String, (u64, f64)> = HashMap::new();
    let mut cancelled = false;

    walk_checkpointed(
        directories,
        &HashSet::new(),
        &HashSet::new(),
        &excluded_names,
        extension_filter.as_ref(),
        opts.exclude_temp_files,
        |event| {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                return false;
            }
            if let WalkEvent::File(path) = event {
                if let Ok(md) = std::fs::metadata(&path) {
                    let mtime = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    manifest.insert(path.to_string_lossy().to_string(), (md.len(), mtime));
                }
            }
            true
        },
    );

    if cancelled {
        return Ok(None);
    }
    Ok(Some(manifest))
}

/// Like `find_duplicates`, but first checks whether this exact set of
/// directories was scanned before with nothing having changed since -- and
/// if so, replays that earlier scan's saved duplicate groups instantly
/// instead of rehashing every file.
///
/// Only attempted for a fresh scan (`resume_state` is `None`). On any
/// difference (or no previous scan of this directory set), falls back to a
/// real `find_duplicates` call, then saves the manifest just collected under
/// `run_id` so the *next* scan of these directories can be checked against it.
pub fn scan_or_reuse(
    directories: &[PathBuf],
    run_id: &str,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    resume_state: Option<ResumeState>,
) -> anyhow::Result<ScanResult> {
    let mut manifest: Option<HashMap<String, (u64, f64)>> = None;
    if resume_state.is_none() {
        manifest = quick_scan_manifest(directories, opts, cancel)?;
        if manifest.is_none() {
            return Ok(ScanResult { groups: vec![], skipped: vec![], cancelled: true, resume_state: None, folder_groups: vec![], reused_run_id: None });
        }
        let dirs_str: Vec<String> = directories.iter().map(|d| d.to_string_lossy().to_string()).collect();
        if let Some(reused_run_id) = store::find_reusable_run(&dirs_str, manifest.as_ref().unwrap())? {
            if let Some((groups, folder_groups)) = store::load_run_groups(&reused_run_id)? {
                return Ok(ScanResult {
                    groups,
                    skipped: vec![],
                    cancelled: false,
                    resume_state: None,
                    folder_groups,
                    reused_run_id: Some(reused_run_id),
                });
            }
        }
    }

    let result = find_duplicates(directories, opts, cancel, resume_state, run_id)?;
    if let Some(m) = &manifest {
        if !result.cancelled {
            let dirs_str: Vec<String> = directories.iter().map(|d| d.to_string_lossy().to_string()).collect();
            store::save_scan_manifest(&dirs_str, run_id, m)?;
        }
    }
    Ok(result)
}
