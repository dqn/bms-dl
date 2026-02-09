mod archive;
mod browser;
mod cli;
mod download;
mod normalize;
mod resolve;
mod table;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::Semaphore;

use crate::cli::Args;
use crate::download::{DownloadResult, DownloadTask};
use crate::table::SongEntry;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into())
                .add_directive("chromiumoxide=off".parse().unwrap()),
        )
        .init();

    let args = Args::parse();
    let output_dir = PathBuf::from(&args.output);
    tokio::fs::create_dir_all(&output_dir).await?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(300))
        .cookie_store(true)
        .build()?;

    // Phase 1: Fetch table
    tracing::info!("fetching table from {}", args.table_url);
    let (header, entries) = table::fetch_table(&client, &args.table_url).await?;
    tracing::info!(
        "table '{}' ({}): {} entries",
        header.name,
        header.symbol,
        entries.len()
    );

    // Filter by level if specified
    let entries: Vec<_> = if let Some(ref level) = args.level {
        entries
            .into_iter()
            .filter(|e| e.level.as_deref() == Some(level))
            .collect()
    } else {
        entries
    };

    tracing::info!("{} entries after filtering", entries.len());

    // Phase 2: Group entries by base URL and generate download tasks
    let groups = group_entries(&entries, &header.symbol);
    let mut tasks = Vec::new();

    for (dir_name, group) in &groups {
        let entry_dir = output_dir.join(dir_name);

        // Skip existing entries if requested, but clean up failed directories
        if args.skip_existing && entry_dir.exists() {
            extract_unprocessed_archives(&entry_dir);

            if normalize::contains_bms_files(&entry_dir) {
                tracing::info!("skipping download for existing: {dir_name}");
                continue;
            }

            tracing::warn!("cleaning up failed directory: {dir_name}");
            std::fs::remove_dir_all(&entry_dir)?;
        }

        // Base download
        if let Some(ref base_url) = group.base_url {
            tasks.push(DownloadTask {
                url: base_url.clone(),
                output_dir: entry_dir.clone(),
                fallback_name: format!("{dir_name}.zip"),
                label: format!("[base] {dir_name}"),
            });
        }

        // Diff downloads
        if !args.no_diff {
            for (i, diff_url) in group.diff_urls.iter().enumerate() {
                tasks.push(DownloadTask {
                    url: diff_url.clone(),
                    output_dir: entry_dir.clone(),
                    fallback_name: format!("{dir_name}_diff{i}.zip"),
                    label: format!("[diff] {dir_name} #{i}"),
                });
            }
        }
    }

    tracing::info!("{} download tasks generated", tasks.len());

    // Phase 3-4: Download with concurrency control
    let download_start = std::time::Instant::now();
    let results = download::execute_downloads(&client, tasks, args.jobs).await;
    let download_duration = download_start.elapsed();

    // Phase 5-6: Extract archives and normalize (parallel)
    let mut success_count = 0u32;
    let mut skip_count = 0u32;
    let mut fail_count = 0u32;
    let mut failed_entries = Vec::new();
    let mut skipped_entries = Vec::new();

    let extract_parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let extract_semaphore = Arc::new(Semaphore::new(extract_parallelism));
    let mut extract_handles = Vec::new();

    for result in results {
        match result {
            DownloadResult::Success { path } => {
                success_count += 1;

                let permit = extract_semaphore.clone().acquire_owned().await.unwrap();
                extract_handles.push(tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    if let Err(e) = extract_and_normalize(&path) {
                        tracing::warn!("extraction failed for {}: {e}", path.display());
                    }
                }));
            }
            DownloadResult::Skipped { url, reason } => {
                skip_count += 1;
                skipped_entries.push(format!("{url}\t{reason}"));
            }
            DownloadResult::Failed { url, error } => {
                fail_count += 1;
                failed_entries.push(format!("{url}\t{error}"));
            }
        }
    }

    for handle in extract_handles {
        let _ = handle.await;
    }

    // Apply diff normalization: copy diff BMS files into base directories
    for dir_name in groups.keys() {
        let entry_dir = output_dir.join(dir_name);
        if !entry_dir.exists() {
            continue;
        }

        // Find the main content directory (non-hidden)
        let main_dirs: Vec<_> = std::fs::read_dir(&entry_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && !e.file_name().to_string_lossy().starts_with('.')
            })
            .collect();

        // Find diff extracted directories
        let diff_dirs: Vec<_> = std::fs::read_dir(&entry_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && e.file_name().to_string_lossy().starts_with('.')
                    && e.file_name().to_string_lossy().ends_with("_extracted")
            })
            .collect();

        if let Some(main_dir) = main_dirs.first() {
            for diff_dir in &diff_dirs {
                let count =
                    normalize::copy_diff_files(&diff_dir.path(), &main_dir.path()).unwrap_or(0);
                if count > 0 {
                    tracing::info!(
                        "copied {count} diff files into {}",
                        main_dir.path().display()
                    );
                }
                // Clean up diff extracted directory
                let _ = std::fs::remove_dir_all(diff_dir.path());
            }
        }
    }

    // Write failed log
    if !failed_entries.is_empty() {
        let failed_log = output_dir.join("failed.log");
        tokio::fs::write(&failed_log, failed_entries.join("\n")).await?;
        tracing::info!("failed entries written to {}", failed_log.display());
    }

    // Summary
    let total_downloads = success_count + skip_count + fail_count;
    let duration_secs = download_duration.as_secs_f64();
    let rate = if duration_secs > 0.0 {
        total_downloads as f64 / duration_secs
    } else {
        0.0
    };

    println!();
    println!("=== Summary ===");
    println!("  Success: {success_count}");
    println!("  Skipped: {skip_count}");
    println!("  Failed:  {fail_count}");
    println!("  Duration: {duration_secs:.1}s ({rate:.1} downloads/s)");

    if !failed_entries.is_empty() {
        println!();
        println!("=== Failed ===");
        for entry in &failed_entries {
            println!("  {entry}");
        }
    }

    if !skipped_entries.is_empty() {
        println!();
        println!("=== Skipped ===");
        for entry in &skipped_entries {
            println!("  {entry}");
        }
    }

    Ok(())
}

struct EntryGroup {
    base_url: Option<String>,
    diff_urls: Vec<String>,
}

fn group_entries(entries: &[SongEntry], symbol: &str) -> HashMap<String, EntryGroup> {
    let mut groups: HashMap<String, EntryGroup> = HashMap::new();

    for entry in entries {
        let dir_name = make_dir_name(entry, symbol);

        let group = groups.entry(dir_name).or_insert_with(|| EntryGroup {
            base_url: None,
            diff_urls: Vec::new(),
        });

        if group.base_url.is_none()
            && let Some(ref url) = entry.url
            && !url.is_empty()
        {
            group.base_url = Some(url.clone());
        }

        if let Some(ref diff_url) = entry.url_diff
            && !diff_url.is_empty()
            && !group.diff_urls.contains(diff_url)
        {
            group.diff_urls.push(diff_url.clone());
        }
    }

    groups
}

fn make_dir_name(entry: &SongEntry, symbol: &str) -> String {
    let level = entry.level.as_deref().unwrap_or("_");
    let title = entry.title.as_deref().unwrap_or("unknown");

    let name = format!("{symbol}{level}_{title}");
    sanitize_dir_name(&name)
}

fn sanitize_dir_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Scan a directory for unextracted archives and HTML junk files.
/// Extracts valid archives and removes HTML files that were saved by mistake.
fn extract_unprocessed_archives(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if fname.starts_with('.') {
            continue;
        }

        // Remove HTML files that were saved by mistake
        if archive::is_html(&path) {
            tracing::warn!("removing HTML junk file: {}", path.display());
            let _ = std::fs::remove_file(&path);
            continue;
        }

        // Try to extract if it looks like an archive
        if archive::ArchiveFormat::detect(&path).is_ok() {
            tracing::info!("extracting unprocessed archive: {}", path.display());
            if let Err(e) = extract_and_normalize(&path) {
                tracing::warn!("extraction failed for {}: {e}", path.display());
            }
        }
    }
}

fn extract_and_normalize(archive_path: &Path) -> Result<()> {
    let parent = archive_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("archive has no parent directory"))?;

    let extract_dir = archive::extract_archive(archive_path, parent)?;

    // Flatten single subdirectories
    normalize::flatten_single_subdirs(&extract_dir)?;

    // Move extracted contents to parent
    for entry in std::fs::read_dir(&extract_dir)? {
        let entry = entry?;
        let dest = parent.join(entry.file_name());
        if !dest.exists() {
            std::fs::rename(entry.path(), &dest)?;
        }
    }

    // Clean up
    let _ = std::fs::remove_dir_all(&extract_dir);
    let _ = std::fs::remove_file(archive_path);

    Ok(())
}
