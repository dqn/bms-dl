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
            "mega.nz" => Err(anyhow!(
                "mega.nz is not supported (encryption API required)"
            )),
            _ => Ok(ResolvedUrl {
                url: raw_url.clone(),
                original: raw_url,
            }),
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

async fn resolve_manbow(client: &reqwest::Client, raw_url: &str) -> Result<ResolvedUrl> {
    let html_text = client.get(raw_url).send().await?.text().await?;

    let base_url = Url::parse(raw_url)?;

    // Collect candidate URLs from the HTML (no async while iterating scraper types)
    let candidate_urls = extract_links_from_html(&html_text, &base_url)?;

    let archive_extensions = [".zip", ".rar", ".7z", ".lzh"];
    let hosting_domains = [
        "drive.google.com",
        "dropbox.com",
        "www.dropbox.com",
        "onedrive.live.com",
        "1drv.ms",
    ];

    for resolved_str in &candidate_urls {
        let resolved_lower = resolved_str.to_lowercase();

        // Check for direct archive links
        if archive_extensions
            .iter()
            .any(|ext| resolved_lower.ends_with(ext))
        {
            return Ok(ResolvedUrl {
                url: resolved_str.clone(),
                original: raw_url.to_string(),
            });
        }

        // Check for hosting service links and resolve them
        if let Ok(resolved_parsed) = Url::parse(resolved_str)
            && let Some(host) = resolved_parsed.host_str()
            && hosting_domains.iter().any(|d| host.contains(d))
        {
            return resolve_url(client, resolved_str).await;
        }
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
