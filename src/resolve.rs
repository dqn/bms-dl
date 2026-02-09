use anyhow::{Result, anyhow};
use scraper::{Html, Selector};
use url::Url;

use crate::browser;

/// Resolved download URL with metadata
#[derive(Debug, Clone)]
pub struct ResolvedUrl {
    pub url: String,
    pub original: String,
}

/// Resolve a URL to its actual download link.
/// Some URLs point to HTML pages that contain the real download link.
pub fn resolve_url<'a>(
    client: &'a reqwest::Client,
    raw_url: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ResolvedUrl>> + Send + 'a>> {
    let raw_url = raw_url.to_string();
    let client = client.clone();
    Box::pin(async move {
        let parsed = Url::parse(&raw_url)?;
        let host = parsed.host_str().unwrap_or("").to_string();

        match host.as_str() {
            "drive.google.com" => resolve_google_drive(&raw_url),
            "dropbox.com" | "www.dropbox.com" | "dl.dropboxusercontent.com" => {
                resolve_dropbox(&raw_url)
            }
            "manbow.nothing.sh" => resolve_manbow(&client, &raw_url).await,
            "venue.bmssearch.net" => resolve_venue_bmssearch(&client, &raw_url).await,
            "mega.nz" => Err(anyhow!(
                "mega.nz is not supported (encryption API required)"
            )),
            _ => {
                // Pass through URLs with archive extensions directly
                let path_lower = parsed.path().to_lowercase();
                let archive_extensions = [".zip", ".rar", ".7z", ".lzh"];
                if archive_extensions
                    .iter()
                    .any(|ext| path_lower.ends_with(ext))
                {
                    return Ok(ResolvedUrl {
                        url: raw_url.clone(),
                        original: raw_url,
                    });
                }
                // Otherwise try to extract a download link from the page
                resolve_generic(&client, &raw_url).await
            }
        }
    })
}

fn resolve_google_drive(raw_url: &str) -> Result<ResolvedUrl> {
    let parsed = Url::parse(raw_url)?;
    let path = parsed.path();

    // Extract file ID: try /file/d/{id} pattern first
    let file_id = path
        .split("/file/d/")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .filter(|s| !s.is_empty())
        .map(String::from)
        // Then try ?id= query parameter
        .or_else(|| {
            parsed
                .query_pairs()
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned())
        })
        .ok_or_else(|| anyhow!("failed to extract Google Drive file ID from {raw_url}"))?;

    let download_url =
        format!("https://drive.google.com/uc?export=download&id={file_id}&confirm=t");

    Ok(ResolvedUrl {
        url: download_url,
        original: raw_url.to_string(),
    })
}

fn resolve_dropbox(raw_url: &str) -> Result<ResolvedUrl> {
    let mut parsed = Url::parse(raw_url)?;

    // Replace dl=0 with dl=1 to get direct download
    let pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| {
            if k == "dl" {
                (k.into_owned(), "1".to_string())
            } else {
                (k.into_owned(), v.into_owned())
            }
        })
        .collect();

    let has_dl = pairs.iter().any(|(k, _)| k == "dl");

    if has_dl {
        parsed.query_pairs_mut().clear();
        for (k, v) in &pairs {
            parsed.query_pairs_mut().append_pair(k, v);
        }
    } else {
        parsed.query_pairs_mut().append_pair("dl", "1");
    }

    Ok(ResolvedUrl {
        url: parsed.to_string(),
        original: raw_url.to_string(),
    })
}

/// Extract download URLs from JSON embedded in HTML (e.g. Next.js SSR pages).
/// Looks for `"downloadURL":"..."` patterns in script tags.
fn extract_json_download_urls(html: &str) -> Vec<String> {
    let needle = "\"downloadURL\":\"";
    let mut urls = Vec::new();
    let mut search_from = 0;

    while let Some(start) = html[search_from..].find(needle) {
        let url_start = search_from + start + needle.len();
        if let Some(end) = html[url_start..].find('"') {
            let raw = &html[url_start..url_start + end];
            // Unescape JSON forward-slash escaping
            let url = raw.replace("\\/", "/");
            urls.push(url);
            search_from = url_start + end;
        } else {
            break;
        }
    }

    urls
}

/// Check a list of candidate URLs for archive or hosting service links.
/// Returns `Some(Ok(...))` if a download link is found,
/// `Some(Err(...))` if resolution failed, or `None` if no candidates matched.
async fn find_download_from_candidates(
    client: &reqwest::Client,
    candidates: &[String],
    raw_url: &str,
) -> Option<Result<ResolvedUrl>> {
    let archive_extensions = [".zip", ".rar", ".7z", ".lzh"];
    let hosting_domains = [
        "drive.google.com",
        "dropbox.com",
        "www.dropbox.com",
        "onedrive.live.com",
        "1drv.ms",
    ];

    for candidate in candidates {
        // Check for direct archive links using only the path component (ignoring query params)
        let is_archive = if let Ok(parsed) = Url::parse(candidate) {
            let path = parsed.path().to_lowercase();
            archive_extensions.iter().any(|ext| path.ends_with(ext))
        } else {
            let lower = candidate.to_lowercase();
            archive_extensions.iter().any(|ext| lower.ends_with(ext))
        };

        if is_archive {
            return Some(Ok(ResolvedUrl {
                url: candidate.clone(),
                original: raw_url.to_string(),
            }));
        }

        // Check for hosting service links and resolve them
        if let Ok(parsed) = Url::parse(candidate)
            && let Some(host) = parsed.host_str()
            && hosting_domains.iter().any(|d| host.contains(d))
        {
            return Some(resolve_url(client, candidate).await);
        }
    }

    None
}

/// Generic fallback resolver: fetch the page and try to find a download link.
/// Used for unknown domains that might be event pages with download links.
async fn resolve_generic(client: &reqwest::Client, raw_url: &str) -> Result<ResolvedUrl> {
    let resp = match client.get(raw_url).send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!("failed to fetch {raw_url} for link extraction: {e}");
            return Ok(ResolvedUrl {
                url: raw_url.to_string(),
                original: raw_url.to_string(),
            });
        }
    };

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // If the response is not HTML, it's likely a direct download
    if !content_type.contains("text/html") {
        return Ok(ResolvedUrl {
            url: raw_url.to_string(),
            original: raw_url.to_string(),
        });
    }

    let html_text = resp.text().await?;
    let base_url = Url::parse(raw_url)?;
    let candidate_urls = extract_links_from_html(&html_text, &base_url)?;

    if let Some(result) = find_download_from_candidates(client, &candidate_urls, raw_url).await {
        return result;
    }

    // No download link found via HTML — try headless browser for SPA pages
    tracing::info!("no download link found via HTML on {raw_url}, trying headless browser");
    match browser::resolve_with_browser(raw_url).await {
        Ok(resolved) => return Ok(resolved),
        Err(e) => tracing::debug!("browser fallback also failed for {raw_url}: {e}"),
    }

    // All attempts failed — return URL as-is (will likely fail at download phase)
    tracing::debug!("no download link found on {raw_url}, passing through as-is");
    Ok(ResolvedUrl {
        url: raw_url.to_string(),
        original: raw_url.to_string(),
    })
}

async fn resolve_venue_bmssearch(client: &reqwest::Client, raw_url: &str) -> Result<ResolvedUrl> {
    let html_text = client.get(raw_url).send().await?.text().await?;

    // Try JSON-embedded download URLs first (Next.js SSR)
    let mut candidates = extract_json_download_urls(&html_text);

    // Fallback: extract <a href> links
    let base_url = Url::parse(raw_url)?;
    candidates.extend(extract_links_from_html(&html_text, &base_url)?);

    match find_download_from_candidates(client, &candidates, raw_url).await {
        Some(result) => result,
        None => Err(anyhow!(
            "no download link found on venue.bmssearch.net page: {raw_url}"
        )),
    }
}

async fn resolve_manbow(client: &reqwest::Client, raw_url: &str) -> Result<ResolvedUrl> {
    let html_text = client.get(raw_url).send().await?.text().await?;

    let base_url = Url::parse(raw_url)?;
    let candidate_urls = extract_links_from_html(&html_text, &base_url)?;

    if let Some(result) = find_download_from_candidates(client, &candidate_urls, raw_url).await {
        return result;
    }

    tracing::info!(
        "no download link found via HTML scraping on {raw_url}, trying headless browser"
    );
    match browser::resolve_with_browser(raw_url).await {
        Ok(resolved) => Ok(resolved),
        Err(e) => Err(anyhow!(
            "no download link found on manbow page (HTML scraping and browser both failed): {raw_url}: {e}"
        )),
    }
}

fn extract_links_from_html(html: &str, base_url: &Url) -> Result<Vec<String>> {
    let document = Html::parse_document(html);
    let link_selector =
        Selector::parse("a[href]").map_err(|e| anyhow!("failed to parse selector: {e}"))?;

    let mut urls = Vec::new();
    for element in document.select(&link_selector) {
        if let Some(href) = element.value().attr("href") {
            let resolved = base_url.join(href).unwrap_or_else(|_| base_url.clone());
            urls.push(resolved.to_string());
        }
    }

    Ok(urls)
}
