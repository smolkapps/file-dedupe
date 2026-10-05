# file-dedupe

Find and remove duplicate files by content hash. Fast by construction: files are
bucketed by **size** first, split by a cheap **4 KiB pre-hash**, and only then
confirmed with a full **[blake3](https://github.com/BLAKE3-team/BLAKE3)** hash —
so most files are never fully read.

Destructive operations are **dry-run by default** and the kept file in a group is
never touched.

## Install

Download `file-dedupe-linux-x86_64` and `SHA256SUMS` from the
[v0.1.1 release](https://github.com/smolkapps/file-dedupe/releases/tag/v0.1.1)
into a new directory, then:

```sh
sha256sum --check --ignore-missing SHA256SUMS &&
install -Dm755 file-dedupe-linux-x86_64 ./file-dedupe-bin/file-dedupe &&
./file-dedupe-bin/file-dedupe --version
```

The supplied binary is GNU/Linux x86_64, tested with glibc 2.39, and uses GLIBC
symbols through 2.34. Other platforms have not been verified; see the release's
`BUILD-INFO.txt` for library requirements.

Or build from source:

```sh
cargo build --release
# binary at target/release/file-dedupe
```

## Try a preview

After installing the downloaded binary above, create a disposable fixture and
inspect its duplicate group and cleanup plan:

```sh
(
  set -e
  fixture=$(mktemp -d)
  printf 'generated duplicate\n' > "$fixture/a.txt"
  cp "$fixture/a.txt" "$fixture/b.txt"
  printf 'generated unique\n' > "$fixture/unique.txt"
  ./file-dedupe-bin/file-dedupe scan "$fixture" --json
  ./file-dedupe-bin/file-dedupe clean "$fixture" --keep first
  printf 'Fixture retained at %s\n' "$fixture"
)
```

The scan reports one duplicate group. Cleanup prints `DRY RUN`, a keeper and
one proposed deletion. Neither command changes files; the fixture is retained
for inspection. Back up real files and read the [safety details](#safety-guarantees)
before applying cleanup.

## Usage

### Scan (read-only)

```sh
file-dedupe scan DIR [DIR2 ...]
```

Reports each group of byte-identical files and the logical reclaimable bytes
(the sum of every duplicate copy beyond the first in each group).
This is an estimate based on file lengths, not a measurement of filesystem space
freed; sparse files, compression, copy-on-write storage and existing links can
change actual disk savings.

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
`link`-then-`rename` inside an exclusively created private staging directory.
Existing staging names are left alone; failed replacements clean up only this
invocation's staging directory.
Hardlinks share both content and inode metadata, including permissions, ownership
and modification time. Editing either path changes the content seen through both.

## Safety guarantees

Back up important files and inspect the dry-run plan before using `--commit`.
See [RELEASE-USAGE.txt](RELEASE-USAGE.txt) for installation and a preview-first
workflow using disposable fixture examples.

- `clean` does nothing without `--commit`.
- The kept file in a group is never deleted or replaced.
- Repeated/overlapping roots and symlink aliases are scanned once using canonical
  paths. On Unix, existing hardlinks are counted once per device/inode too.
- Planning and commit validation refuse repeated targets or keeper identities.
- Hardlink replacement refuses symlinks, non-regular files, and changed content.
- Actions never cross a group boundary.
- If a keeper can't be determined (e.g. a `newest`/`oldest` policy on files
  whose mtime can't be read), `clean` refuses rather than guessing.
- Unreadable files are skipped, not fatal.

Cleanup is not a transaction across the whole batch: a later I/O failure does
not undo earlier deletions or replacements. Do not modify the tree concurrently
while cleaning. Identity/content checks detect changes before replacement but
cannot prevent another process changing a path between a check and a filesystem
operation. Non-Unix builds use canonical path identity and do not detect distinct
hardlink aliases.

## Library

The core is exposed as a library:

```rust
use file_dedupe::{find_duplicates, plan_clean, ScanOptions, KeepPolicy, CleanMode};

let groups = find_duplicates(&roots, &ScanOptions::default());
let actions = plan_clean(&groups, KeepPolicy::First, CleanMode::Delete)?;
```

`find_duplicates` does the size-bucket → pre-hash → full-hash pipeline.
`plan_clean` turns groups + a policy into a list of `Action`s and checks existing
file identities without changing files. Library callers applying a batch should
call `validate_actions` before applying any action. `apply_action` performs the
filesystem mutation.

## License

MIT — see [LICENSE](LICENSE).
