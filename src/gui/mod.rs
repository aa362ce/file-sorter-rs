mod style;
mod view;
mod worker;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use iced::widget::container;
use iced::{Element, Fill, Subscription, Task, Theme};

use crate::dedupe::{self, ScanOptions};
use crate::store;
use worker::{ActionHandle, ActionItem, ActionKind, ActionReport, ScanHandle, ScanSummary};

pub fn run() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("File Sorter")
        .theme(|_state: &App| Theme::Dark)
        .style(|_state: &App, theme: &Theme| style::app_background(theme))
        .subscription(App::subscription)
        .window_size((1180.0, 820.0))
        .centered()
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Scan,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultsView {
    Files,
    Folders,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Scanning,
    Reviewing,
    Acting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    Trash,
}

pub struct App {
    pub tab: Tab,

    // -- scan configuration --
    pub directories: Vec<PathBuf>,
    pub dir_input: String,
    pub min_size_mb: String,
    pub threads: String,
    pub large_threshold_mb: String,
    pub exclude_input: String,
    pub excludes: Vec<String>,
    pub use_default_excludes: bool,
    pub exclude_temp_files: bool,
    pub file_type_filter: HashSet<String>,
    pub dry_run: bool,

    // -- scan lifecycle --
    pub phase: Phase,
    pub scan_handle: Option<ScanHandle>,
    pub scan_started: Option<Instant>,
    pub resumable_run: Option<String>,
    pub last_summary: Option<ScanSummary>,
    pub error: Option<String>,

    // -- results / selection --
    pub results_view: ResultsView,
    pub file_removed: HashSet<(usize, usize)>,
    pub folder_removed: HashSet<(usize, usize)>,

    // -- actions --
    pub action_handle: Option<ActionHandle>,
    pub confirm: Option<PendingAction>,
    pub last_action_report: Option<ActionReport>,

    // -- history --
    pub history: Vec<store::RunRecord>,
    pub resumable_ids: HashSet<String>,
    pub history_error: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        App {
            tab: Tab::Scan,
            directories: Vec::new(),
            dir_input: String::new(),
            min_size_mb: "0".to_string(),
            threads: "0".to_string(),
            large_threshold_mb: "500".to_string(),
            exclude_input: String::new(),
            excludes: Vec::new(),
            use_default_excludes: true,
            exclude_temp_files: true,
            file_type_filter: HashSet::new(),
            dry_run: false,
            phase: Phase::Idle,
            scan_handle: None,
            scan_started: None,
            resumable_run: None,
            last_summary: None,
            error: None,
            results_view: ResultsView::Files,
            file_removed: HashSet::new(),
            folder_removed: HashSet::new(),
            action_handle: None,
            confirm: None,
            last_action_report: None,
            history: Vec::new(),
            resumable_ids: HashSet::new(),
            history_error: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    TabSelected(Tab),

    DirInputChanged(String),
    AddTypedDirectory,
    PickDirectory,
    DirectoryPicked(Option<PathBuf>),
    RemoveDirectory(usize),

    MinSizeChanged(String),
    ThreadsChanged(String),
    LargeThresholdChanged(String),
    ExcludeInputChanged(String),
    AddExclude,
    RemoveExclude(usize),
    ToggleDefaultExcludes(bool),
    ToggleExcludeTempFiles(bool),
    ToggleFileType(String, bool),
    ToggleDryRun(bool),

    StartScan,
    ResumeScan,
    CancelScan,
    Tick(Instant),

    ResultsViewChanged(ResultsView),
    ToggleFileRemoved(usize, usize, bool),
    ToggleFolderRemoved(usize, usize, bool),
    SelectAllFiles,
    SelectNoneFiles,
    SelectAllFolders,
    SelectNoneFolders,

    RequestTrash,
    RequestMoveTo,
    MoveDestPicked(Option<PathBuf>),
    ConfirmAction,
    CancelConfirm,

    LoadHistoryRun(usize),
    DismissError,
}

impl App {
    fn boot() -> (App, Task<Message>) {
        let mut app = App::default();
        app.refresh_history();
        (app, Task::none())
    }

    fn subscription(&self) -> Subscription<Message> {
        if matches!(self.phase, Phase::Scanning | Phase::Acting) {
            iced::time::every(Duration::from_millis(150)).map(Message::Tick)
        } else {
            Subscription::none()
        }
    }

    fn refresh_history(&mut self) {
        match store::load_history() {
            Ok(mut records) => {
                records.reverse();
                self.history = records;
                self.history_error = None;
            }
            Err(e) => self.history_error = Some(format!("{:#}", e)),
        }
        self.resumable_ids = store::resumable_run_ids().unwrap_or_default();
        self.resumable_run = store::latest_resume_run_id().unwrap_or(None);
    }

    fn build_scan_options(&self, large_threshold_bytes: u64) -> ScanOptions {
        let mut exclude_dirs: HashSet<String> = if self.use_default_excludes {
            dedupe::DEFAULT_EXCLUDED_DIR_NAMES.iter().map(|s| s.to_lowercase()).collect()
        } else {
            HashSet::new()
        };
        exclude_dirs.extend(self.excludes.iter().map(|s| s.to_lowercase()));

        ScanOptions {
            show_progress: false,
            workers: self.threads.trim().parse::<usize>().unwrap_or(0),
            large_file_threshold: large_threshold_bytes,
            exclude_dirs,
            exclude_temp_files: self.exclude_temp_files,
            file_types: if self.file_type_filter.is_empty() { None } else { Some(self.file_type_filter.clone()) },
        }
    }

    fn default_selection(&mut self) {
        self.file_removed.clear();
        self.folder_removed.clear();
        if let Some(summary) = &self.last_summary {
            for (gi, g) in summary.groups.iter().enumerate() {
                for pi in 1..g.paths.len() {
                    self.file_removed.insert((gi, pi));
                }
            }
            for (gi, g) in summary.folder_groups.iter().enumerate() {
                if !g.confirmed {
                    continue;
                }
                for pi in 1..g.paths.len() {
                    self.folder_removed.insert((gi, pi));
                }
            }
        }
    }

    fn apply_scan_summary(&mut self, summary: ScanSummary) {
        self.directories = summary.directories.clone();
        self.last_summary = Some(summary);
        self.default_selection();
        self.phase = Phase::Reviewing;
        self.tab = Tab::Scan;
        self.last_action_report = None;
    }

    /// Gathers the currently checked files/folders into action items,
    /// dropping any file that lives under a folder that's also selected for
    /// bulk removal so it's never touched twice.
    fn collect_selected_items(&self) -> Vec<ActionItem> {
        let Some(summary) = &self.last_summary else { return Vec::new() };
        let mut items = Vec::new();
        let mut folder_roots: Vec<PathBuf> = Vec::new();

        for (gi, g) in summary.folder_groups.iter().enumerate() {
            if !g.confirmed {
                continue;
            }
            for (pi, path) in g.paths.iter().enumerate() {
                if pi == 0 || !self.folder_removed.contains(&(gi, pi)) {
                    continue;
                }
                folder_roots.push(path.clone());
                items.push(ActionItem { path: path.clone(), is_folder: true, needs_verify: false, reference: PathBuf::new() });
            }
        }

        for (gi, g) in summary.groups.iter().enumerate() {
            let reference = g.paths[0].clone();
            for (pi, path) in g.paths.iter().enumerate() {
                if pi == 0 || !self.file_removed.contains(&(gi, pi)) {
                    continue;
                }
                if folder_roots.iter().any(|r| path.starts_with(r)) {
                    continue;
                }
                items.push(ActionItem { path: path.clone(), is_folder: false, needs_verify: !g.confirmed, reference: reference.clone() });
            }
        }

        items
    }

    fn prune_after_action(&mut self, report: &ActionReport) {
        if report.dry_run || report.succeeded_paths.is_empty() {
            return;
        }
        let removed: HashSet<PathBuf> = report.succeeded_paths.iter().cloned().collect();
        if let Some(summary) = &mut self.last_summary {
            for g in summary.groups.iter_mut() {
                g.paths.retain(|p| !removed.contains(p));
            }
            summary.groups.retain(|g| g.paths.len() >= 2);
            for g in summary.folder_groups.iter_mut() {
                g.paths.retain(|p| !removed.contains(p));
            }
            summary.folder_groups.retain(|g| g.paths.len() >= 2);
        }
        self.default_selection();
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TabSelected(tab) => {
                self.tab = tab;
                if tab == Tab::History {
                    self.refresh_history();
                }
                Task::none()
            }

            Message::DirInputChanged(v) => {
                self.dir_input = v;
                Task::none()
            }
            Message::AddTypedDirectory => {
                let raw = self.dir_input.trim().to_string();
                if !raw.is_empty() {
                    self.add_directory(PathBuf::from(raw));
                }
                self.dir_input.clear();
                Task::none()
            }
            Message::PickDirectory => Task::perform(worker::pick_folder("Choose a folder to scan"), Message::DirectoryPicked),
            Message::DirectoryPicked(path) => {
                if let Some(p) = path {
                    self.add_directory(p);
                }
                Task::none()
            }
            Message::RemoveDirectory(idx) => {
                if idx < self.directories.len() {
                    self.directories.remove(idx);
                }
                Task::none()
            }

            Message::MinSizeChanged(v) => {
                if v.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    self.min_size_mb = v;
                }
                Task::none()
            }
            Message::ThreadsChanged(v) => {
                if v.chars().all(|c| c.is_ascii_digit()) {
                    self.threads = v;
                }
                Task::none()
            }
            Message::LargeThresholdChanged(v) => {
                if v.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    self.large_threshold_mb = v;
                }
                Task::none()
            }
            Message::ExcludeInputChanged(v) => {
                self.exclude_input = v;
                Task::none()
            }
            Message::AddExclude => {
                let raw = self.exclude_input.trim().to_string();
                if !raw.is_empty() && !self.excludes.iter().any(|e| e.eq_ignore_ascii_case(&raw)) {
                    self.excludes.push(raw);
                }
                self.exclude_input.clear();
                Task::none()
            }
            Message::RemoveExclude(idx) => {
                if idx < self.excludes.len() {
                    self.excludes.remove(idx);
                }
                Task::none()
            }
            Message::ToggleDefaultExcludes(v) => {
                self.use_default_excludes = v;
                Task::none()
            }
            Message::ToggleExcludeTempFiles(v) => {
                self.exclude_temp_files = v;
                Task::none()
            }
            Message::ToggleFileType(name, checked) => {
                if checked {
                    self.file_type_filter.insert(name);
                } else {
                    self.file_type_filter.remove(&name);
                }
                Task::none()
            }
            Message::ToggleDryRun(v) => {
                self.dry_run = v;
                Task::none()
            }

            Message::StartScan => {
                if self.directories.is_empty() {
                    self.error = Some("Add at least one directory to scan first.".to_string());
                    return Task::none();
                }
                let min_size = parse_mb(&self.min_size_mb).unwrap_or(0.0);
                let large_threshold = parse_mb(&self.large_threshold_mb).unwrap_or(500.0);
                let opts = self.build_scan_options(mb_to_bytes(large_threshold));
                self.error = None;
                self.last_action_report = None;
                self.phase = Phase::Scanning;
                self.scan_started = Some(Instant::now());
                self.scan_handle = Some(worker::start_scan(self.directories.clone(), opts, mb_to_bytes(min_size), None));
                Task::none()
            }
            Message::ResumeScan => {
                let Some(run_id) = self.resumable_run.clone() else { return Task::none() };
                let min_size = parse_mb(&self.min_size_mb).unwrap_or(0.0);
                let large_threshold = parse_mb(&self.large_threshold_mb).unwrap_or(500.0);
                let opts = self.build_scan_options(mb_to_bytes(large_threshold));
                self.error = None;
                self.last_action_report = None;
                self.phase = Phase::Scanning;
                self.scan_started = Some(Instant::now());
                self.scan_handle = Some(worker::start_scan(Vec::new(), opts, mb_to_bytes(min_size), Some(run_id)));
                Task::none()
            }
            Message::CancelScan => {
                if let Some(handle) = &self.scan_handle {
                    handle.request_cancel();
                }
                Task::none()
            }

            Message::Tick(_) => {
                if self.phase == Phase::Scanning {
                    if let Some(done) = self.scan_handle.as_ref().map(|h| h.is_done()) {
                        if done {
                            let outcome = self.scan_handle.take().and_then(|h| h.take());
                            match outcome {
                                Some(Ok(summary)) => self.apply_scan_summary(summary),
                                Some(Err(e)) => {
                                    self.error = Some(format!("{:#}", e));
                                    self.phase = Phase::Idle;
                                }
                                None => self.phase = Phase::Idle,
                            }
                            self.refresh_history();
                        }
                    }
                }
                if self.phase == Phase::Acting {
                    if let Some(done) = self.action_handle.as_ref().map(|h| h.is_done()) {
                        if done {
                            if let Some(report) = self.action_handle.take().and_then(|h| h.take()) {
                                self.prune_after_action(&report);
                                self.last_action_report = Some(report);
                            }
                            self.phase = Phase::Reviewing;
                        }
                    }
                }
                Task::none()
            }

            Message::ResultsViewChanged(v) => {
                self.results_view = v;
                Task::none()
            }
            Message::ToggleFileRemoved(gi, pi, checked) => {
                if checked {
                    self.file_removed.insert((gi, pi));
                } else {
                    self.file_removed.remove(&(gi, pi));
                }
                Task::none()
            }
            Message::ToggleFolderRemoved(gi, pi, checked) => {
                if checked {
                    self.folder_removed.insert((gi, pi));
                } else {
                    self.folder_removed.remove(&(gi, pi));
                }
                Task::none()
            }
            Message::SelectAllFiles => {
                if let Some(summary) = &self.last_summary {
                    for (gi, g) in summary.groups.iter().enumerate() {
                        for pi in 1..g.paths.len() {
                            self.file_removed.insert((gi, pi));
                        }
                    }
                }
                Task::none()
            }
            Message::SelectNoneFiles => {
                self.file_removed.clear();
                Task::none()
            }
            Message::SelectAllFolders => {
                if let Some(summary) = &self.last_summary {
                    for (gi, g) in summary.folder_groups.iter().enumerate() {
                        if !g.confirmed {
                            continue;
                        }
                        for pi in 1..g.paths.len() {
                            self.folder_removed.insert((gi, pi));
                        }
                    }
                }
                Task::none()
            }
            Message::SelectNoneFolders => {
                self.folder_removed.clear();
                Task::none()
            }

            Message::RequestTrash => {
                self.confirm = Some(PendingAction::Trash);
                Task::none()
            }
            Message::RequestMoveTo => Task::perform(worker::pick_folder("Choose a destination folder"), Message::MoveDestPicked),
            Message::MoveDestPicked(dest) => {
                if let Some(dest) = dest {
                    self.run_action(ActionKind::MoveTo(dest));
                }
                Task::none()
            }
            Message::ConfirmAction => {
                if let Some(PendingAction::Trash) = self.confirm.take() {
                    self.run_action(ActionKind::Trash);
                }
                Task::none()
            }
            Message::CancelConfirm => {
                self.confirm = None;
                Task::none()
            }

            Message::LoadHistoryRun(idx) => {
                self.load_history_run(idx);
                Task::none()
            }

            Message::DismissError => {
                self.error = None;
                Task::none()
            }
        }
    }

    fn run_action(&mut self, kind: ActionKind) {
        let items = self.collect_selected_items();
        if items.is_empty() {
            self.error = Some("Nothing selected to act on.".to_string());
            return;
        }
        self.error = None;
        self.phase = Phase::Acting;
        self.action_handle = Some(worker::start_action(items, kind, self.dry_run));
    }

    fn add_directory(&mut self, path: PathBuf) {
        let resolved = std::path::absolute(&path).unwrap_or(path);
        if !self.directories.iter().any(|d| d == &resolved) {
            self.directories.push(resolved);
        }
    }

    fn load_history_run(&mut self, idx: usize) {
        let Some(record) = self.history.get(idx).cloned() else { return };
        let run_id = format!("{}", record.timestamp);
        match store::load_run_groups(&run_id) {
            Ok(Some((groups, folder_groups))) => {
                let summary = ScanSummary {
                    run_id,
                    directories: record.directories.iter().map(PathBuf::from).collect(),
                    groups,
                    folder_groups,
                    skipped: record.skipped.max(0) as usize,
                    cancelled: record.cancelled,
                    resumable: self.resumable_ids.contains(&format!("{}", record.timestamp)),
                    reused: false,
                    duration_seconds: record.duration_seconds,
                    reclaimable_bytes: record.reclaimable_bytes.max(0) as u64,
                };
                self.apply_scan_summary(summary);
            }
            Ok(None) => self.error = Some("No saved detail for that run -- it may predate detail tracking.".to_string()),
            Err(e) => self.error = Some(format!("{:#}", e)),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        container(view::root(self)).width(Fill).height(Fill).style(|_theme| container::Style::default()).into()
    }
}

fn parse_mb(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return Some(0.0);
    }
    s.parse::<f64>().ok()
}

fn mb_to_bytes(mb: f64) -> u64 {
    (mb * 1024.0 * 1024.0).round().max(0.0) as u64
}
