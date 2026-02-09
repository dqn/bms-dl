use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use chromiumoxide::browser::{Browser, BrowserConfig};
use futures_util::StreamExt;
use tokio::sync::Semaphore;
use url::Url;

use crate::resolve::ResolvedUrl;

/// Serialize browser launches to avoid SingletonLock conflicts between
/// concurrent Chromium instances.
static BROWSER_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

fn browser_semaphore() -> &'static Semaphore {
    BROWSER_SEMAPHORE.get_or_init(|| Semaphore::new(1))
}

/// Resolve download URL using headless Chrome for JS-rendered pages.
pub async fn resolve_with_browser(raw_url: &str) -> Result<ResolvedUrl> {
    let _permit = browser_semaphore().acquire().await?;

    let config = BrowserConfig::builder()
        .no_sandbox()
        .build()
        .map_err(|e| anyhow!("failed to build browser config: {e}"))?;

    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| anyhow!("failed to launch browser: {e}"))?;

    let handle = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            let _ = event;
        }
    });

    let page = browser.new_page(raw_url).await?;

    // Wait for page to render
    page.wait_for_navigation().await?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Extract all links from the page
    let links: Vec<String> = page
        .evaluate(
            r#"
            Array.from(document.querySelectorAll('a[href]'))
                .map(a => a.href)
            "#,
        )
        .await?
        .into_value()?;

    let base_url = Url::parse(raw_url)?;
    let archive_extensions = [".zip", ".rar", ".7z", ".lzh"];
    let hosting_domains = [
        "drive.google.com",
        "dropbox.com",
        "www.dropbox.com",
        "onedrive.live.com",
        "1drv.ms",
    ];

    for link in &links {
        let Ok(resolved) = base_url.join(link) else {
            continue;
        };
        let resolved_str = resolved.to_string();
        let resolved_lower = resolved_str.to_lowercase();

        if archive_extensions
            .iter()
            .any(|ext| resolved_lower.ends_with(ext))
        {
            drop(browser);
            handle.abort();
            return Ok(ResolvedUrl {
                url: resolved_str,
                original: raw_url.to_string(),
            });
        }

        if let Some(host) = resolved.host_str()
            && hosting_domains.iter().any(|d| host.contains(d))
        {
            drop(browser);
            handle.abort();
            return Ok(ResolvedUrl {
                url: resolved_str,
                original: raw_url.to_string(),
            });
        }
    }

    drop(browser);
    handle.abort();

    Err(anyhow!(
        "no download link found on JS-rendered page: {raw_url}"
    ))
}
