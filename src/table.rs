use anyhow::{Context, Result, anyhow};
use scraper::{Html, Selector};
use serde::Deserialize;
use url::Url;

#[derive(Debug, Deserialize)]
pub struct TableHeader {
    pub name: String,
    pub symbol: String,
    pub data_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SongEntry {
    #[allow(dead_code)]
    pub md5: Option<String>,
    #[allow(dead_code)]
    pub sha256: Option<String>,
    pub title: Option<String>,
    #[allow(dead_code)]
    pub artist: Option<String>,
    pub url: Option<String>,
    pub url_diff: Option<String>,
    pub level: Option<String>,
}

pub async fn fetch_table(
    client: &reqwest::Client,
    table_url: &str,
) -> Result<(TableHeader, Vec<SongEntry>)> {
    let table_url = Url::parse(table_url).context("invalid table URL")?;

    // Fetch HTML and extract bmstable meta tag
    let html_text = client
        .get(table_url.as_str())
        .send()
        .await
        .context("failed to fetch table HTML")?
        .text()
        .await
        .context("failed to read table HTML body")?;

    let document = Html::parse_document(&html_text);
    let selector = Selector::parse(r#"meta[name="bmstable"]"#)
        .map_err(|e| anyhow!("failed to parse selector: {e}"))?;

    let meta = document
        .select(&selector)
        .next()
        .ok_or_else(|| anyhow!("bmstable meta tag not found"))?;

    let header_path = meta
        .value()
        .attr("content")
        .ok_or_else(|| anyhow!("bmstable meta tag has no content attribute"))?;

    // Resolve header URL relative to table URL
    let header_url = table_url
        .join(header_path)
        .context("failed to resolve header URL")?;

    tracing::info!("fetching header from {header_url}");

    let header: TableHeader = client
        .get(header_url.as_str())
        .send()
        .await
        .context("failed to fetch header.json")?
        .json()
        .await
        .context("failed to parse header.json")?;

    // Resolve data URL relative to header URL
    let data_url = header_url
        .join(&header.data_url)
        .context("failed to resolve data URL")?;

    tracing::info!("fetching body from {data_url}");

    let entries: Vec<SongEntry> = client
        .get(data_url.as_str())
        .send()
        .await
        .context("failed to fetch body.json")?
        .json()
        .await
        .context("failed to parse body.json")?;

    tracing::info!(
        "loaded {} entries from table '{}'",
        entries.len(),
        header.name
    );

    Ok((header, entries))
}
