//! file-dedupe core library.
//!
//! Two pieces matter here:
//!   * [`find_duplicates`] — walk the given roots, bucket candidate files by
//!     size, then (cheaply) by a 4 KiB pre-hash, then confirm with a full
//!     blake3 hash. Only files that share a full content hash end up in a
//!     [`DupGroup`].
//!   * [`plan_clean`] — turns dup groups + a keep policy
//!     into a list of [`Action`]s, refusing overlapping file identities.
//!     [`validate_actions`] checks the entire batch before mutation.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use walkdir::WalkDir;

/// Size of the cheap pre-hash window. We hash at most this many bytes from the
/// front of each file to split a same-size bucket before paying for a full read.
const PREHASH_BYTES: u64 = 4096;

/// Options controlling a [`find_duplicates`] scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Files strictly smaller than this many bytes are ignored entirely.
    pub min_size: u64,
    /// Recurse into subdirectories. When false, only the immediate entries of
    /// each root directory are considered.
    pub recursive: bool,
    /// Follow symlinks while walking. Off by default to avoid escaping the
    /// intended tree or hashing the same inode twice.
    pub follow_symlinks: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            min_size: 0,
            recursive: true,
            follow_symlinks: false,
        }
    }
}

/// Per-file metadata captured during the walk, carried through hashing so the
/// keep policies (newest/oldest) have what they need without re-stat-ing.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    /// Modification time, if the platform/file provided one.
    pub modified: Option<SystemTime>,
}

/// A set of two or more files that share identical content.
#[derive(Debug, Clone, Serialize)]
pub struct DupGroup {
    /// Hex blake3 hash shared by every member.
    pub hash: String,
    /// Common size in bytes of every member.
    pub size: u64,
    /// Member file paths. Always length >= 2. Sorted for deterministic output.
    pub paths: Vec<PathBuf>,
}

impl DupGroup {
    /// Bytes that could be reclaimed by collapsing this group to a single copy:
    /// `size * (count - 1)`.
    pub fn reclaimable_bytes(&self) -> u64 {
        self.size * (self.paths.len() as u64 - 1)
    }
}

/// Policy for choosing which file in a group to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepPolicy {
    /// Keep the path that sorts first lexicographically (stable, no stat needed).
    First,
    /// Keep the most recently modified file.
    Newest,
    /// Keep the least recently modified file.
    Oldest,
}

/// What [`plan_clean`] decides should happen to a single file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Action {
    /// Keep this file untouched. Exactly one per group.
    Keep { path: PathBuf },
    /// Delete this duplicate. `reclaim` is the bytes freed.
    Delete { path: PathBuf, reclaim: u64 },
    /// Replace this duplicate with a hardlink to `keep`. `reclaim` is the bytes
    /// freed (the data block becomes shared).
    Hardlink {
        path: PathBuf,
        keep: PathBuf,
        reclaim: u64,
    },
}

impl Action {
    /// The file this action targets.
    pub fn path(&self) -> &Path {
        match self {
            Action::Keep { path } | Action::Delete { path, .. } | Action::Hardlink { path, .. } => {
                path
            }
        }
    }
}

/// How duplicates should be removed when planning a clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanMode {
    /// Delete the duplicate files.
    Delete,
    /// Replace duplicates with hardlinks to the kept file.
    Hardlink,
}

/// Walk `roots`, find groups of byte-identical files, and return them sorted.
///
/// Algorithm:
///   1. Collect every regular file (honoring recursion / symlink / min-size
///      options) keyed by exact byte size. Sizes with a single file can never
///      have a duplicate and are dropped immediately.
///   2. Within each surviving size bucket, split by a cheap front-of-file
///      pre-hash (first [`PREHASH_BYTES`] bytes). Sub-buckets that fall to a
///      single file are dropped.
///   3. Within each surviving sub-bucket, compute the full blake3 hash and
///      group by it. Any full-hash group with >= 2 members is a real duplicate
///      group.
///
/// Files that cannot be read (permissions, races) are skipped rather than
/// aborting the whole scan.
pub fn find_duplicates(roots: &[PathBuf], opts: &ScanOptions) -> Vec<DupGroup> {
    // --- Step 1: bucket by size -------------------------------------------
    let mut by_size: HashMap<u64, Vec<FileEntry>> = HashMap::new();
    let mut seen = HashSet::new();
    for root in roots {
        for mut entry in collect_files(root, opts) {
            if entry.size < opts.min_size {
                continue;
            }
            // Roots can overlap or refer to the same directory through aliases.
            // Canonical paths also keep followed symlink aliases out of actions.
            let path = match fs::canonicalize(&entry.path) {
                Ok(path) => path,
                Err(_) => continue,
            };
            let identity = match file_identity(&path) {
                Ok(identity) => identity,
                Err(_) => continue,
            };
            if !seen.insert(identity) {
                continue;
            }
            entry.path = path;
            by_size.entry(entry.size).or_default().push(entry);
        }
    }

    let mut groups: Vec<DupGroup> = Vec::new();

    for (size, entries) in by_size {
        if entries.len() < 2 {
            continue; // unique size => cannot be a duplicate
        }

        // --- Step 2: split bucket by cheap pre-hash -----------------------
        // For files smaller than the pre-hash window the pre-hash already
        // covers the whole file, but we still confirm with a full hash for
        // uniformity (and because two files can share a prefix but differ
        // later).
        let mut by_prehash: HashMap<[u8; 32], Vec<FileEntry>> = HashMap::new();
        for entry in entries {
            match prehash(&entry.path) {
                Ok(h) => by_prehash.entry(h).or_default().push(entry),
                Err(_) => { /* unreadable: skip this file */ }
            }
        }

        for (_ph, candidates) in by_prehash {
            if candidates.len() < 2 {
                continue;
            }

            // --- Step 3: confirm with full hash ---------------------------
            let mut by_hash: HashMap<String, Vec<FileEntry>> = HashMap::new();
            for entry in candidates {
                match full_hash(&entry.path) {
                    Ok(h) => by_hash.entry(h).or_default().push(entry),
                    Err(_) => { /* unreadable: skip */ }
                }
            }

            for (hash, members) in by_hash {
                if members.len() < 2 {
                    continue;
                }
                let mut paths: Vec<PathBuf> = members.into_iter().map(|e| e.path).collect();
                paths.sort();
                groups.push(DupGroup { hash, size, paths });
            }
        }
    }

    // Deterministic ordering: largest reclaim first, then by hash for ties.
    groups.sort_by(|a, b| {
        b.reclaimable_bytes()
            .cmp(&a.reclaimable_bytes())
            .then_with(|| a.hash.cmp(&b.hash))
    });
    groups
}

/// Total bytes reclaimable across all groups (sum of every duplicate copy
/// beyond the first in each group).
pub fn total_reclaimable(groups: &[DupGroup]) -> u64 {
    groups.iter().map(DupGroup::reclaimable_bytes).sum()
}

/// Turn duplicate groups into a flat list of actions given a keep policy and
/// mode. Validates existing file identities and mutates nothing.
///
/// For each group exactly one [`Action::Keep`] is emitted (the keeper chosen by
/// `policy`); every other member becomes a [`Action::Delete`] or
/// [`Action::Hardlink`] depending on `mode`. The keeper is never targeted for
/// removal, and actions never cross a group boundary.
///
/// Returns an error string if a keeper cannot be determined for some group
/// (e.g. a newest/oldest policy on entries lacking mtimes) — callers must
/// refuse to act rather than guess.
pub fn plan_clean(
    groups: &[DupGroup],
    policy: KeepPolicy,
    mode: CleanMode,
) -> Result<Vec<Action>, String> {
    let mut paths = HashSet::new();
    let mut identities = HashSet::new();
    for group in groups {
        for path in &group.paths {
            let normalized = fs::canonicalize(path).unwrap_or_else(|_| {
                path.components()
                    .filter(|c| !matches!(c, Component::CurDir))
                    .collect::<PathBuf>()
            });
            if !paths.insert(normalized) {
                return Err(format!(
                    "repeated path in duplicate groups: {}",
                    path.display()
                ));
            }
            if let Ok(identity) = file_identity(path) {
                if !identities.insert(identity) {
                    return Err(format!(
                        "repeated file identity in duplicate groups: {}",
                        path.display()
                    ));
                }
            }
        }
    }
    let mut actions = Vec::new();
    for group in groups {
        let keeper = choose_keeper(group, policy)?;
        let keep_path = group.paths[keeper].clone();
        for (i, path) in group.paths.iter().enumerate() {
            if i == keeper {
                actions.push(Action::Keep { path: path.clone() });
            } else {
                match mode {
                    CleanMode::Delete => actions.push(Action::Delete {
                        path: path.clone(),
                        reclaim: group.size,
                    }),
                    CleanMode::Hardlink => actions.push(Action::Hardlink {
                        path: path.clone(),
                        keep: keep_path.clone(),
                        reclaim: group.size,
                    }),
                }
            }
        }
    }
    Ok(actions)
}

/// Refuse overlapping targets or keeper identities before applying any action.
/// Paths must still be regular files; symlink replacements are refused.
pub fn validate_actions(actions: &[Action]) -> io::Result<()> {
    let mut identities = HashSet::new();
    let mut keepers = HashSet::new();
    for action in actions {
        regular_file(action.path())?;
        let identity = file_identity(action.path())?;
        if !identities.insert(identity.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "repeated action target or keeper identity: {}",
                    action.path().display()
                ),
            ));
        }
        if matches!(action, Action::Keep { .. }) {
            keepers.insert(identity);
        }
    }
    for action in actions {
        if let Action::Hardlink { keep, .. } = action {
            if !keepers.contains(&file_identity(keep)?) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "hardlink keeper is not protected by this plan: {}",
                        keep.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn regular_file(path: &Path) -> io::Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing non-regular file: {}", path.display()),
        ));
    }
    Ok(meta)
}

/// Pick the index into `group.paths` of the file to keep under `policy`.
fn choose_keeper(group: &DupGroup, policy: KeepPolicy) -> Result<usize, String> {
    if group.paths.is_empty() {
        return Err("encountered an empty duplicate group".to_string());
    }
    match policy {
        // paths are pre-sorted, so index 0 is the lexicographically first.
        KeepPolicy::First => Ok(0),
        KeepPolicy::Newest | KeepPolicy::Oldest => {
            // We need mtimes here. Re-stat lazily so plan_clean stays usable
            // even when DupGroup was built without carrying times.
            let mut best_idx: Option<usize> = None;
            let mut best_time: Option<SystemTime> = None;
            for (i, p) in group.paths.iter().enumerate() {
                let mtime = fs::metadata(p).and_then(|m| m.modified()).map_err(|e| {
                    format!(
                        "cannot read modification time of {} (needed for keep policy): {}",
                        p.display(),
                        e
                    )
                })?;
                let take = match (best_time, policy) {
                    (None, _) => true,
                    (Some(bt), KeepPolicy::Newest) => mtime > bt,
                    (Some(bt), KeepPolicy::Oldest) => mtime < bt,
                    _ => unreachable!(),
                };
                if take {
                    best_time = Some(mtime);
                    best_idx = Some(i);
                }
            }
            best_idx.ok_or_else(|| "could not determine a keeper for a group".to_string())
        }
    }
}

/// Apply a single planned action to the filesystem. `Keep` is a no-op.
///
/// Hardlinking is done safely: link the kept file to a temp name beside the
/// duplicate, then atomically rename it over the duplicate. That way a crash
/// never leaves the path missing. Returns bytes reclaimed by this action.
pub fn apply_action(action: &Action) -> io::Result<u64> {
    match action {
        Action::Keep { .. } => Ok(0),
        Action::Delete { path, reclaim } => {
            regular_file(path)?;
            fs::remove_file(path)?;
            Ok(*reclaim)
        }
        Action::Hardlink {
            path,
            keep,
            reclaim,
        } => {
            regular_file(path)?;
            regular_file(keep)?;
            // Skip if already the same inode (idempotent / nothing to do).
            if same_inode(path, keep)? {
                return Ok(0);
            }
            let original_identity = file_identity(path)?;
            let keeper_identity = file_identity(keep)?;
            if full_hash(path)? != full_hash(keep)? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("refusing to replace changed duplicate: {}", path.display()),
                ));
            }
            // Exclusively reserve a private directory; existing staging names
            // belong to somebody else and must never be removed.
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let staging = tempfile::Builder::new()
                .prefix(".file-dedupe-")
                .tempdir_in(parent)?;
            let tmp = staging.path().join("link");
            fs::hard_link(keep, &tmp)?;
            regular_file(path)?;
            regular_file(keep)?;
            if file_identity(path)? != original_identity
                || file_identity(keep)? != keeper_identity
                || full_hash(path)? != full_hash(&tmp)?
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("file identity changed while staging: {}", path.display()),
                ));
            }
            // Atomic replace.
            fs::rename(&tmp, path)?;
            Ok(*reclaim)
        }
    }
}

/// Collect regular files under `root` per the scan options.
fn collect_files(root: &Path, opts: &ScanOptions) -> Vec<FileEntry> {
    let mut walker = WalkDir::new(root).follow_links(opts.follow_symlinks);
    if !opts.recursive {
        // depth 0 = root itself, depth 1 = immediate children.
        walker = walker.max_depth(1);
    }
    let mut out = Vec::new();
    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        // Only regular files. When not following symlinks, file_type() reports
        // the link itself (is_file() == false), so symlinks are naturally
        // excluded unless follow_symlinks turned them into their targets.
        if !entry.file_type().is_file() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(FileEntry {
            path: entry.path().to_path_buf(),
            size: meta.len(),
            modified: meta.modified().ok(),
        });
    }
    out
}

/// Hash the first [`PREHASH_BYTES`] bytes of a file (or the whole file if it is
/// shorter). Returns a fixed 32-byte blake3 digest.
fn prehash(path: &Path) -> io::Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; PREHASH_BYTES as usize];
    let mut filled = 0usize;
    // Read up to the window; short reads are fine.
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&buf[..filled]);
    Ok(*hasher.finalize().as_bytes())
}

/// Compute the full blake3 hash of a file, returned as a lowercase hex string.
fn full_hash(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(unix)]
type FileIdentity = (u64, u64);

#[cfg(unix)]
fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path)?;
    Ok((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
type FileIdentity = PathBuf;

#[cfg(not(unix))]
fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    fs::canonicalize(path)
}

#[cfg(unix)]
fn same_inode(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(file_identity(a)? == file_identity(b)?)
}

#[cfg(not(unix))]
fn same_inode(a: &Path, b: &Path) -> io::Result<bool> {
    // Portable fallback protects aliases of the same path. Distinct hardlink
    // identities are only detected by the Unix implementation above.
    Ok(fs::canonicalize(a)? == fs::canonicalize(b)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(hash: &str, size: u64, paths: &[&str]) -> DupGroup {
        DupGroup {
            hash: hash.to_string(),
            size,
            paths: paths.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn batch_validation_refuses_keeper_deletion_before_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let keeper = dir.path().join("keeper");
        fs::write(&keeper, b"only copy").unwrap();
        let actions = vec![
            Action::Keep {
                path: keeper.clone(),
            },
            Action::Delete {
                path: dir.path().join("./keeper"),
                reclaim: 9,
            },
        ];
        assert_eq!(
            validate_actions(&actions).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(&keeper).unwrap(), b"only copy");
    }

    #[test]
    fn reclaimable_bytes_math() {
        let g = group("h", 100, &["a", "b", "c"]);
        assert_eq!(g.reclaimable_bytes(), 200); // keep one, free two copies
        let g2 = group("h", 50, &["a", "b"]);
        assert_eq!(g2.reclaimable_bytes(), 50);
        assert_eq!(total_reclaimable(&[g, g2]), 250);
    }

    #[test]
    fn plan_clean_keep_first_deletes_rest() {
        let g = group("h", 10, &["a.txt", "b.txt", "c.txt"]);
        let actions = plan_clean(&[g], KeepPolicy::First, CleanMode::Delete).unwrap();
        assert_eq!(actions.len(), 3);
        // Sorted paths => "a.txt" kept.
        assert_eq!(
            actions[0],
            Action::Keep {
                path: PathBuf::from("a.txt")
            }
        );
        assert_eq!(
            actions[1],
            Action::Delete {
                path: PathBuf::from("b.txt"),
                reclaim: 10
            }
        );
        assert_eq!(
            actions[2],
            Action::Delete {
                path: PathBuf::from("c.txt"),
                reclaim: 10
            }
        );
    }

    #[test]
    fn plan_clean_hardlink_mode_points_at_keeper() {
        let g = group("h", 10, &["a.txt", "b.txt"]);
        let actions = plan_clean(&[g], KeepPolicy::First, CleanMode::Hardlink).unwrap();
        assert_eq!(
            actions[0],
            Action::Keep {
                path: PathBuf::from("a.txt")
            }
        );
        assert_eq!(
            actions[1],
            Action::Hardlink {
                path: PathBuf::from("b.txt"),
                keep: PathBuf::from("a.txt"),
                reclaim: 10
            }
        );
    }

    #[test]
    fn plan_clean_never_targets_keeper() {
        let g = group("h", 7, &["a", "b", "c", "d"]);
        let actions = plan_clean(&[g.clone()], KeepPolicy::First, CleanMode::Delete).unwrap();
        let keepers: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, Action::Keep { .. }))
            .collect();
        assert_eq!(keepers.len(), 1, "exactly one keeper per group");
        // The keeper path must never appear in a Delete.
        let keep_path = keepers[0].path().to_path_buf();
        assert!(actions
            .iter()
            .all(|a| !(matches!(a, Action::Delete { .. }) && a.path() == keep_path)));
    }
}
