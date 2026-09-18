use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::dedupe::{self, DuplicateGroup, ScanOptions};
use crate::folders::FolderGroup;
use crate::store;

/// A finished (or cancelled) scan's results, filtered by the min-size
/// threshold the same way the CLI applies `--min-size` after scanning.
#[derive(Debug, Clone)]
pub struct ScanSummary {
    pub run_id: String,
    pub directories: Vec<PathBuf>,
    pub groups: Vec<DuplicateGroup>,
    pub folder_groups: Vec<FolderGroup>,
    pub skipped: usize,
    pub cancelled: bool,
    pub resumable: bool,
    pub reused: bool,
    pub duration_seconds: f64,
    pub reclaimable_bytes: u64,
}

/// A background scan handed off to a worker thread. Polled from the UI's
/// `Tick` subscription rather than awaited, since `find_duplicates` is a
/// long blocking call the iced executor shouldn't be asked to run inline.
pub struct ScanHandle {
    pub cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    result: Arc<Mutex<Option<anyhow::Result<ScanSummary>>>>,
}

impl ScanHandle {
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn take(&self) -> Option<anyhow::Result<ScanSummary>> {
        self.result.lock().unwrap().take()
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// Starts a fresh scan of `directories`, or -- when `resume_run_id` is set --
/// resumes a previously cancelled run, in which case `directories` is
/// ignored and the run's own saved directory list is used instead (mirrors
/// `file-sorter --resume`).
pub fn start_scan(directories: Vec<PathBuf>, opts: ScanOptions, min_size: u64, resume_run_id: Option<String>) -> ScanHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let result = Arc::new(Mutex::new(None));

    let handle = ScanHandle { cancel: Arc::clone(&cancel), done: Arc::clone(&done), result: Arc::clone(&result) };

    std::thread::spawn(move || {
        let outcome = run_scan(directories, opts, min_size, resume_run_id, &cancel);
        *result.lock().unwrap() = Some(outcome);
        done.store(true, Ordering::Relaxed);
    });

    handle
}

fn run_scan(
    directories: Vec<PathBuf>,
    opts: ScanOptions,
    min_size: u64,
    resume_run_id: Option<String>,
    cancel: &AtomicBool,
) -> anyhow::Result<ScanSummary> {
    let (run_id, resume_state, directories) = match resume_run_id {
        Some(id) => {
            let state = store::load_resume_state(&id)?.ok_or_else(|| anyhow::anyhow!("No stopped run to resume."))?;
            let dirs: Vec<PathBuf> = state.directories.iter().map(PathBuf::from).collect();
            for p in &dirs {
                if !p.is_dir() {
                    anyhow::bail!("Cannot resume -- {} is no longer a directory", p.display());
                }
            }
            (id, Some(state), dirs)
        }
        None => (format!("{}", store::now_secs()), None, directories),
    };

    let start = std::time::Instant::now();
    let result = dedupe::scan_or_reuse(&directories, &run_id, &opts, cancel, resume_state)?;
    let duration = start.elapsed().as_secs_f64();

    let record = store::record_run(&directories, &result, duration, &run_id)?;
    store::save_run_groups(&run_id, &result)?;
    store::clear_resume_state(&run_id)?;
    if result.cancelled {
        if let Some(rs) = &result.resume_state {
            store::save_resume_state(&run_id, rs)?;
        }
    }

    let mut groups = result.groups;
    let mut folder_groups = result.folder_groups;
    if min_size > 0 {
        groups.retain(|g| g.size >= min_size);
        folder_groups.retain(|g| g.size >= min_size);
    }

    Ok(ScanSummary {
        run_id,
        directories,
        groups,
        folder_groups,
        skipped: result.skipped.len(),
        cancelled: result.cancelled,
        resumable: result.cancelled && result.resume_state.is_some(),
        reused: result.reused_run_id.is_some(),
        duration_seconds: duration,
        reclaimable_bytes: record.reclaimable_bytes.max(0) as u64,
    })
}

/// A single file or folder selected for removal/move, plus whatever's needed
/// to safely act on it.
#[derive(Debug, Clone)]
pub struct ActionItem {
    pub path: PathBuf,
    pub is_folder: bool,
    /// When true, `path` must first be confirmed as an actual duplicate of
    /// `reference` (an unconfirmed/deferred group) before it's touched --
    /// mirrors the CLI's own safety check.
    pub needs_verify: bool,
    pub reference: PathBuf,
}

#[derive(Debug, Clone)]
pub enum ActionKind {
    Trash,
    MoveTo(PathBuf),
}

#[derive(Debug, Clone, Default)]
pub struct ActionReport {
    pub moved_files: usize,
    pub moved_folders: usize,
    pub failures: Vec<String>,
    pub dry_run: bool,
    pub dest: Option<PathBuf>,
    /// Paths that were actually removed/moved (empty for a dry run), so the
    /// UI can prune them out of the results it's showing.
    pub succeeded_paths: Vec<PathBuf>,
}

pub struct ActionHandle {
    done: Arc<AtomicBool>,
    result: Arc<Mutex<Option<ActionReport>>>,
}

impl ActionHandle {
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn take(&self) -> Option<ActionReport> {
        self.result.lock().unwrap().take()
    }
}

pub fn start_action(items: Vec<ActionItem>, kind: ActionKind, dry_run: bool) -> ActionHandle {
    let done = Arc::new(AtomicBool::new(false));
    let result = Arc::new(Mutex::new(None));
    let handle = ActionHandle { done: Arc::clone(&done), result: Arc::clone(&result) };

    std::thread::spawn(move || {
        let report = run_action(items, kind, dry_run);
        *result.lock().unwrap() = Some(report);
        done.store(true, Ordering::Relaxed);
    });

    handle
}

fn run_action(items: Vec<ActionItem>, kind: ActionKind, dry_run: bool) -> ActionReport {
    let mut report =
        ActionReport { dry_run, dest: if let ActionKind::MoveTo(d) = &kind { Some(d.clone()) } else { None }, ..Default::default() };

    if let ActionKind::MoveTo(dest_root) = &kind {
        if !dry_run {
            if let Err(e) = std::fs::create_dir_all(dest_root) {
                report.failures.push(format!("{}: could not create destination: {}", dest_root.display(), e));
                return report;
            }
        }
    }

    for item in items {
        if item.needs_verify {
            match dedupe::files_equal(&item.reference, &item.path) {
                Ok(true) => {}
                Ok(false) => {
                    report.failures.push(format!(
                        "{}: not verified as an actual duplicate of the kept file -- skipped rather than risk it",
                        item.path.display()
                    ));
                    continue;
                }
                Err(e) => {
                    report.failures.push(format!("{}: could not verify against kept file: {}", item.path.display(), e));
                    continue;
                }
            }
        }

        match &kind {
            ActionKind::Trash => {
                if dry_run {
                    tally(&mut report, &item);
                    continue;
                }
                match trash::delete(&item.path) {
                    Ok(()) => {
                        report.succeeded_paths.push(item.path.clone());
                        tally(&mut report, &item);
                    }
                    Err(e) => report.failures.push(format!("{}: {}", item.path.display(), e)),
                }
            }
            ActionKind::MoveTo(dest_root) => {
                let dest = crate::cli::mirrored_path(&item.path, dest_root);
                if dest.exists() {
                    report.failures.push(format!("{}: destination {} already exists -- skipped", item.path.display(), dest.display()));
                    continue;
                }
                if dry_run {
                    tally(&mut report, &item);
                    continue;
                }
                match crate::cli::move_path(&item.path, &dest) {
                    Ok(()) => {
                        report.succeeded_paths.push(item.path.clone());
                        tally(&mut report, &item);
                    }
                    Err(e) => report.failures.push(format!("{}: {}", item.path.display(), e)),
                }
            }
        }
    }

    report
}

fn tally(report: &mut ActionReport, item: &ActionItem) {
    if item.is_folder {
        report.moved_folders += 1;
    } else {
        report.moved_files += 1;
    }
}

/// Opens a native "choose a folder" dialog and returns the chosen path, or
/// `None` if the user cancelled. Run as an async `Task` from the UI thread.
pub async fn pick_folder(title: &str) -> Option<PathBuf> {
    rfd::AsyncFileDialog::new().set_title(title).pick_folder().await.map(|h| h.path().to_path_buf())
}
