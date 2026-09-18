use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::dedupe::DuplicateGroup;
use crate::progress::Progress;

/// Two or more directories whose entire recursive contents are byte-for-byte
/// duplicates of each other: the same set of relative file paths, each pair
/// an exact match. `confirmed` is false if any of the underlying file
/// matches was itself unconfirmed (see `DuplicateGroup::confirmed`) -- the
/// same "verify before deleting" caution applies to the folder as a whole.
#[derive(Debug, Clone)]
pub struct FolderGroup {
    pub paths: Vec<PathBuf>,
    pub file_count: usize,
    pub size: u64,
    pub confirmed: bool,
}

fn link(
    start: PathBuf,
    roots: &HashSet<PathBuf>,
    known_dirs: &mut HashSet<PathBuf>,
    dir_subdirs: &mut HashMap<PathBuf, HashSet<PathBuf>>,
) {
    let mut current = start;
    loop {
        if known_dirs.contains(&current) {
            return;
        }
        known_dirs.insert(current.clone());
        if roots.contains(&current) {
            return;
        }
        let parent = match current.parent() {
            Some(p) => p.to_path_buf(),
            None => return,
        };
        if parent == current {
            return;
        }
        dir_subdirs.entry(parent.clone()).or_default().insert(current.clone());
        current = parent;
    }
}

fn mark_covered(start: PathBuf, covered: &mut HashSet<PathBuf>, dir_subdirs: &HashMap<PathBuf, HashSet<PathBuf>>) {
    let mut stack = vec![start];
    while let Some(current) = stack.pop() {
        if covered.contains(&current) {
            continue;
        }
        covered.insert(current.clone());
        if let Some(subs) = dir_subdirs.get(&current) {
            stack.extend(subs.iter().cloned());
        }
    }
}

/// Find directories whose entire recursive file contents exactly match
/// another directory's, built entirely on top of already-computed file-level
/// duplicate groups rather than doing any extra hashing.
///
/// A directory is only a candidate if every file under it (recursively)
/// already has a match somewhere in `groups` -- a directory containing even
/// one file with no duplicate anywhere in the scan can never have a matching
/// sibling. `skipped` (unreadable files) disqualify their directory the same
/// way: its true contents can't be verified.
///
/// Nested duplicates are collapsed: if two directories match, matching
/// subdirectories under them aren't reported separately, since that's
/// already implied by the parent match.
pub fn find_duplicate_folders(
    all_files: &[PathBuf],
    skipped: &[PathBuf],
    groups: &[DuplicateGroup],
    scan_roots: &[PathBuf],
    show_progress: bool,
) -> Vec<FolderGroup> {
    let mut content_id: HashMap<&std::path::Path, (&str, bool, u64)> = HashMap::new();
    for g in groups {
        for p in &g.paths {
            content_id.insert(p.as_path(), (g.file_hash.as_str(), g.confirmed, g.size));
        }
    }

    let mut dir_files: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut dir_subdirs: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    let mut known_dirs: HashSet<PathBuf> = HashSet::new();
    let roots: HashSet<PathBuf> = scan_roots.iter().cloned().collect();

    for path in all_files.iter().chain(skipped.iter()) {
        if let Some(parent) = path.parent() {
            dir_files.entry(parent.to_path_buf()).or_default().push(path.clone());
            link(parent.to_path_buf(), &roots, &mut known_dirs, &mut dir_subdirs);
        }
    }

    // Deepest directories first, so a directory's subdirectories are always
    // already resolved (signature computed, or disqualified) by the time the
    // directory itself is processed.
    let mut ordered: Vec<PathBuf> = known_dirs.into_iter().collect();
    ordered.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

    let mut signature: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut confirmed_by_dir: HashMap<PathBuf, bool> = HashMap::new();
    let mut size_by_dir: HashMap<PathBuf, u64> = HashMap::new();
    let mut count_by_dir: HashMap<PathBuf, usize> = HashMap::new();

    let progress = Progress::new("Analyzing folders", Some(ordered.len() as u64), show_progress);

    for d in &ordered {
        progress.update(1);
        let mut entries: Vec<(&str, String, String)> = Vec::new();
        let mut disqualified = false;
        let mut confirmed = true;
        let mut total_size: u64 = 0;
        let mut total_count: usize = 0;

        if let Some(files) = dir_files.get(d) {
            let mut files_sorted: Vec<&PathBuf> = files.iter().collect();
            files_sorted.sort_by_key(|p| p.file_name().map(|n| n.to_os_string()));
            for f in files_sorted {
                match content_id.get(f.as_path()) {
                    None => {
                        disqualified = true;
                        break;
                    }
                    Some((file_hash, file_confirmed, file_size)) => {
                        entries.push((
                            "F",
                            f.file_name().unwrap().to_string_lossy().to_string(),
                            file_hash.to_string(),
                        ));
                        confirmed = confirmed && *file_confirmed;
                        total_size += file_size;
                        total_count += 1;
                    }
                }
            }
        }

        if !disqualified {
            if let Some(subs) = dir_subdirs.get(d) {
                let mut subs_sorted: Vec<&PathBuf> = subs.iter().collect();
                subs_sorted.sort_by_key(|p| p.file_name().map(|n| n.to_os_string()));
                for sub in subs_sorted {
                    match signature.get(sub) {
                        Some(Some(sub_sig)) => {
                            entries.push(("D", sub.file_name().unwrap().to_string_lossy().to_string(), sub_sig.clone()));
                            confirmed = confirmed && confirmed_by_dir[sub];
                            total_size += size_by_dir[sub];
                            total_count += count_by_dir[sub];
                        }
                        _ => {
                            disqualified = true;
                            break;
                        }
                    }
                }
            }
        }

        if disqualified || total_count == 0 {
            signature.insert(d.clone(), None);
            continue;
        }

        let repr_str = format!("{:?}", entries);
        let mut hasher = Sha256::new();
        hasher.update(repr_str.as_bytes());
        let sig = format!("{:x}", hasher.finalize());
        signature.insert(d.clone(), Some(sig));
        confirmed_by_dir.insert(d.clone(), confirmed);
        size_by_dir.insert(d.clone(), total_size);
        count_by_dir.insert(d.clone(), total_count);
    }
    progress.close();

    let mut sig_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (d, sig) in &signature {
        if let Some(s) = sig {
            sig_groups.entry(s.clone()).or_default().push(d.clone());
        }
    }

    // Shallowest (i.e. largest/outermost) matches first, so once a pair of
    // directories is reported, their matching subdirectories can be skipped.
    let mut candidates: Vec<Vec<PathBuf>> = sig_groups.into_values().filter(|v| v.len() >= 2).collect();
    candidates.sort_by_key(|dirs| dirs.iter().map(|d| d.components().count()).min().unwrap_or(0));

    let mut covered: HashSet<PathBuf> = HashSet::new();
    let mut result: Vec<FolderGroup> = Vec::new();
    for mut dirs in candidates {
        if dirs.iter().any(|d| covered.contains(d)) {
            continue;
        }
        dirs.sort();
        let representative = dirs[0].clone();
        result.push(FolderGroup {
            file_count: count_by_dir[&representative],
            size: size_by_dir[&representative],
            confirmed: dirs.iter().all(|d| confirmed_by_dir[d]),
            paths: dirs.clone(),
        });
        for d in dirs {
            mark_covered(d, &mut covered, &dir_subdirs);
        }
    }

    result.sort_by_key(|g| std::cmp::Reverse(g.size * g.paths.len() as u64));
    result
}
