//! file-dedupe CLI.
//!
//! Thin wrapper over the `file_dedupe` library: parse args, run a scan or a
//! clean, and print human or JSON output. All the interesting logic
//! (size-bucketing, hashing, the keep-policy planner) lives in `lib.rs`.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use file_dedupe::{
    apply_action, find_duplicates, plan_clean, total_reclaimable, Action, CleanMode, DupGroup,
    KeepPolicy, ScanOptions,
};

#[derive(Parser)]
#[command(
    name = "file-dedupe",
    about = "Find and remove duplicate files by content hash (blake3).",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan directories and report duplicate groups (read-only).
    Scan(ScanArgs),
    /// Remove duplicates by deleting or hardlinking. Dry-run by default.
    Clean(CleanArgs),
}

#[derive(Args)]
struct ScanArgs {
    /// One or more directories to scan.
    #[arg(required = true, value_name = "DIR")]
    dirs: Vec<PathBuf>,

    /// Ignore files smaller than this many bytes.
    #[arg(long, default_value_t = 0, value_name = "BYTES")]
    min_size: u64,

    /// Recurse into subdirectories (default: on). Use --no-recursive to disable.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    recursive: bool,

    /// Follow symlinks while walking (default: off).
    #[arg(long, default_value_t = false)]
    follow_symlinks: bool,

    /// Emit machine-readable JSON instead of a human report.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CleanArgs {
    /// One or more directories to scan for duplicates to clean.
    #[arg(required = true, value_name = "DIR")]
    dirs: Vec<PathBuf>,

    /// Which file in each duplicate group to keep.
    #[arg(long, value_enum, value_name = "POLICY")]
    keep: KeepArg,

    /// Replace duplicates with hardlinks to the kept file instead of deleting.
    #[arg(long)]
    hardlink: bool,

    /// Actually perform the deletions/hardlinks. Without this it is a dry-run.
    #[arg(long)]
    commit: bool,

    /// Explicitly request a dry-run (this is already the default).
    #[arg(long, conflicts_with = "commit")]
    dry_run: bool,

    /// Ignore files smaller than this many bytes.
    #[arg(long, default_value_t = 0, value_name = "BYTES")]
    min_size: u64,

    /// Recurse into subdirectories (default: on). Use --no-recursive to disable.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    recursive: bool,

    /// Follow symlinks while walking (default: off).
    #[arg(long, default_value_t = false)]
    follow_symlinks: bool,

    /// Emit machine-readable JSON instead of a human report.
    #[arg(long)]
    json: bool,
}

#[derive(Copy, Clone, ValueEnum)]
enum KeepArg {
    First,
    Newest,
    Oldest,
}

impl From<KeepArg> for KeepPolicy {
    fn from(k: KeepArg) -> Self {
        match k {
            KeepArg::First => KeepPolicy::First,
            KeepArg::Newest => KeepPolicy::Newest,
            KeepArg::Oldest => KeepPolicy::Oldest,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan(args) => cmd_scan(args),
        Command::Clean(args) => cmd_clean(args),
    }
}

fn validate_dirs(dirs: &[PathBuf]) -> Result<()> {
    for d in dirs {
        if !d.exists() {
            bail!("path does not exist: {}", d.display());
        }
        if !d.is_dir() {
            bail!("not a directory: {}", d.display());
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct ScanReport<'a> {
    groups: &'a [DupGroup],
    group_count: usize,
    reclaimable_bytes: u64,
}

fn cmd_scan(args: ScanArgs) -> Result<()> {
    validate_dirs(&args.dirs)?;
    let opts = ScanOptions {
        min_size: args.min_size,
        recursive: args.recursive,
        follow_symlinks: args.follow_symlinks,
    };
    let groups = find_duplicates(&args.dirs, &opts);
    let reclaimable = total_reclaimable(&groups);

    if args.json {
        let report = ScanReport {
            groups: &groups,
            group_count: groups.len(),
            reclaimable_bytes: reclaimable,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if groups.is_empty() {
        println!("No duplicate files found.");
        return Ok(());
    }

    println!(
        "Found {} duplicate group(s); {} reclaimable.\n",
        groups.len(),
        human_bytes(reclaimable)
    );
    for (i, g) in groups.iter().enumerate() {
        println!(
            "Group {} — {} x {} files = {} reclaimable  [{}]",
            i + 1,
            human_bytes(g.size),
            g.paths.len(),
            human_bytes(g.reclaimable_bytes()),
            short_hash(&g.hash),
        );
        for p in &g.paths {
            println!("    {}", p.display());
        }
        println!();
    }
    Ok(())
}

#[derive(Serialize)]
struct CleanReport<'a> {
    committed: bool,
    mode: &'a str,
    keep_policy: &'a str,
    actions: &'a [Action],
    deletions_planned: usize,
    reclaimable_bytes: u64,
    reclaimed_bytes: u64,
}

fn cmd_clean(args: CleanArgs) -> Result<()> {
    validate_dirs(&args.dirs)?;
    let opts = ScanOptions {
        min_size: args.min_size,
        recursive: args.recursive,
        follow_symlinks: args.follow_symlinks,
    };
    let mode = if args.hardlink {
        CleanMode::Hardlink
    } else {
        CleanMode::Delete
    };
    let policy: KeepPolicy = args.keep.into();

    let groups = find_duplicates(&args.dirs, &opts);
    let actions = plan_clean(&groups, policy, mode)
        .map_err(anyhow::Error::msg)
        .context("refusing to clean: could not plan a safe set of actions")?;

    // Bytes we *would* reclaim (sum over non-keep actions).
    let reclaimable: u64 = actions
        .iter()
        .map(|a| match a {
            Action::Delete { reclaim, .. } | Action::Hardlink { reclaim, .. } => *reclaim,
            Action::Keep { .. } => 0,
        })
        .sum();
    let to_remove = actions
        .iter()
        .filter(|a| !matches!(a, Action::Keep { .. }))
        .count();

    let verb = match mode {
        CleanMode::Delete => "delete",
        CleanMode::Hardlink => "hardlink",
    };

    // Apply only when --commit is given. --dry-run (and the absence of any
    // flag) leaves the filesystem untouched.
    let mut reclaimed = 0u64;
    if args.commit {
        for a in &actions {
            reclaimed += apply_action(a)
                .with_context(|| format!("failed to {verb} {}", a.path().display()))?;
        }
    }

    if args.json {
        let report = CleanReport {
            committed: args.commit,
            mode: verb,
            keep_policy: keep_label(policy),
            actions: &actions,
            deletions_planned: to_remove,
            reclaimable_bytes: reclaimable,
            reclaimed_bytes: reclaimed,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if groups.is_empty() {
        println!("No duplicate files found; nothing to clean.");
        return Ok(());
    }

    let past = match mode {
        CleanMode::Delete => "deleted",
        CleanMode::Hardlink => "hardlinked",
    };
    if args.commit {
        println!(
            "Committed: {} {} file(s) (keep={}), reclaimed {}.\n",
            past,
            to_remove,
            keep_label(policy),
            human_bytes(reclaimed)
        );
    } else {
        println!(
            "DRY RUN — nothing was changed. Would {} {} file(s) (keep={}), reclaiming {}.",
            verb,
            to_remove,
            keep_label(policy),
            human_bytes(reclaimable)
        );
        println!("Re-run with --commit to apply.\n");
    }

    for a in &actions {
        match a {
            Action::Keep { path } => println!("  keep      {}", path.display()),
            Action::Delete { path, .. } => {
                let tag = if args.commit { "deleted" } else { "would del" };
                println!("  {tag:<9} {}", path.display());
            }
            Action::Hardlink { path, keep, .. } => {
                let tag = if args.commit { "linked" } else { "would link" };
                println!("  {:<9} {} -> {}", tag, path.display(), keep.display());
            }
        }
    }
    Ok(())
}

fn keep_label(p: KeepPolicy) -> &'static str {
    match p {
        KeepPolicy::First => "first",
        KeepPolicy::Newest => "newest",
        KeepPolicy::Oldest => "oldest",
    }
}

/// First 12 hex chars of a hash for compact display.
fn short_hash(h: &str) -> &str {
    &h[..h.len().min(12)]
}

/// Format a byte count as a human-friendly string (binary units).
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.2} {}", UNITS[i])
}
