use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::dedupe::{DuplicateGroup, ScanResult};
use crate::folders::FolderGroup;

const MAX_RESUME_STATES: i64 = 50;
const MAX_HISTORY_ENTRIES: i64 = 200;

fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h);
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        return PathBuf::from(h);
    }
    PathBuf::from(".")
}

fn store_dir() -> PathBuf {
    home_dir().join(".file-sorter-rs")
}

fn db_path() -> PathBuf {
    store_dir().join("file_sorter.db")
}

/// A snapshot of an interrupted scan, saved so it can be picked up again
/// without redoing work already done. `stage` is "scanning" (walking
/// directories), "quick_hash" (partial hashing) or "full_hash" (confirming
/// duplicates by content). See `dedupe::find_duplicates` for how each field
/// is used to resume a given stage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResumeState {
    pub directories: Vec<String>,
    pub stage: String,
    #[serde(default)]
    pub by_size: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub by_partial: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub by_full: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub processed: Vec<String>,
    #[serde(default)]
    pub skipped: Vec<String>,
    #[serde(default)]
    pub completed_dirs: Vec<String>,
    #[serde(default)]
    pub root_keys: HashMap<String, String>,
}

/// What's new since the previous checkpoint of a running scan -- not the
/// full accumulated state, so persisting it costs work proportional to this
/// delta, not to how far into a huge scan it fires.
#[derive(Debug, Clone, Default)]
pub struct CheckpointDelta {
    pub stage: String,
    pub new_entries: Vec<(String, String)>,
    pub new_skipped: Vec<String>,
    pub new_completed_dirs: Vec<String>,
    pub by_size: Option<HashMap<String, Vec<String>>>,
    pub directories: Option<Vec<String>>,
    pub root_keys: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub timestamp: f64,
    #[serde(default)]
    pub directories: Vec<String>,
    #[serde(default)]
    pub groups: i64,
    #[serde(default)]
    pub reclaimable_bytes: i64,
    #[serde(default)]
    pub skipped: i64,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub duration_seconds: f64,
}

fn connect() -> Result<Connection> {
    std::fs::create_dir_all(store_dir())?;
    let conn = Connection::open(db_path())?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=30000;")?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS runs (
            run_id TEXT PRIMARY KEY,
            timestamp REAL NOT NULL,
            directories TEXT NOT NULL,
            groups INTEGER NOT NULL,
            reclaimable_bytes INTEGER NOT NULL,
            skipped INTEGER NOT NULL,
            cancelled INTEGER NOT NULL,
            duration_seconds REAL NOT NULL
        );
        CREATE TABLE IF NOT EXISTS resume_runs (
            run_id TEXT PRIMARY KEY,
            directories TEXT NOT NULL,
            stage TEXT NOT NULL,
            updated_at REAL NOT NULL
        );
        CREATE TABLE IF NOT EXISTS resume_progress (
            run_id TEXT NOT NULL,
            list_name TEXT NOT NULL,
            key TEXT NOT NULL,
            path TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_resume_progress_run ON resume_progress(run_id);
        CREATE TABLE IF NOT EXISTS run_groups (
            run_id TEXT NOT NULL,
            group_idx INTEGER NOT NULL,
            kind TEXT NOT NULL,
            file_hash TEXT,
            file_count INTEGER,
            size INTEGER NOT NULL,
            confirmed INTEGER NOT NULL,
            PRIMARY KEY (run_id, kind, group_idx)
        );
        CREATE TABLE IF NOT EXISTS run_group_paths (
            run_id TEXT NOT NULL,
            group_idx INTEGER NOT NULL,
            kind TEXT NOT NULL,
            seq INTEGER NOT NULL,
            path TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_run_group_paths_run ON run_group_paths(run_id);
        CREATE TABLE IF NOT EXISTS scan_manifests (
            dir_key TEXT PRIMARY KEY,
            run_id TEXT NOT NULL,
            saved_at REAL NOT NULL
        );
        CREATE TABLE IF NOT EXISTS scan_manifest_files (
            dir_key TEXT NOT NULL,
            path TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime REAL NOT NULL,
            PRIMARY KEY (dir_key, path)
        );
        CREATE INDEX IF NOT EXISTS idx_scan_manifest_files_dir ON scan_manifest_files(dir_key);
        "#,
    )?;
    Ok(())
}

// -- resume state ---------------------------------------------------------

fn touch_resume_run(conn: &Connection, run_id: &str, stage: &str, directories: Option<&[String]>) -> Result<()> {
    let now = now_secs();
    if let Some(dirs) = directories {
        let dirs_json = serde_json::to_string(dirs)?;
        conn.execute(
            "INSERT INTO resume_runs (run_id, directories, stage, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(run_id) DO UPDATE SET stage=excluded.stage, updated_at=excluded.updated_at, directories=excluded.directories",
            params![run_id, dirs_json, stage, now],
        )?;
    } else {
        conn.execute(
            "INSERT INTO resume_runs (run_id, directories, stage, updated_at) VALUES (?1, '[]', ?2, ?3)
             ON CONFLICT(run_id) DO UPDATE SET stage=excluded.stage, updated_at=excluded.updated_at",
            params![run_id, stage, now],
        )?;
    }
    Ok(())
}

fn evict_old_resume_runs(conn: &Connection) -> Result<()> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM resume_runs", [], |r| r.get(0))?;
    if count <= MAX_RESUME_STATES {
        return Ok(());
    }
    let mut stmt = conn.prepare("SELECT run_id FROM resume_runs ORDER BY updated_at ASC LIMIT ?1")?;
    let stale: Vec<String> = stmt
        .query_map(params![count - MAX_RESUME_STATES], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    for rid in stale {
        conn.execute("DELETE FROM resume_runs WHERE run_id = ?1", params![rid])?;
        conn.execute("DELETE FROM resume_progress WHERE run_id = ?1", params![rid])?;
    }
    Ok(())
}

fn replace_resume_state(conn: &Connection, run_id: &str, state: &ResumeState) -> Result<()> {
    touch_resume_run(conn, run_id, &state.stage, Some(&state.directories))?;
    conn.execute("DELETE FROM resume_progress WHERE run_id = ?1", params![run_id])?;
    let mut stmt = conn.prepare(
        "INSERT INTO resume_progress (run_id, list_name, key, path) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (key, paths) in &state.by_size {
        for path in paths {
            stmt.execute(params![run_id, "by_size", key, path])?;
        }
    }
    for (key, paths) in &state.by_partial {
        for path in paths {
            stmt.execute(params![run_id, "by_partial", key, path])?;
        }
    }
    for (key, paths) in &state.by_full {
        for path in paths {
            stmt.execute(params![run_id, "by_full", key, path])?;
        }
    }
    for path in &state.skipped {
        stmt.execute(params![run_id, "skipped", "", path])?;
    }
    for dir_key in &state.completed_dirs {
        stmt.execute(params![run_id, "completed_dirs", "", dir_key])?;
    }
    for (directory, key) in &state.root_keys {
        stmt.execute(params![run_id, "root_keys", directory, key])?;
    }
    drop(stmt);
    evict_old_resume_runs(conn)?;
    Ok(())
}

pub fn save_resume_state(run_id: &str, state: &ResumeState) -> Result<()> {
    let mut conn = connect()?;
    let tx = conn.transaction()?;
    replace_resume_state(&tx, run_id, state)?;
    tx.commit()?;
    Ok(())
}

/// Persist `delta` -- what's new since the previous checkpoint -- without
/// touching anything already saved for `run_id`. Errors are swallowed
/// (matching the Python original): a checkpoint write failing shouldn't
/// abort an otherwise-successful scan.
pub fn checkpoint_progress(run_id: &str, delta: &CheckpointDelta) {
    let _ = checkpoint_progress_inner(run_id, delta);
}

fn checkpoint_progress_inner(run_id: &str, delta: &CheckpointDelta) -> Result<()> {
    let mut conn = connect()?;
    let tx = conn.transaction()?;
    touch_resume_run(&tx, run_id, &delta.stage, delta.directories.as_deref())?;

    if let Some(by_size) = &delta.by_size {
        tx.execute(
            "DELETE FROM resume_progress WHERE run_id = ?1 AND list_name = 'by_size'",
            params![run_id],
        )?;
        let mut stmt = tx.prepare("INSERT INTO resume_progress (run_id, list_name, key, path) VALUES (?1, 'by_size', ?2, ?3)")?;
        for (key, paths) in by_size {
            for path in paths {
                stmt.execute(params![run_id, key, path])?;
            }
        }
    }
    if let Some(root_keys) = &delta.root_keys {
        tx.execute(
            "DELETE FROM resume_progress WHERE run_id = ?1 AND list_name = 'root_keys'",
            params![run_id],
        )?;
        let mut stmt = tx.prepare("INSERT INTO resume_progress (run_id, list_name, key, path) VALUES (?1, 'root_keys', ?2, ?3)")?;
        for (directory, key) in root_keys {
            stmt.execute(params![run_id, directory, key])?;
        }
    }
    if !delta.new_entries.is_empty() {
        let list_name = match delta.stage.as_str() {
            "scanning" => "by_size",
            "quick_hash" => "by_partial",
            "full_hash" => "by_full",
            other => other,
        };
        let mut stmt = tx.prepare("INSERT INTO resume_progress (run_id, list_name, key, path) VALUES (?1, ?2, ?3, ?4)")?;
        for (key, path) in &delta.new_entries {
            stmt.execute(params![run_id, list_name, key, path])?;
        }
    }
    if !delta.new_skipped.is_empty() {
        let mut stmt = tx.prepare("INSERT INTO resume_progress (run_id, list_name, key, path) VALUES (?1, 'skipped', '', ?2)")?;
        for path in &delta.new_skipped {
            stmt.execute(params![run_id, path])?;
        }
    }
    if !delta.new_completed_dirs.is_empty() {
        let mut stmt = tx.prepare("INSERT INTO resume_progress (run_id, list_name, key, path) VALUES (?1, 'completed_dirs', '', ?2)")?;
        for dir_key in &delta.new_completed_dirs {
            stmt.execute(params![run_id, dir_key])?;
        }
    }
    evict_old_resume_runs(&tx)?;
    tx.commit()?;
    Ok(())
}

pub fn load_resume_state(run_id: &str) -> Result<Option<ResumeState>> {
    let conn = connect()?;
    let meta: Option<(String, String)> = conn
        .query_row(
            "SELECT directories, stage FROM resume_runs WHERE run_id = ?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let Some((directories_json, stage)) = meta else {
        return Ok(None);
    };

    let mut stmt = conn.prepare("SELECT list_name, key, path FROM resume_progress WHERE run_id = ?1")?;
    let rows = stmt.query_map(params![run_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?;

    let mut by_size: HashMap<String, Vec<String>> = HashMap::new();
    let mut by_partial: HashMap<String, Vec<String>> = HashMap::new();
    let mut by_full: HashMap<String, Vec<String>> = HashMap::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut completed_dirs: Vec<String> = Vec::new();
    let mut root_keys: HashMap<String, String> = HashMap::new();

    for row in rows {
        let (list_name, key, path) = row?;
        match list_name.as_str() {
            "by_size" => by_size.entry(key).or_default().push(path),
            "by_partial" => by_partial.entry(key).or_default().push(path),
            "by_full" => by_full.entry(key).or_default().push(path),
            "skipped" => skipped.push(path),
            "completed_dirs" => completed_dirs.push(path),
            "root_keys" => {
                root_keys.insert(key, path);
            }
            _ => {}
        }
    }

    // `processed` isn't stored directly -- reconstruct it from whichever
    // stage was in progress, same as the Python original.
    let mut processed: HashSet<String> = HashSet::new();
    for paths in by_partial.values() {
        processed.extend(paths.iter().cloned());
    }
    for paths in by_full.values() {
        processed.extend(paths.iter().cloned());
    }
    processed.extend(skipped.iter().cloned());
    let mut processed: Vec<String> = processed.into_iter().collect();
    processed.sort();

    Ok(Some(ResumeState {
        directories: serde_json::from_str(&directories_json).unwrap_or_default(),
        stage,
        by_size,
        by_partial,
        by_full,
        processed,
        skipped,
        completed_dirs,
        root_keys,
    }))
}

pub fn clear_resume_state(run_id: &str) -> Result<()> {
    let conn = connect()?;
    conn.execute("DELETE FROM resume_runs WHERE run_id = ?1", params![run_id])?;
    conn.execute("DELETE FROM resume_progress WHERE run_id = ?1", params![run_id])?;
    Ok(())
}

pub fn resumable_run_ids() -> Result<HashSet<String>> {
    let conn = connect()?;
    let mut stmt = conn.prepare("SELECT run_id FROM resume_runs")?;
    let ids = stmt.query_map([], |r| r.get::<_, String>(0))?.filter_map(|r| r.ok()).collect();
    Ok(ids)
}

pub fn latest_resume_run_id() -> Result<Option<String>> {
    let conn = connect()?;
    conn.query_row("SELECT run_id FROM resume_runs ORDER BY updated_at DESC LIMIT 1", [], |r| r.get(0))
        .map(Some)
        .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e.into()) })
}

// -- run history ------------------------------------------------------------

fn upsert_run(conn: &Connection, run_id: &str, record: &RunRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO runs (run_id, timestamp, directories, groups, reclaimable_bytes, skipped, cancelled, duration_seconds)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(run_id) DO UPDATE SET
            timestamp=excluded.timestamp, directories=excluded.directories, groups=excluded.groups,
            reclaimable_bytes=excluded.reclaimable_bytes, skipped=excluded.skipped,
            cancelled=excluded.cancelled, duration_seconds=excluded.duration_seconds",
        params![
            run_id,
            record.timestamp,
            serde_json::to_string(&record.directories)?,
            record.groups,
            record.reclaimable_bytes,
            record.skipped,
            record.cancelled as i64,
            record.duration_seconds,
        ],
    )?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?;
    if count > MAX_HISTORY_ENTRIES {
        let mut stmt = conn.prepare("SELECT run_id FROM runs ORDER BY timestamp ASC LIMIT ?1")?;
        let stale: Vec<String> = stmt
            .query_map(params![count - MAX_HISTORY_ENTRIES], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        for rid in &stale {
            conn.execute("DELETE FROM runs WHERE run_id = ?1", params![rid])?;
            conn.execute("DELETE FROM run_groups WHERE run_id = ?1", params![rid])?;
            conn.execute("DELETE FROM run_group_paths WHERE run_id = ?1", params![rid])?;
        }
        delete_manifests_for_runs(conn, &stale)?;
    }
    Ok(())
}

fn row_to_run_record(row: &rusqlite::Row) -> rusqlite::Result<RunRecord> {
    Ok(RunRecord {
        timestamp: row.get("timestamp")?,
        directories: serde_json::from_str(&row.get::<_, String>("directories")?).unwrap_or_default(),
        groups: row.get("groups")?,
        reclaimable_bytes: row.get("reclaimable_bytes")?,
        skipped: row.get("skipped")?,
        cancelled: row.get::<_, i64>("cancelled")? != 0,
        duration_seconds: row.get("duration_seconds")?,
    })
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Record a finished (or cancelled) run in history. `run_id` is used as both
/// the row's primary key and its timestamp (`run_id.parse::<f64>()`), so the
/// history entry lines up with whatever was checkpointed under the same id
/// during the run.
pub fn record_run(directories: &[PathBuf], result: &ScanResult, duration_seconds: f64, run_id: &str) -> Result<RunRecord> {
    let reclaimable: i64 = result
        .groups
        .iter()
        .map(|g| g.size as i64 * (g.paths.len() as i64 - 1))
        .sum();
    let timestamp: f64 = run_id.parse().unwrap_or_else(|_| now_secs());
    let record = RunRecord {
        timestamp,
        directories: directories.iter().map(|d| d.to_string_lossy().to_string()).collect(),
        groups: result.groups.len() as i64,
        reclaimable_bytes: reclaimable,
        skipped: result.skipped.len() as i64,
        cancelled: result.cancelled,
        duration_seconds,
    };
    let conn = connect()?;
    upsert_run(&conn, run_id, &record)?;
    Ok(record)
}

pub fn save_run_groups(run_id: &str, result: &ScanResult) -> Result<()> {
    let mut conn = connect()?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM run_groups WHERE run_id = ?1", params![run_id])?;
    tx.execute("DELETE FROM run_group_paths WHERE run_id = ?1", params![run_id])?;
    {
        let mut group_stmt = tx.prepare(
            "INSERT INTO run_groups (run_id, group_idx, kind, file_hash, file_count, size, confirmed) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let mut path_stmt = tx.prepare(
            "INSERT INTO run_group_paths (run_id, group_idx, kind, seq, path) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (idx, group) in result.groups.iter().enumerate() {
            group_stmt.execute(params![run_id, idx as i64, "file", group.file_hash, None::<i64>, group.size as i64, group.confirmed as i64])?;
            for (seq, path) in group.paths.iter().enumerate() {
                path_stmt.execute(params![run_id, idx as i64, "file", seq as i64, path.to_string_lossy().to_string()])?;
            }
        }
        for (idx, fg) in result.folder_groups.iter().enumerate() {
            group_stmt.execute(params![run_id, idx as i64, "folder", None::<String>, fg.file_count as i64, fg.size as i64, fg.confirmed as i64])?;
            for (seq, path) in fg.paths.iter().enumerate() {
                path_stmt.execute(params![run_id, idx as i64, "folder", seq as i64, path.to_string_lossy().to_string()])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn load_run_groups(run_id: &str) -> Result<Option<(Vec<DuplicateGroup>, Vec<FolderGroup>)>> {
    let conn = connect()?;
    let mut group_stmt = conn.prepare(
        "SELECT group_idx, kind, file_hash, file_count, size, confirmed FROM run_groups WHERE run_id = ?1 ORDER BY kind, group_idx",
    )?;
    struct GroupRow {
        idx: i64,
        kind: String,
        file_hash: Option<String>,
        file_count: Option<i64>,
        size: i64,
        confirmed: bool,
    }
    let group_rows: Vec<GroupRow> = group_stmt
        .query_map(params![run_id], |r| {
            Ok(GroupRow {
                idx: r.get(0)?,
                kind: r.get(1)?,
                file_hash: r.get(2)?,
                file_count: r.get(3)?,
                size: r.get(4)?,
                confirmed: r.get::<_, i64>(5)? != 0,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    if group_rows.is_empty() {
        return Ok(None);
    }

    let mut path_stmt = conn.prepare(
        "SELECT group_idx, kind, path FROM run_group_paths WHERE run_id = ?1 ORDER BY kind, group_idx, seq",
    )?;
    let mut paths_by_key: HashMap<(String, i64), Vec<PathBuf>> = HashMap::new();
    let path_rows = path_stmt.query_map(params![run_id], |r| {
        Ok((r.get::<_, String>(1)?, r.get::<_, i64>(0)?, r.get::<_, String>(2)?))
    })?;
    for row in path_rows {
        let (kind, idx, path) = row?;
        paths_by_key.entry((kind, idx)).or_default().push(PathBuf::from(path));
    }

    let mut groups = Vec::new();
    let mut folder_groups = Vec::new();
    for row in group_rows {
        let paths = paths_by_key.remove(&(row.kind.clone(), row.idx)).unwrap_or_default();
        if row.kind == "file" {
            groups.push(DuplicateGroup {
                file_hash: row.file_hash.unwrap_or_default(),
                size: row.size as u64,
                paths,
                confirmed: row.confirmed,
            });
        } else {
            folder_groups.push(FolderGroup {
                paths,
                file_count: row.file_count.unwrap_or(0) as usize,
                size: row.size as u64,
                confirmed: row.confirmed,
            });
        }
    }
    Ok(Some((groups, folder_groups)))
}

pub fn load_history() -> Result<Vec<RunRecord>> {
    let conn = connect()?;
    let mut stmt = conn.prepare("SELECT * FROM runs ORDER BY timestamp ASC")?;
    let rows = stmt.query_map([], row_to_run_record)?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

pub fn export_history(path: &Path) -> Result<usize> {
    let records = load_history()?;
    std::fs::write(path, serde_json::to_string_pretty(&records)?)?;
    Ok(records.len())
}

pub fn import_history(path: &Path) -> Result<usize> {
    let raw = std::fs::read_to_string(path)?;
    let incoming: Vec<RunRecord> = serde_json::from_str(&raw)?;

    let existing = load_history()?;
    let mut seen: HashSet<(String, String)> = existing
        .iter()
        .map(|r| (format!("{}", r.timestamp), r.directories.join("\u{0}")))
        .collect();

    let mut added = 0usize;
    let conn = connect()?;
    for record in incoming {
        let key = (format!("{}", record.timestamp), record.directories.join("\u{0}"));
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key);
        upsert_run(&conn, &record.timestamp.to_string(), &record)?;
        added += 1;
    }
    Ok(added)
}

fn delete_manifests_for_runs(conn: &Connection, run_ids: &[String]) -> Result<()> {
    if run_ids.is_empty() {
        return Ok(());
    }
    for rid in run_ids {
        let mut stmt = conn.prepare("SELECT dir_key FROM scan_manifests WHERE run_id = ?1")?;
        let dir_keys: Vec<String> = stmt.query_map(params![rid], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
        conn.execute("DELETE FROM scan_manifests WHERE run_id = ?1", params![rid])?;
        for dk in dir_keys {
            conn.execute("DELETE FROM scan_manifest_files WHERE dir_key = ?1", params![dk])?;
        }
    }
    Ok(())
}

// -- scan manifests -----------------------------------------------------

fn dir_key_for(directories: &[String]) -> String {
    let mut sorted: Vec<String> = directories.to_vec();
    sorted.sort();
    serde_json::to_string(&sorted).unwrap_or_default()
}

pub fn save_scan_manifest(directories: &[String], run_id: &str, manifest: &HashMap<String, (u64, f64)>) -> Result<()> {
    let dir_key = dir_key_for(directories);
    let mut conn = connect()?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM scan_manifest_files WHERE dir_key = ?1", params![dir_key])?;
    tx.execute(
        "INSERT INTO scan_manifests (dir_key, run_id, saved_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(dir_key) DO UPDATE SET run_id=excluded.run_id, saved_at=excluded.saved_at",
        params![dir_key, run_id, now_secs()],
    )?;
    {
        let mut stmt = tx.prepare("INSERT INTO scan_manifest_files (dir_key, path, size, mtime) VALUES (?1, ?2, ?3, ?4)")?;
        for (path, (size, mtime)) in manifest {
            stmt.execute(params![dir_key, path, *size as i64, *mtime])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn find_reusable_run(directories: &[String], manifest: &HashMap<String, (u64, f64)>) -> Result<Option<String>> {
    let dir_key = dir_key_for(directories);
    let conn = connect()?;
    let run_id: Option<String> = conn
        .query_row("SELECT run_id FROM scan_manifests WHERE dir_key = ?1", params![dir_key], |r| r.get(0))
        .ok();
    let Some(run_id) = run_id else {
        return Ok(None);
    };

    let mut stmt = conn.prepare("SELECT path, size, mtime FROM scan_manifest_files WHERE dir_key = ?1")?;
    struct Row {
        path: String,
        size: i64,
        mtime: f64,
    }
    let rows: Vec<Row> = stmt
        .query_map(params![dir_key], |r| {
            Ok(Row { path: r.get(0)?, size: r.get(1)?, mtime: r.get(2)? })
        })?
        .filter_map(|r| r.ok())
        .collect();

    if rows.len() != manifest.len() {
        return Ok(None);
    }
    for row in rows {
        match manifest.get(&row.path) {
            Some((size, mtime)) if *size as i64 == row.size && (*mtime - row.mtime).abs() < f64::EPSILON => {}
            _ => return Ok(None),
        }
    }
    Ok(Some(run_id))
}
