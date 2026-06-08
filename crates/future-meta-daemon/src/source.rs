//! HTML discovery and fetch abstraction.

use anyhow::{Result, anyhow, bail};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue, REFERER, USER_AGENT};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

const TOTAL_URL: &str = "https://www.9qihuo.com/qihuoshouxufei";
const DETAIL_BASE_URL: &str = "https://www.9qihuo.com/qihuoshouxufeisingle";
const CSV_BASE_URL: &str = "https://www.9qihuo.com/shouxufeixz";
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const HTTP_MAX_ATTEMPTS: usize = 3;

/// Downloadable source discovered from the 9qihuo total fee page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    /// Variety code from the source `heyue` query parameter.
    pub heyue: String,
    /// Canonical source detail page URL for this variety.
    pub detail_url: String,
    /// Canonical CSV download URL for this variety.
    pub csv_url: String,
}

/// Discover single-variety source entries from a total page HTML document.
///
/// # Errors
///
/// Returns an error if the static anchor selector cannot be parsed.
pub fn discover_sources_from_html(html: &str) -> Result<Vec<SourceEntry>> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a").map_err(|err| anyhow!("invalid anchor selector: {err}"))?;
    let mut entries = BTreeMap::new();

    for node in document.select(&selector) {
        let Some(href) = node.value().attr("href") else {
            continue;
        };
        let Some(heyue) = extract_heyue(href) else {
            continue;
        };

        entries.insert(
            heyue.clone(),
            SourceEntry {
                detail_url: canonical_url(DETAIL_BASE_URL, &heyue)?,
                csv_url: canonical_url(CSV_BASE_URL, &heyue)?,
                heyue,
            },
        );
    }

    Ok(entries.into_values().collect())
}

/// Discover sources and write them to a file.
///
/// # Errors
///
/// Returns an error if discovery fails.
pub fn discover_to_file(out: &Path) -> Result<()> {
    let client = http_client()?;
    let html = fetch_text(&client, TOTAL_URL)?;
    let sources = discover_sources_from_html(&html)?;

    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(out, serde_json::to_vec_pretty(&sources)?)?;
    Ok(())
}

fn extract_heyue(href: &str) -> Option<String> {
    let url = reqwest::Url::parse(TOTAL_URL).ok()?.join(href).ok()?;
    if url.host_str()? != "www.9qihuo.com" || url.path() != "/qihuoshouxufeisingle" {
        return None;
    }

    let values = url
        .query_pairs()
        .filter_map(|(key, value)| (key == "heyue").then(|| value.into_owned()))
        .collect::<Vec<_>>();

    match values.as_slice() {
        [value] if is_valid_heyue(value) => Some(value.clone()),
        _ => None,
    }
}

fn is_valid_heyue(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn canonical_url(base: &str, heyue: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(base)?;
    url.query_pairs_mut().append_pair("heyue", heyue);
    Ok(url.into())
}

pub(crate) fn http_client() -> Result<reqwest::blocking::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/125.0 Safari/537.36",
        ),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,text/csv;q=0.8,*/*;q=0.7",
        ),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
    );
    headers.insert(REFERER, HeaderValue::from_static(TOTAL_URL));

    reqwest::blocking::Client::builder()
        .default_headers(headers)
        .cookie_store(true)
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(HTTP_TIMEOUT)
        .build()
        .map_err(Into::into)
}

pub(crate) fn fetch_text(client: &reqwest::blocking::Client, url: &str) -> Result<String> {
    let mut last_error = None;

    for attempt in 0..HTTP_MAX_ATTEMPTS {
        match client.get(url).send() {
            Ok(response) if response.status().is_success() => return Ok(response.text()?),
            Ok(response) if should_retry(response.status()) && attempt + 1 < HTTP_MAX_ATTEMPTS => {
                drop(response);
                std::thread::sleep(retry_delay(attempt));
            }
            Ok(response) => return Ok(response.error_for_status()?.text()?),
            Err(err) if attempt + 1 < HTTP_MAX_ATTEMPTS => {
                last_error = Some(err);
                std::thread::sleep(retry_delay(attempt));
            }
            Err(err) => return Err(err.into()),
        }
    }

    if let Some(err) = last_error {
        return Err(err.into());
    }

    bail!("failed to fetch {url}")
}

fn should_retry(status: StatusCode) -> bool {
    status == StatusCode::FORBIDDEN
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(500 * 2_u64.saturating_pow(u32::try_from(attempt).unwrap_or(0)))
}
