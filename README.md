# file-sorter

A fast duplicate-file finder for one or more directories, with a command-line tool and an optional desktop GUI. It hashes files in parallel, groups exact duplicates (including whole duplicate folders), and can move the extras to the Trash or into a mirrored destination directory.

## Features

- **Parallel hashing** — uses all CPU cores (via [rayon](https://crates.io/crates/rayon)) with SHA-256, comparing file size and a partial hash before committing to a full read.
- **Large-file handling** — files above a configurable size threshold (default 500 MB) are reported as *probable* duplicates immediately and only fully verified right before deletion/move, so a scan doesn't stall hashing huge files.
- **Whole-folder duplicate detection** — recognizes when entire directory trees are duplicates of each other, not just individual files.
- **Resumable scans** — a cancelled scan (Ctrl+C) checkpoints its progress and can be picked up later with `--resume`.
- **Run history** — every scan is recorded locally and can be listed, re-displayed, exported to JSON, or imported.
- **Safe deletion** — duplicates go to the Trash (recoverable), not `rm`'d directly; `--dry-run` previews any delete/move without touching anything.
- **Filtering** — skip files below a minimum size, exclude directories by name, restrict to specific file-type categories (images, audio, video, documents, archives, programs, misc).
- **Desktop GUI** — an [iced](https://iced.rs/)-based GUI (`file-sorter-gui`) covering the same scan/review/delete workflow with a folder picker and live progress.

## Prerequisites

- [Rust and Cargo](https://www.rust-lang.org/tools/install) (stable toolchain, 2021 edition or newer)

No other system dependencies are required — `rusqlite` is built with its `bundled` SQLite, so no separate SQLite install is needed.

## Build and run locally

Clone the repo and build from the project root:

```bash
git clone https://github.com/aa362ce/file-sorter-rs.git
cd file-sorter-rs
cargo build --release
```

This produces two binaries under `target/release/`:

- `file-sorter` (or `file-sorter.exe` on Windows) — the CLI
- `file-sorter-gui` (or `file-sorter-gui.exe` on Windows) — the GUI

Run the CLI directly with Cargo (add `--` before any of its own arguments):

```bash
cargo run --release --bin file-sorter -- <directories...> [options]
```

Or run the compiled binary directly:

```bash
./target/release/file-sorter <directories...> [options]
```

Run the GUI the same way:

```bash
cargo run --release --bin file-sorter-gui
```

For quicker iteration during development, drop `--release` (debug builds compile faster but hash more slowly).

## CLI usage

```
file-sorter [OPTIONS] [DIRECTORIES]...
```

Scan one or more directories recursively and print any duplicate files/folders found:

```bash
file-sorter ~/Downloads ~/Pictures
```

Common options:

| Flag | Description |
| --- | --- |
| `--min-size <BYTES>` | Ignore files smaller than this many bytes |
| `-j, --threads <N>` | Number of hashing threads (default: one per CPU core) |
| `--large-threshold <BYTES>` | Size at which files are reported as probable duplicates without full verification during the scan |
| `--exclude <NAME>` | Skip a directory name wherever it's found (repeatable) |
| `--no-default-excludes` | Don't skip the built-in excluded directories/files (e.g. `node_modules`, `.git`-adjacent caches, `.tmp` files) |
| `--type <CATEGORY>` | Only scan a given file-type category — `images`, `audio`, `video`, `documents`, `archives`, `programs`, `misc` (repeatable) |
| `--delete` | Move duplicates to the Trash after scanning, keeping the first copy of each group |
| `--move-to <DIR>` | Move duplicates into `DIR` instead of deleting, mirroring each file's original path underneath it |
| `-y, --yes` | Skip the confirmation prompt before deleting/moving |
| `--dry-run` | Preview what `--delete`/`--move-to` would do without changing anything |
| `--resume [N]` | Resume a cancelled scan (most recent, or run `#N` from `--history`) |
| `--history` | Show past run history instead of scanning |
| `--show <N>` | Reload and print the full results of a past run |
| `--export-history <PATH>` | Export run history to a JSON file |
| `--import-history <PATH>` | Import run history from a JSON file (merges with existing) |
| `-q, --quiet` | Suppress the live progress display |
| `-v, --verbose` | Increase log verbosity (repeatable) |

Run `file-sorter --help` for the full, authoritative list.

### Examples

Preview what would be deleted, without touching anything:

```bash
file-sorter ~/Downloads --delete --dry-run
```

Delete duplicates without a confirmation prompt:

```bash
file-sorter ~/Downloads --delete -y
```

Move duplicate photos/videos into a review folder instead of trashing them:

```bash
file-sorter ~/Photos --type images --type video --move-to ~/DuplicateReview
```

Resume the most recently cancelled scan:

```bash
file-sorter --resume
```

## License

MIT — see [Cargo.toml](Cargo.toml).
