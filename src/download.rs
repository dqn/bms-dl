use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::header;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::archive;
use crate::resolve::{self, ResolvedUrl};

/// Result of a single download task
#[derive(Debug)]
pub enum DownloadResult {
    Success { path: PathBuf },
    Skipped { url: String, reason: String },
    Failed { url: String, error: String },
}

/// Whether a download error is worth retrying.
/// HTTP 4xx and content-type mismatches are deterministic — retrying won't help.
fn is_retryable(err: &anyhow::Error) -> bool {
    let msg = err.to_string();

    // HTTP 4xx errors are deterministic — file doesn't exist or access denied
    if msg.contains("status client error") {
        return false;
    }

    // Content-type mismatch — server consistently returns HTML
    if msg.contains("server returned HTML")
        || msg.contains("Dropbox file has been removed")
        || msg.contains("Google Drive file requires authentication")
        || msg.contains("Google Drive returned HTML confirmation")
        || msg.contains("downloaded file is HTML")
    {
        return false;
    }

    // Everything else (5xx, network, timeout) is worth retrying
    true
}

/// Download a file from a resolved URL to the given directory.
async fn download_file(
    client: &reqwest::Client,
    resolved: &ResolvedUrl,
    output_dir: &Path,
    fallback_name: &str,
    pb: &ProgressBar,
) -> Result<PathBuf> {
    let mut last_error = None;

    for attempt in 0..3 {
        if attempt > 0 {
            let delay = std::time::Duration::from_secs(1 << (2 * attempt));
            pb.set_message(format!("retry {attempt}/3 in {}s...", delay.as_secs()));
            tokio::time::sleep(delay).await;
        }

        match try_download(client, &resolved.url, output_dir, fallback_name, pb).await {
            Ok(path) => return Ok(path),
            Err(e) => {
                tracing::warn!(
                    "download attempt {}/{} failed for {} (resolved: {}): {e}",
                    attempt + 1,
                    3,
                    resolved.original,
                    resolved.url,
                );
                if !is_retryable(&e) {
                    return Err(e);
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap())
}

async fn try_download(
    client: &reqwest::Client,
    url: &str,
    output_dir: &Path,
    fallback_name: &str,
    pb: &ProgressBar,
) -> Result<PathBuf> {
    let resp = client.get(url).send().await?.error_for_status()?;

    // Check if this is a Google Drive virus scan confirmation page
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.contains("text/html") {
        if is_google_drive_url(url) {
            let html_body = resp.text().await?;
            if let Some(confirm_url) = extract_gdrive_confirm_url(&html_body) {
                tracing::info!("Google Drive virus scan detected, following confirmation URL");
                let resp2 = client.get(&confirm_url).send().await?.error_for_status()?;
                return save_response(resp2, output_dir, fallback_name, pb).await;
            }
            // Detect Google login redirect (file is deleted or private)
            if html_body.contains("accounts.google.com") || html_body.contains("ServiceLogin") {
                return Err(anyhow::anyhow!(
                    "Google Drive file requires authentication (likely deleted or private)"
                ));
            }
            return Err(anyhow::anyhow!(
                "Google Drive returned HTML confirmation page but could not extract download URL"
            ));
        }

        // Non-Google-Drive URL returned HTML — detect specific hosting service errors
        let html_body = resp.text().await?;

        if (url.contains("dropbox.com") || url.contains("dropboxusercontent.com"))
            && (html_body.contains("doesn't exist")
                || html_body.contains("has been removed")
                || html_body.contains("Error (404)"))
        {
            return Err(anyhow::anyhow!(
                "Dropbox file has been removed or does not exist"
            ));
        }

        // Try secondary resolution: extract download links from the HTML page
        if let Ok(base_url) = url::Url::parse(url)
            && let Ok(links) = resolve::extract_links_from_html(&html_body, &base_url)
        {
            let no_redirect_client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(10))
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| client.clone());
            if let Some(Ok(resolved)) =
                resolve::find_download_from_candidates(&no_redirect_client, &links, url).await
            {
                tracing::info!(
                    "secondary resolution found download link: {} -> {}",
                    url,
                    resolved.url
                );
                return save_response(
                    client.get(&resolved.url).send().await?.error_for_status()?,
                    output_dir,
                    fallback_name,
                    pb,
                )
                .await;
            }
        }

        return Err(anyhow::anyhow!(
            "server returned HTML instead of archive file (Content-Type: text/html)"
        ));
    }

    save_response(resp, output_dir, fallback_name, pb).await
}

async fn save_response(
    resp: reqwest::Response,
    output_dir: &Path,
    fallback_name: &str,
    pb: &ProgressBar,
) -> Result<PathBuf> {
    let filename =
        extract_filename(&resp, resp.url().as_str()).unwrap_or_else(|| fallback_name.to_string());
    let dest = output_dir.join(&filename);
    let tmp = output_dir.join(format!(".{filename}.tmp"));

    pb.set_message(filename.clone());

    if let Some(len) = resp.content_length() {
        pb.set_length(len);
    }

    let mut file = tokio::fs::File::create(&tmp)
        .await
        .context("failed to create temp file")?;

    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("error reading response body")?;
        pb.inc(chunk.len() as u64);
        file.write_all(&chunk).await?;
    }

    file.flush().await?;
    drop(file);

    tokio::fs::rename(&tmp, &dest).await?;

    // Validate downloaded content is not HTML
    if archive::is_html(&dest) {
        let _ = tokio::fs::remove_file(&dest).await;
        return Err(anyhow::anyhow!(
            "downloaded file is HTML, not an archive (possible redirect or error page)"
        ));
    }

    Ok(dest)
}

fn is_google_drive_url(url: &str) -> bool {
    url.contains("drive.google.com") || url.contains("drive.usercontent.google.com")
}

/// Parse a Google Drive virus scan confirmation page and extract the actual download URL.
fn extract_gdrive_confirm_url(html: &str) -> Option<String> {
    let document = scraper::Html::parse_document(html);
    let input_selector = scraper::Selector::parse("input[type='hidden']").ok()?;

    // Try multiple form selectors to handle different Google Drive page structures
    let form_selectors = [
        "form#download-form",
        "form#downloadForm",
        "form[action*='drive.google.com']",
        "form[action*='drive.usercontent.google.com']",
    ];

    for selector_str in &form_selectors {
        let form_selector = match scraper::Selector::parse(selector_str) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if let Some(form) = document.select(&form_selector).next()
            && let Some(action) = form.value().attr("action")
        {
            let mut url = match url::Url::parse(action) {
                Ok(u) => u,
                Err(_) => continue,
            };
            for input in form.select(&input_selector) {
                if let Some(name) = input.value().attr("name") {
                    let value = input.value().attr("value").unwrap_or("");
                    url.query_pairs_mut().append_pair(name, value);
                }
            }
            return Some(url.to_string());
        }
    }

    // Fallback: look for direct download links in the page
    let link_selector = scraper::Selector::parse("a[href]").ok()?;
    for element in document.select(&link_selector) {
        if let Some(href) = element.value().attr("href")
            && (href.contains("export=download") || href.contains("confirm="))
        {
            return Some(href.to_string());
        }
    }

    None
}

fn extract_filename(resp: &reqwest::Response, url: &str) -> Option<String> {
    // Try Content-Disposition header
    if let Some(cd) = resp.headers().get(header::CONTENT_DISPOSITION)
        && let Ok(cd_str) = cd.to_str()
        && let Some(fname) = parse_content_disposition(cd_str)
    {
        return Some(sanitize_filename(&fname));
    }

    // Try URL path
    let parsed = url::Url::parse(url).ok()?;
    let path = parsed.path();
    let segment = path.rsplit('/').next()?;

    if segment.is_empty() {
        return None;
    }

    let decoded = urlencoding::decode(segment).ok()?;
    Some(sanitize_filename(&decoded))
}

fn parse_content_disposition(header: &str) -> Option<String> {
    // Look for filename*=UTF-8''... first (RFC 5987)
    if let Some(pos) = header.find("filename*=") {
        let rest = &header[pos + 10..];
        if let Some(rest) = rest
            .strip_prefix("UTF-8''")
            .or_else(|| rest.strip_prefix("utf-8''"))
        {
            let end = rest.find(';').unwrap_or(rest.len());
            let encoded = &rest[..end].trim();
            if let Ok(decoded) = urlencoding::decode(encoded) {
                return Some(decoded.into_owned());
            }
        }
    }

    // Fallback to filename="..."
    if let Some(pos) = header.find("filename=") {
        let rest = &header[pos + 9..];
        let rest = rest.trim_start_matches('"');
        let end = rest
            .find('"')
            .or_else(|| rest.find(';'))
            .unwrap_or(rest.len());
        let name = rest[..end].trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    None
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            _ => c,
        })
        .collect()
}

/// Task descriptor for one download unit (base or diff)
pub struct DownloadTask {
    pub url: String,
    pub output_dir: PathBuf,
    pub fallback_name: String,
    pub label: String,
}

/// Result of URL resolution phase
enum ResolveResult {
    Resolved {
        resolved: resolve::ResolvedUrl,
        task: DownloadTask,
    },
    Skipped {
        url: String,
        reason: String,
    },
}

/// Execute all download tasks with concurrency control and progress display.
///
/// Phase 1: Resolve all URLs in parallel (with `jobs * 2` concurrency).
/// Phase 2: Download resolved URLs in parallel (with `jobs` concurrency).
pub async fn execute_downloads(
    client: &reqwest::Client,
    tasks: Vec<DownloadTask>,
    jobs: usize,
) -> Vec<DownloadResult> {
    // Phase 1: Resolve URLs
    let resolve_semaphore = Arc::new(Semaphore::new(jobs * 2));
    let client_arc = Arc::new(client.clone());
    let mut resolve_handles = Vec::new();

    for task in tasks {
        let sem = resolve_semaphore.clone();
        let client = client_arc.clone();

        resolve_handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            match resolve::resolve_url(&client, &task.url).await {
                Ok(resolved) => ResolveResult::Resolved { resolved, task },
                Err(e) => ResolveResult::Skipped {
                    url: task.url.clone(),
                    reason: e.to_string(),
                },
            }
        }));
    }

    let mut resolved_tasks = Vec::new();
    let mut results = Vec::new();

    for handle in resolve_handles {
        match handle.await {
            Ok(ResolveResult::Resolved { resolved, task }) => {
                resolved_tasks.push((resolved, task));
            }
            Ok(ResolveResult::Skipped { url, reason }) => {
                tracing::warn!("skipping {url}: {reason}");
                results.push(DownloadResult::Skipped { url, reason });
            }
            Err(e) => {
                results.push(DownloadResult::Failed {
                    url: "unknown".to_string(),
                    error: format!("resolve task panicked: {e}"),
                });
            }
        }
    }

    // Phase 2: Download resolved URLs
    let download_semaphore = Arc::new(Semaphore::new(jobs));
    let multi_progress = MultiProgress::new();
    let style = ProgressStyle::with_template(
        "{spinner:.green} [{bar:30.cyan/blue}] {bytes}/{total_bytes} {msg}",
    )
    .unwrap()
    .progress_chars("=>-");

    let mut download_handles = Vec::new();

    for (resolved, task) in resolved_tasks {
        let sem = download_semaphore.clone();
        let client = client_arc.clone();
        let pb = multi_progress.add(ProgressBar::new(0));
        pb.set_style(style.clone());
        pb.set_message(task.label.clone());

        download_handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            // Create output directory
            if let Err(e) = tokio::fs::create_dir_all(&task.output_dir).await {
                pb.finish_with_message(format!("FAIL: {e}"));
                return DownloadResult::Failed {
                    url: task.url.clone(),
                    error: e.to_string(),
                };
            }

            match download_file(
                &client,
                &resolved,
                &task.output_dir,
                &task.fallback_name,
                &pb,
            )
            .await
            {
                Ok(path) => {
                    pb.finish_with_message("done");
                    DownloadResult::Success { path }
                }
                Err(e) => {
                    pb.finish_with_message(format!("FAIL: {e}"));
                    let error = if resolved.url != task.url {
                        format!("[resolved: {}] {e}", resolved.url)
                    } else {
                        e.to_string()
                    };
                    DownloadResult::Failed {
                        url: task.url.clone(),
                        error,
                    }
                }
            }
        }));
    }

    for handle in download_handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => results.push(DownloadResult::Failed {
                url: "unknown".to_string(),
                error: format!("task panicked: {e}"),
            }),
        }
    }

    results
}
