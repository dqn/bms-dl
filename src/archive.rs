use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Detect archive format from magic bytes, falling back to extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    Rar,
    SevenZ,
    Lzh,
}

impl ArchiveFormat {
    pub fn detect(path: &Path) -> Result<Self> {
        let mut file = fs::File::open(path)?;
        let mut magic = [0u8; 8];
        let n = file.read(&mut magic)?;
        let magic = &magic[..n];

        if magic.starts_with(b"PK") {
            return Ok(Self::Zip);
        }
        if magic.starts_with(b"Rar!") {
            return Ok(Self::Rar);
        }
        if magic.starts_with(b"7z\xBC\xAF\x27\x1C") {
            return Ok(Self::SevenZ);
        }
        // LZH: bytes 2-4 are "-lh" or "-lz"
        if magic.len() >= 5 && (magic[2] == b'-') && (magic[3] == b'l') {
            return Ok(Self::Lzh);
        }

        // Fallback to extension
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        match ext.as_str() {
            "zip" => Ok(Self::Zip),
            "rar" => Ok(Self::Rar),
            "7z" => Ok(Self::SevenZ),
            "lzh" | "lha" => Ok(Self::Lzh),
            _ => Err(anyhow!("unknown archive format for {}", path.display())),
        }
    }
}

/// Extract an archive to the given directory.
pub fn extract(archive_path: &Path, output_dir: &Path) -> Result<()> {
    let format = ArchiveFormat::detect(archive_path)?;

    fs::create_dir_all(output_dir)?;

    match format {
        ArchiveFormat::Zip => extract_zip(archive_path, output_dir),
        ArchiveFormat::Rar => extract_rar(archive_path, output_dir),
        ArchiveFormat::SevenZ => extract_7z(archive_path, output_dir),
        ArchiveFormat::Lzh => extract_lzh(archive_path, output_dir),
    }
}

fn extract_zip(archive_path: &Path, output_dir: &Path) -> Result<()> {
    let file = fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;

        // Decode filename - try UTF-8 first, then Shift_JIS
        let raw = entry.name_raw().to_vec();
        let name = match std::str::from_utf8(&raw) {
            Ok(s) => s.to_string(),
            Err(_) => {
                let (decoded, _, _) = encoding_rs::SHIFT_JIS.decode(&raw);
                decoded.into_owned()
            }
        };

        // Zip Slip protection
        let path = output_dir.join(&name);
        if !path.starts_with(output_dir) {
            tracing::warn!("skipping zip entry with path traversal: {name}");
            continue;
        }

        if entry.is_dir() {
            fs::create_dir_all(&path)?;
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut outfile = fs::File::create(&path)?;
            std::io::copy(&mut entry, &mut outfile)?;
        }
    }

    Ok(())
}

fn extract_rar(archive_path: &Path, output_dir: &Path) -> Result<()> {
    let archive = unrar::Archive::new(archive_path)
        .open_for_processing()
        .map_err(|e| anyhow!("failed to open RAR archive: {e}"))?;

    let mut entry = match archive.read_header() {
        Ok(Some(e)) => e,
        Ok(None) => return Ok(()),
        Err(e) => return Err(anyhow!("failed to read RAR header: {e}")),
    };

    loop {
        let next = entry
            .extract_with_base(output_dir)
            .map_err(|e| anyhow!("failed to extract RAR entry: {e}"))?;

        match next.read_header() {
            Ok(Some(e)) => entry = e,
            Ok(None) => break,
            Err(e) => return Err(anyhow!("failed to read RAR header: {e}")),
        }
    }

    Ok(())
}

fn extract_7z(archive_path: &Path, output_dir: &Path) -> Result<()> {
    sevenz_rust2::decompress_file(archive_path, output_dir)
        .context("failed to extract 7z archive")?;

    Ok(())
}

fn extract_lzh(archive_path: &Path, output_dir: &Path) -> Result<()> {
    let file = fs::File::open(archive_path)?;
    let mut lha_reader = delharc::LhaDecodeReader::new(file)?;

    loop {
        let header = lha_reader.header();
        let path_str = header.parse_pathname().to_string_lossy().into_owned();

        let dest = output_dir.join(&path_str);

        // Path traversal protection
        if !dest.starts_with(output_dir) {
            tracing::warn!("skipping LZH entry with path traversal: {path_str}");
            if !lha_reader.next_file()? {
                break;
            }
            continue;
        }

        if header.is_directory() {
            fs::create_dir_all(&dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut outfile = fs::File::create(&dest)?;
            std::io::copy(&mut lha_reader, &mut outfile)?;
            lha_reader.crc_check()?;
        }

        if !lha_reader.next_file()? {
            break;
        }
    }

    Ok(())
}

/// Extract archive and return the output directory path (for cleanup).
pub fn extract_archive(archive_path: &Path, base_dir: &Path) -> Result<PathBuf> {
    let stem = archive_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("extracted");

    let extract_dir = base_dir.join(format!(".{stem}_extracted"));
    extract(archive_path, &extract_dir)?;

    Ok(extract_dir)
}
