//! End-to-end tests over a real temp directory: scan grouping + reclaim math,
//! the dry-run-by-default safety guarantee, keep policies, and hardlinking.

use std::fs;
use std::path::{Path, PathBuf};

use file_dedupe::{
    apply_action, find_duplicates, plan_clean, total_reclaimable, Action, CleanMode, DupGroup,
    KeepPolicy, ScanOptions,
};
use tempfile::TempDir;

/// Build a fixture tree:
///   dup1.txt, dup2.txt, sub/dup3.txt  -> identical "DUPLICATE CONTENT ..." (>min)
///   pairA.bin, pairB.bin              -> a second, different duplicate pair
///   unique1.txt                       -> one-off content
///   unique2.txt                       -> different one-off content
///   tiny.txt                          -> below the min-size threshold we'll use
///   same_size_diff.txt                -> SAME size as the unique files but
///                                        different bytes (exercises size-bucket
///                                        collision without a content match)
///
/// Returns (tempdir, the duplicate content length, the pair content length).
fn make_fixture() -> (TempDir, u64, u64) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // Three identical files (one nested) — the primary duplicate group.
    let dup_body = b"DUPLICATE CONTENT 1234567890 abcdefghij KLMNOPQRSTUVWXYZ\n";
    fs::write(root.join("dup1.txt"), dup_body).unwrap();
    fs::write(root.join("dup2.txt"), dup_body).unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/dup3.txt"), dup_body).unwrap();

    // A second duplicate group of a different size/content.
    let pair_body = b"second group payload -- 0xDEADBEEF -- second group payload\n";
    fs::write(root.join("pairA.bin"), pair_body).unwrap();
    fs::write(root.join("pairB.bin"), pair_body).unwrap();

    // Three DIFFERENT files that all share one length, built so equal length is
    // structural (fixed-length prefix + a distinct fill byte to a fixed total).
    // This puts 3 distinct files in the same size bucket, so a correct
    // implementation must still produce zero groups for that size.
    let unique_len = 64usize;
    let mut u1 = b"unique-A ".to_vec();
    u1.resize(unique_len, b'1');
    let mut u2 = b"unique-B ".to_vec();
    u2.resize(unique_len, b'2');
    let mut u3 = b"unique-C ".to_vec();
    u3.resize(unique_len, b'3');
    assert_eq!(u1.len(), u2.len());
    assert_eq!(u1.len(), u3.len());
    assert!(u1 != u2 && u2 != u3 && u1 != u3);
    fs::write(root.join("unique1.txt"), &u1).unwrap();
    fs::write(root.join("unique2.txt"), &u2).unwrap();
    fs::write(root.join("same_size_diff.txt"), &u3).unwrap();

    // A tiny file we will exclude with --min-size.
    fs::write(root.join("tiny.txt"), b"x").unwrap();

    (dir, dup_body.len() as u64, pair_body.len() as u64)
}

fn opts(min_size: u64) -> ScanOptions {
    ScanOptions {
        min_size,
        recursive: true,
        follow_symlinks: false,
    }
}

/// Find the group (by size) containing a given filename. Panics if absent.
fn group_with<'a>(groups: &'a [DupGroup], name: &str) -> &'a DupGroup {
    groups
        .iter()
        .find(|g| g.paths.iter().any(|p| p.ends_with(name)))
        .unwrap_or_else(|| panic!("no group contained {name}"))
}

#[test]
fn scan_groups_exactly_the_duplicates() {
    let (dir, dup_len, pair_len) = make_fixture();
    let roots = vec![dir.path().to_path_buf()];
    let groups = find_duplicates(&roots, &opts(2)); // min_size 2 excludes tiny.txt (1 byte)

    // Exactly two duplicate groups: the dup-trio and the pair. The three
    // same-length-but-distinct uniques must NOT form a group.
    assert_eq!(
        groups.len(),
        2,
        "expected exactly 2 dup groups, got {}: {:#?}",
        groups.len(),
        groups
    );

    let trio = group_with(&groups, "dup1.txt");
    assert_eq!(trio.paths.len(), 3);
    assert_eq!(trio.size, dup_len);
    // Members are exactly dup1, dup2, sub/dup3.
    let mut names: Vec<String> = trio
        .paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["dup1.txt", "dup2.txt", "dup3.txt"]);

    let pair = group_with(&groups, "pairA.bin");
    assert_eq!(pair.paths.len(), 2);
    assert_eq!(pair.size, pair_len);

    // No group may contain any unique file.
    for forbidden in [
        "unique1.txt",
        "unique2.txt",
        "same_size_diff.txt",
        "tiny.txt",
    ] {
        assert!(
            !groups
                .iter()
                .any(|g| g.paths.iter().any(|p| p.ends_with(forbidden))),
            "{forbidden} should never be in a duplicate group"
        );
    }
}

#[test]
fn scan_computes_correct_reclaimable_bytes() {
    let (dir, dup_len, pair_len) = make_fixture();
    let roots = vec![dir.path().to_path_buf()];
    let groups = find_duplicates(&roots, &opts(2));

    // trio: keep 1, reclaim 2 copies; pair: keep 1, reclaim 1 copy.
    let expected = dup_len * 2 + pair_len * 1;
    assert_eq!(total_reclaimable(&groups), expected);

    let trio = group_with(&groups, "dup1.txt");
    assert_eq!(trio.reclaimable_bytes(), dup_len * 2);
    let pair = group_with(&groups, "pairA.bin");
    assert_eq!(pair.reclaimable_bytes(), pair_len);
}

#[test]
fn min_size_excludes_tiny_files() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    // Two identical 1-byte files: a duplicate, but below the threshold.
    fs::write(root.join("a"), b"z").unwrap();
    fs::write(root.join("b"), b"z").unwrap();
    let roots = vec![root.to_path_buf()];

    // With min_size 2 they are filtered out entirely.
    let groups = find_duplicates(&roots, &opts(2));
    assert!(
        groups.is_empty(),
        "1-byte dups must be excluded by --min-size 2"
    );

    // With min_size 0 they show up.
    let groups0 = find_duplicates(&roots, &opts(0));
    assert_eq!(groups0.len(), 1);
    assert_eq!(groups0[0].paths.len(), 2);
}

#[test]
fn non_recursive_scan_skips_subdirs() {
    let (dir, _dup_len, _pair_len) = make_fixture();
    let roots = vec![dir.path().to_path_buf()];
    let mut o = opts(2);
    o.recursive = false;

    let groups = find_duplicates(&roots, &o);
    // sub/dup3.txt is now invisible, so the trio collapses to a pair (dup1,dup2).
    let trio = group_with(&groups, "dup1.txt");
    assert_eq!(
        trio.paths.len(),
        2,
        "non-recursive scan must not see sub/dup3.txt"
    );
    assert!(!trio.paths.iter().any(|p| p.ends_with("dup3.txt")));
}

#[test]
fn dry_run_default_deletes_nothing() {
    let (dir, _dup_len, _pair_len) = make_fixture();
    let roots = vec![dir.path().to_path_buf()];
    let groups = find_duplicates(&roots, &opts(2));

    // Planning alone must never touch the filesystem.
    let actions = plan_clean(&groups, KeepPolicy::First, CleanMode::Delete).unwrap();
    assert!(!actions.is_empty());

    // Every fixture file is still present after planning (no apply happened).
    for f in [
        "dup1.txt",
        "dup2.txt",
        "sub/dup3.txt",
        "pairA.bin",
        "pairB.bin",
        "unique1.txt",
        "unique2.txt",
    ] {
        assert!(
            dir.path().join(f).exists(),
            "{f} must still exist after a dry-run/plan"
        );
    }
}

#[test]
fn commit_keep_first_removes_right_copies_and_keeps_one() {
    let (dir, _dup_len, _pair_len) = make_fixture();
    let roots = vec![dir.path().to_path_buf()];
    let groups = find_duplicates(&roots, &opts(2));
    let actions = plan_clean(&groups, KeepPolicy::First, CleanMode::Delete).unwrap();

    // Apply for real.
    for a in &actions {
        apply_action(a).unwrap();
    }

    // The trio: exactly one of the three survives, and it's the kept one.
    let trio_paths: Vec<PathBuf> = group_with(&groups, "dup1.txt").paths.clone();
    let survivors: Vec<&PathBuf> = trio_paths.iter().filter(|p| p.exists()).collect();
    assert_eq!(survivors.len(), 1, "exactly one trio copy must remain");

    // keep=first => lexicographically smallest path survives. paths are sorted,
    // so that's index 0.
    assert_eq!(survivors[0], &trio_paths[0]);

    // The pair: exactly one survives.
    let pair_paths: Vec<PathBuf> = group_with(&groups, "pairA.bin").paths.clone();
    let pair_survivors: Vec<&PathBuf> = pair_paths.iter().filter(|p| p.exists()).collect();
    assert_eq!(pair_survivors.len(), 1);

    // Unique files untouched.
    assert!(dir.path().join("unique1.txt").exists());
    assert!(dir.path().join("unique2.txt").exists());
}

#[cfg(unix)]
fn nlink(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().nlink()
}

#[cfg(unix)]
fn inode(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(path).unwrap();
    (m.dev(), m.ino())
}

#[test]
#[cfg(unix)]
fn hardlink_leaves_both_paths_linked() {
    let (dir, _dup_len, _pair_len) = make_fixture();
    let roots = vec![dir.path().to_path_buf()];
    let groups = find_duplicates(&roots, &opts(2));
    let actions = plan_clean(&groups, KeepPolicy::First, CleanMode::Hardlink).unwrap();

    for a in &actions {
        apply_action(a).unwrap();
    }

    // For the trio: all three paths still exist...
    let trio = group_with(&groups, "dup1.txt");
    for p in &trio.paths {
        assert!(p.exists(), "hardlink must preserve path {}", p.display());
    }
    // ...and they all share one inode with nlink >= 3.
    let keeper = &trio.paths[0];
    let keep_ino = inode(keeper);
    for p in &trio.paths {
        assert_eq!(
            inode(p),
            keep_ino,
            "{} should share the keeper's inode",
            p.display()
        );
    }
    assert!(
        nlink(keeper) >= 3,
        "keeper nlink should be >= 3 after linking trio, got {}",
        nlink(keeper)
    );

    // The pair: both paths exist and are linked (nlink >= 2).
    let pair = group_with(&groups, "pairA.bin");
    for p in &pair.paths {
        assert!(p.exists());
    }
    assert!(nlink(&pair.paths[0]) >= 2);
    assert_eq!(inode(&pair.paths[0]), inode(&pair.paths[1]));
}

#[test]
#[cfg(unix)]
fn keep_newest_and_oldest_select_by_mtime() {
    use std::time::{Duration, SystemTime};

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let body = b"mtime policy fixture content padding padding padding\n";
    let old = root.join("old.txt");
    let mid = root.join("mid.txt");
    let new = root.join("new.txt");
    fs::write(&old, body).unwrap();
    fs::write(&mid, body).unwrap();
    fs::write(&new, body).unwrap();

    // Set explicit, well-separated mtimes.
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&old, base);
    set_mtime(&mid, base + Duration::from_secs(100));
    set_mtime(&new, base + Duration::from_secs(200));

    let roots = vec![root.to_path_buf()];
    let groups = find_duplicates(&roots, &opts(0));
    assert_eq!(groups.len(), 1);

    // keep=newest -> new.txt kept (Keep action targets new.txt).
    let newest = plan_clean(&groups, KeepPolicy::Newest, CleanMode::Delete).unwrap();
    let kept_new = newest
        .iter()
        .find(|a| matches!(a, Action::Keep { .. }))
        .unwrap();
    assert!(
        kept_new.path().ends_with("new.txt"),
        "newest policy keeps new.txt"
    );

    // keep=oldest -> old.txt kept.
    let oldest = plan_clean(&groups, KeepPolicy::Oldest, CleanMode::Delete).unwrap();
    let kept_old = oldest
        .iter()
        .find(|a| matches!(a, Action::Keep { .. }))
        .unwrap();
    assert!(
        kept_old.path().ends_with("old.txt"),
        "oldest policy keeps old.txt"
    );
}

#[cfg(unix)]
fn set_mtime(path: &Path, t: std::time::SystemTime) {
    // Use filetime-free approach via libc utimes through std is unstable, so
    // shell out is overkill; instead use the `utime`-style call from std once
    // available. As a portable fallback for the test, re-touch via the
    // `filetime`-less method: open with write to update, then fix. Simplest
    // reliable path: use libc directly.
    use std::os::unix::ffi::OsStrExt;
    let secs = t
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let times = [
        libc_timeval {
            tv_sec: secs,
            tv_usec: 0,
        },
        libc_timeval {
            tv_sec: secs,
            tv_usec: 0,
        },
    ];
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { utimes(c_path.as_ptr(), times.as_ptr()) };
    assert_eq!(rc, 0, "utimes failed for {}", path.display());
}

// Minimal libc binding for utimes so the test can set mtimes without an extra
// crate dependency.
#[cfg(unix)]
#[repr(C)]
struct libc_timeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[cfg(unix)]
extern "C" {
    fn utimes(path: *const std::os::raw::c_char, times: *const libc_timeval)
        -> std::os::raw::c_int;
}

#[test]
fn overlapping_and_aliased_roots_do_not_inflate_groups() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("a.txt"), b"same bytes").unwrap();
    fs::write(dir.path().join("sub/b.txt"), b"same bytes").unwrap();
    let roots = vec![
        dir.path().to_path_buf(),
        dir.path().join("sub"),
        dir.path().join("sub/.."),
    ];
    let groups = find_duplicates(&roots, &ScanOptions::default());
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].paths.len(), 2);
    assert_eq!(total_reclaimable(&groups), 10);
    for action in plan_clean(&groups, KeepPolicy::First, CleanMode::Delete).unwrap() {
        apply_action(&action).unwrap();
    }
    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"same bytes");
    assert!(!dir.path().join("sub/b.txt").exists());
    assert!(find_duplicates(&roots, &ScanOptions::default()).is_empty());
}

#[test]
#[cfg(unix)]
fn scan_does_not_count_existing_hardlinks_as_reclaimable_copies() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, b"one data block").unwrap();
    fs::hard_link(&a, &b).unwrap();
    assert!(find_duplicates(&[dir.path().to_path_buf()], &ScanOptions::default()).is_empty());
}

#[test]
#[cfg(unix)]
fn symlink_root_alias_does_not_duplicate_a_file_identity() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real");
    let alias = dir.path().join("alias");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("only.txt"), b"only copy").unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let mut opts = ScanOptions::default();
    opts.follow_symlinks = true;
    assert!(find_duplicates(&[real, alias], &opts).is_empty());
}

#[test]
fn planner_refuses_repeated_paths_before_emitting_keeper_deletion() {
    for paths in [vec!["a", "a"], vec!["a", "./a"]] {
        let group = DupGroup {
            hash: "fixture".into(),
            size: 10,
            paths: paths.into_iter().map(PathBuf::from).collect(),
        };
        assert!(plan_clean(&[group], KeepPolicy::First, CleanMode::Delete).is_err());
    }
    let first = DupGroup {
        hash: "one".into(),
        size: 10,
        paths: vec!["a".into(), "b".into()],
    };
    let second = DupGroup {
        hash: "two".into(),
        size: 10,
        paths: vec!["c".into(), "a".into()],
    };
    assert!(plan_clean(&[first, second], KeepPolicy::First, CleanMode::Delete).is_err());
}

#[test]
#[cfg(unix)]
fn hardlink_preserves_unrelated_legacy_staging_file_and_repeated_runs() {
    let dir = TempDir::new().unwrap();
    let keep = dir.path().join("a.txt");
    let path = dir.path().join("b.txt");
    let sentinel = dir.path().join(".file-dedupe-tmp-b.txt");
    fs::write(&keep, b"duplicate").unwrap();
    fs::write(&path, b"duplicate").unwrap();
    fs::write(&sentinel, b"unrelated staging name").unwrap();
    let action = Action::Hardlink {
        path: path.clone(),
        keep: keep.clone(),
        reclaim: 9,
    };
    assert_eq!(apply_action(&action).unwrap(), 9);
    assert_eq!(fs::read(&sentinel).unwrap(), b"unrelated staging name");
    assert_eq!(apply_action(&action).unwrap(), 0);
    assert_eq!(inode(&keep), inode(&path));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    assert!(find_duplicates(&[dir.path().to_path_buf()], &ScanOptions::default()).is_empty());
}

#[test]
fn hardlink_refuses_changed_content_without_replacing_it() {
    let dir = TempDir::new().unwrap();
    let keep = dir.path().join("a.txt");
    let path = dir.path().join("b.txt");
    fs::write(&keep, b"duplicate").unwrap();
    fs::write(&path, b"new data!").unwrap();
    let action = Action::Hardlink {
        path: path.clone(),
        keep: keep.clone(),
        reclaim: 9,
    };
    assert!(apply_action(&action).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"new data!");
    assert_eq!(fs::read(&keep).unwrap(), b"duplicate");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn hardlink_failure_preserves_destination_and_leaves_no_staging_file() {
    let dir = TempDir::new().unwrap();
    let keep = dir.path().join("a.txt");
    let path = dir.path().join("b.txt");
    fs::write(&keep, b"duplicate").unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("sentinel"), b"unrelated").unwrap();
    let action = Action::Hardlink {
        path: path.clone(),
        keep: keep.clone(),
        reclaim: 9,
    };
    assert!(apply_action(&action).is_err());
    assert_eq!(fs::read(path.join("sentinel")).unwrap(), b"unrelated");
    assert_eq!(fs::read(&keep).unwrap(), b"duplicate");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
#[cfg(unix)]
fn planner_refuses_aliases_of_a_keeper_across_groups() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    let c = dir.path().join("c");
    let alias = dir.path().join("alias");
    fs::write(&a, b"duplicate").unwrap();
    fs::write(&b, b"duplicate").unwrap();
    fs::write(&c, b"duplicate").unwrap();
    fs::hard_link(&a, &alias).unwrap();
    let one = DupGroup {
        hash: "one".into(),
        size: 9,
        paths: vec![a, b],
    };
    let two = DupGroup {
        hash: "two".into(),
        size: 9,
        paths: vec![c, alias],
    };
    assert!(plan_clean(&[one, two], KeepPolicy::First, CleanMode::Delete).is_err());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 4);
}

#[test]
#[cfg(unix)]
fn hardlink_refuses_a_symlink_destination_without_replacing_it() {
    let dir = TempDir::new().unwrap();
    let keep = dir.path().join("keep");
    let original = dir.path().join("original");
    let path = dir.path().join("alias");
    fs::write(&keep, b"duplicate").unwrap();
    fs::write(&original, b"duplicate").unwrap();
    std::os::unix::fs::symlink(&original, &path).unwrap();
    assert!(apply_action(&Action::Hardlink {
        path: path.clone(),
        keep,
        reclaim: 9
    })
    .is_err());
    assert!(fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&original).unwrap(), b"duplicate");
}
