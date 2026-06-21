//! CLI-level tests driving the actual `file-dedupe` binary via assert_cmd,
//! checking human output, JSON shape, and the dry-run-vs-commit boundary.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

/// Two identical files + one unique file in a temp dir.
fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let body = b"cli fixture duplicate body 0123456789 0123456789 0123456789\n";
    fs::write(dir.path().join("a.txt"), body).unwrap();
    fs::write(dir.path().join("b.txt"), body).unwrap();
    fs::write(
        dir.path().join("only.txt"),
        b"unique unique unique unique\n",
    )
    .unwrap();
    dir
}

fn bin() -> Command {
    Command::cargo_bin("file-dedupe").unwrap()
}

#[test]
fn scan_human_reports_group() {
    let dir = fixture();
    bin()
        .arg("scan")
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("duplicate group"))
        .stdout(predicate::str::contains("a.txt"))
        .stdout(predicate::str::contains("b.txt"))
        .stdout(predicate::str::contains("reclaimable"));
}

#[test]
fn scan_no_dups_message() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("x"), b"one").unwrap();
    fs::write(dir.path().join("y"), b"two").unwrap();
    bin()
        .arg("scan")
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("No duplicate files found"));
}

#[test]
fn scan_json_shape() {
    let dir = fixture();
    let out = bin()
        .arg("scan")
        .arg(dir.path())
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: Value = serde_json::from_slice(&out).expect("scan --json must emit valid JSON");
    assert_eq!(v["group_count"], 1);
    assert!(v["reclaimable_bytes"].as_u64().unwrap() > 0);

    let groups = v["groups"].as_array().expect("groups must be an array");
    assert_eq!(groups.len(), 1);
    let g = &groups[0];
    assert!(g["hash"].is_string());
    assert!(g["size"].as_u64().unwrap() > 0);
    let paths = g["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    let joined = paths
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(",");
    assert!(joined.contains("a.txt") && joined.contains("b.txt"));
}

#[test]
fn clean_defaults_to_dry_run_and_changes_nothing() {
    let dir = fixture();
    bin()
        .arg("clean")
        .arg(dir.path())
        .arg("--keep")
        .arg("first")
        .assert()
        .success()
        .stdout(predicate::str::contains("DRY RUN"))
        .stdout(predicate::str::contains("--commit"));

    // Both duplicates must still be present.
    assert!(dir.path().join("a.txt").exists());
    assert!(dir.path().join("b.txt").exists());
    assert!(dir.path().join("only.txt").exists());
}

#[test]
fn clean_commit_deletes_one_keeps_one() {
    let dir = fixture();
    bin()
        .arg("clean")
        .arg(dir.path())
        .arg("--keep")
        .arg("first")
        .arg("--commit")
        .assert()
        .success()
        .stdout(predicate::str::contains("Committed"));

    // keep=first => a.txt (sorts first) kept, b.txt deleted.
    assert!(
        dir.path().join("a.txt").exists(),
        "a.txt (keeper) must remain"
    );
    assert!(!dir.path().join("b.txt").exists(), "b.txt must be deleted");
    assert!(dir.path().join("only.txt").exists());
}

#[test]
fn clean_json_shape_reports_planned_actions() {
    let dir = fixture();
    let out = bin()
        .arg("clean")
        .arg(dir.path())
        .arg("--keep")
        .arg("first")
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: Value = serde_json::from_slice(&out).expect("clean --json must emit valid JSON");
    assert_eq!(v["committed"], false, "default clean is a dry-run");
    assert_eq!(v["mode"], "delete");
    assert_eq!(v["keep_policy"], "first");
    assert_eq!(v["deletions_planned"], 1);
    assert!(v["reclaimable_bytes"].as_u64().unwrap() > 0);
    assert_eq!(v["reclaimed_bytes"], 0, "dry-run reclaims nothing");

    let actions = v["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 2);
    let kinds: Vec<&str> = actions
        .iter()
        .map(|a| a["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"keep"));
    assert!(kinds.contains(&"delete"));

    // Files untouched (it was JSON dry-run).
    assert!(dir.path().join("a.txt").exists());
    assert!(dir.path().join("b.txt").exists());
}

#[test]
fn clean_nonexistent_dir_errors() {
    bin()
        .arg("clean")
        .arg("/no/such/path/file-dedupe-xyz")
        .arg("--keep")
        .arg("first")
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));
}
