# file-dedupe

Find and remove duplicate files by content hash. Fast by construction: files are
bucketed by **size** first, split by a cheap **4 KiB pre-hash**, and only then
confirmed with a full **[blake3](https://github.com/BLAKE3-team/BLAKE3)** hash —
so most files are never fully read.

Destructive operations are **dry-run by default** and the kept file in a group is
never touched.

## Install

```sh
cargo build --release
# binary at target/release/file-dedupe
```

## Usage

### Scan (read-only)

```sh
file-dedupe scan DIR [DIR2 ...]
```

Reports each group of byte-identical files and the total reclaimable bytes
(the sum of every duplicate copy beyond the first in each group).

Flags:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--min-size <BYTES>` | `0` | Ignore files smaller than this. |
| `--recursive <true\|false>` | `true` | Recurse into subdirectories. |
| `--follow-symlinks` | off | Follow symlinks while walking. |
| `--json` | off | Emit machine-readable JSON. |

Example:

```sh
file-dedupe scan ~/Downloads --min-size 1024
```

### Clean (delete or hardlink)

```sh
file-dedupe clean DIR --keep <first|newest|oldest>
```

Within each duplicate group, keep exactly one file (chosen by `--keep`) and
remove the rest. **This is a dry-run by default** — it prints what *would*
happen and changes nothing. Add `--commit` to actually apply it.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--keep <first\|newest\|oldest>` | required | Which copy to keep per group. |
| `--commit` | off | Actually delete/hardlink. Otherwise dry-run. |
| `--dry-run` | (default) | Explicitly request the default dry-run. |
| `--hardlink` | off | Replace duplicates with hardlinks to the kept file instead of deleting. |
| `--min-size`, `--recursive`, `--follow-symlinks`, `--json` | | Same as `scan`. |

Examples:

```sh
# Preview what deleting (keeping the newest copy) would do:
file-dedupe clean ~/Pictures --keep newest

# Actually do it:
file-dedupe clean ~/Pictures --keep newest --commit

# Reclaim space but keep every path, by hardlinking duplicates:
file-dedupe clean ~/Pictures --keep first --hardlink --commit
```

`--hardlink` swaps each duplicate for a hardlink to the kept file via an atomic
`link`-then-`rename`, so a crash can never leave a path missing.

## Safety guarantees

- `clean` does nothing without `--commit`.
- The kept file in a group is never deleted or replaced.
- Actions never cross a group boundary.
- If a keeper can't be determined (e.g. a `newest`/`oldest` policy on files
  whose mtime can't be read), `clean` refuses rather than guessing.
- Unreadable files are skipped, not fatal.

## Library

The core is exposed as a library:

```rust
use file_dedupe::{find_duplicates, plan_clean, ScanOptions, KeepPolicy, CleanMode};

let groups = find_duplicates(&roots, &ScanOptions::default());
let actions = plan_clean(&groups, KeepPolicy::First, CleanMode::Delete)?;
```

`find_duplicates` does the size-bucket → pre-hash → full-hash pipeline.
`plan_clean` is a pure function (no I/O) that turns groups + a policy into a list
of `Action`s — it's the primary test surface. `main` is the only place that
mutates the filesystem.

## License

MIT — see [LICENSE](LICENSE).
