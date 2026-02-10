# bms-dl

A CLI tool that downloads all BMS files and diffs from a BMS difficulty table URL. It resolves indirect links, extracts archives, and normalizes directory structure.

## Installation

Requires [Rust](https://rustup.rs/) and Chromium/Chrome (for JS-rendered pages).

```sh
cargo install --path .
```

## Usage

```
bms-dl <TABLE_URL> [OPTIONS]
```

### Options

| Option | Description | Default |
|--------|-------------|---------|
| `-o, --output <DIR>` | Output directory | `.` |
| `-j, --jobs <N>` | Number of concurrent downloads | `8` |
| `--no-diff` | Skip downloading diffs | |
| `--level <LEVEL>` | Filter by level (e.g. `"0"`, `"5"`) | |
| `--skip-existing` | Skip entries that already exist in the output directory | |

### Examples

Download an entire table:

```sh
bms-dl https://stellabms.xyz/sl/table.html -o satellite
```

Download only level 0 entries with 16 concurrent downloads:

```sh
bms-dl https://stellabms.xyz/sl/table.html -o satellite --level 0 -j 16
```

Resume a previous download (skip already downloaded entries):

```sh
bms-dl https://stellabms.xyz/sl/table.html -o satellite --skip-existing
```

## Features

- **Archive formats**: ZIP, RAR, 7z, LZH (with Shift_JIS filename support)
- **Hosting services**: Google Drive, Dropbox, OneDrive, 1drv.ms, and more
- **Headless browser fallback**: Resolves JS-rendered pages via Chromium
- **Concurrent downloads** with retry and progress bar
- **Diff integration**: Automatically downloads and merges diff files
- **Directory normalization**: Flattens nested directory structures

## License

[MIT](LICENSE)
