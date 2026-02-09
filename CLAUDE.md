# bms-dl

## Overview

BMS difficulty table downloader CLI. Downloads all BMS files and diffs from a table URL,
resolves indirect links, extracts archives, and normalizes directory structure.

## Commands

- Build: `cargo build`
- Run: `cargo run -- <TABLE_URL> [OPTIONS]`
- Check: `cargo check && cargo clippy && cargo fmt --check`

## CLI Usage

```
bms-dl <TABLE_URL> [OPTIONS]

Options:
  -o, --output <DIR>    Output directory [default: .]
  -j, --jobs <N>        Concurrent downloads [default: 4]
      --no-diff         Skip downloading diffs
      --level <LEVEL>   Filter by level (e.g. "0", "5")
      --skip-existing   Skip entries that already exist
```

## Module Structure

| File | Role |
|------|------|
| `src/main.rs` | Pipeline orchestration |
| `src/cli.rs` | CLI argument definitions (clap derive) |
| `src/table.rs` | Table parsing (HTML → header.json → body.json) |
| `src/resolve.rs` | URL resolution (Google Drive, Dropbox, manbow.nothing.sh) |
| `src/browser.rs` | Headless Chrome for JS-rendered pages |
| `src/download.rs` | Concurrent download with Semaphore, retry, progress bar |
| `src/archive.rs` | Archive extraction (ZIP/RAR/7z/LZH, Shift_JIS support) |
| `src/normalize.rs` | Directory flattening + diff file placement |

## Key Dependencies

- `unrar` (v0.5): uses typestate API — `open_for_processing()` → `read_header()` → `extract_with_base()`
- `delharc` (v0.6): skip entries by calling `next_file()` without reading
- `zip` (v2): no `is_utf8()` method — check with `std::str::from_utf8(name_raw())`
- `chromiumoxide` (v0.7): handler needs `futures_util::StreamExt` for `.next()`

## Notes

- `resolve_url` uses `Box::pin` to handle recursive async (manbow → Google Drive)
- Scraper types (`ElementRef`) are not `Send` — collect links into `Vec<String>` before awaiting

## Known Tables

| Table | URL | Output Dir |
|-------|-----|------------|
| Satellite (sl) | `https://stellabms.xyz/sl/table.html` | `.agent/satellite` |
