use std::fs;
use std::path::Path;

use anyhow::Result;

/// Flatten single-subdirectory nesting.
/// If a directory contains only one subdirectory and nothing else,
/// move its contents up to the parent level. Applied recursively.
pub fn flatten_single_subdirs(dir: &Path) -> Result<()> {
    loop {
        let entries: Vec<_> = fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();

        // Only flatten if there's exactly one entry and it's a directory
        if entries.len() != 1 {
            break;
        }

        let single = &entries[0];
        if !single.file_type()?.is_dir() {
            break;
        }

        let sub_dir = single.path();

        // Move all contents from subdirectory to parent
        for entry in fs::read_dir(&sub_dir)? {
            let entry = entry?;
            let dest = dir.join(entry.file_name());
            fs::rename(entry.path(), &dest)?;
        }

        // Remove the now-empty subdirectory
        fs::remove_dir(&sub_dir)?;
    }

    Ok(())
}

/// Copy diff files (.bms, .bme, .bml, .bmson) from src_dir to dest_dir.
pub fn copy_diff_files(src_dir: &Path, dest_dir: &Path) -> Result<u32> {
    let bms_extensions = ["bms", "bme", "bml", "bmson"];
    let mut count = 0;

    if !src_dir.exists() {
        return Ok(0);
    }

    for entry in walkdir(src_dir)? {
        let ext = entry
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        if bms_extensions.contains(&ext.as_str()) {
            let filename = entry.file_name().unwrap();
            let dest = dest_dir.join(filename);
            if !dest.exists() {
                fs::copy(&entry, &dest)?;
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Recursively list all files in a directory.
fn walkdir(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir(&path)?);
        } else {
            files.push(path);
        }
    }

    Ok(files)
}
