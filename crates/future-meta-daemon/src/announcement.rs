//! Broker announcement discovery and fee-candidate classification.
//!
//! Broker pages are discovery inputs only. This module intentionally does not
//! parse fee values or update fee history; callers must obtain and verify the
//! linked exchange-original evidence before applying a change.

use crate::db;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, REFERER, USER_AGENT};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use time::format_description;
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration as TimeDuration, OffsetDateTime, UtcOffset};

const CITICS_DETAIL_BASE_URL: &str = "https://www.citicsf.com/e-futures/content/000509/";
const HTFC_BASE_URL: &str = "https://htfc.com";
const CITICS_LIST_URL: &str = "https://www.citicsf.com/e-futures/news/exchangeNotice";
const HTFC_API_URL: &str = "https://htfc.com/servlet/json";
const REPLAY_DAYS: i64 = 14;
const INITIAL_LOOKBACK_DAYS: i64 = 30;
const MAX_SCAN_PAGES: usize = 1_000;
const FEE_KEYWORDS: [&str; 8] = [
    "手续费",
    "交易费用",
    "收费标准",
    "平今",
    "日内平仓",
    "免收",
    "减半",
    "减收",
];
const ADJUSTMENT_KEYWORDS: [&str; 9] = [
    "调整", "提高", "降低", "上调", "下调", "恢复", "免收", "减半", "减收",
];
const FEE_STANDARD_PHRASES: [&str; 3] = ["手续费标准", "交易手续费标准", "收费标准"];

/// A broker announcement directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncementSource {
    /// CITIC Futures' dedicated exchange-notice directory.
    Citics,
    /// HTFC's broader business-notice directory.
    Htfc,
}

impl AnnouncementSource {
    /// Stable source identifier used by persistence.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Citics => "citics",
            Self::Htfc => "htfc",
        }
    }
}

/// One item listed by a broker announcement source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnouncementListItem {
    /// Broker source that issued the listing.
    pub source: AnnouncementSource,
    /// Stable source-local article ID.
    pub article_id: String,
    /// Broker title, preserved for audit and queue display.
    pub title: String,
    /// Broker-published calendar date in `YYYY-MM-DD` form when present.
    pub published_at: String,
    /// Canonical broker detail page URL.
    pub url: String,
}

/// The selected content body of a broker announcement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnouncementDocument {
    /// Listing metadata from which the body was fetched.
    pub item: AnnouncementListItem,
    /// Selected body HTML, excluding global navigation and footer content.
    pub body_html: String,
    /// Whitespace-normalized selected body text.
    pub body_text: String,
    /// Exchange-original links embedded in the selected body.
    pub official_urls: Vec<String>,
}

/// A broker body mentioning a potentially fee-relevant adjustment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnouncementCandidate {
    /// Broker source that discovered the candidate.
    pub source: AnnouncementSource,
    /// Stable source-local article ID.
    pub article_id: String,
    /// Matched selected-body keywords.
    pub keywords: Vec<String>,
    /// Exchange-original URLs to fetch as authoritative evidence.
    pub official_urls: Vec<String>,
}

/// Transport abstraction used to scan broker announcement directories.
///
/// Production uses [`HttpAnnouncementTransport`]; tests inject deterministic
/// fixtures so source health behavior never depends on broker availability.
pub trait AnnouncementTransport {
    /// Fetch one page of source-specific listing metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the source cannot be fetched or parsed.
    fn fetch_list(
        &self,
        source: AnnouncementSource,
        page: usize,
    ) -> Result<Vec<AnnouncementListItem>>;

    /// Fetch the broker detail page for one listed article.
    ///
    /// # Errors
    ///
    /// Returns an error when the broker detail page cannot be fetched.
    fn fetch_body(&self, item: &AnnouncementListItem) -> Result<String>;

    /// Fetch an exchange-original document linked from a broker body.
    ///
    /// # Errors
    ///
    /// Returns an error when the exchange-original document cannot be fetched.
    fn fetch_official(&self, url: &str) -> Result<String>;
}

/// Summary of one primary/fallback announcement scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnouncementScan {
    /// Newly persisted selected-body snapshots.
    pub documents: usize,
    /// Newly detected fee-relevant broker candidates.
    pub candidates: usize,
    /// Sources successfully scanned in this run, in execution order.
    pub sources: Vec<AnnouncementSource>,
    /// Whether HTFC was used after a failed CITIC scan.
    pub used_fallback: bool,
}

/// HTTP implementation of the broker announcement transport.
pub struct HttpAnnouncementTransport {
    client: reqwest::blocking::Client,
}

impl HttpAnnouncementTransport {
    /// Build a transport with browser-like headers and bounded timeouts.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built.
    pub fn new() -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("text/html,application/json;q=0.9,*/*;q=0.8"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/125.0 Safari/537.36",
            ),
        );
        headers.insert(
            REFERER,
            HeaderValue::from_static("https://htfc.com/main/index/ggdt/index.shtml"),
        );
        let client = reqwest::blocking::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self { client })
    }
}

impl AnnouncementTransport for HttpAnnouncementTransport {
    fn fetch_list(
        &self,
        source: AnnouncementSource,
        page: usize,
    ) -> Result<Vec<AnnouncementListItem>> {
        match source {
            AnnouncementSource::Citics => {
                let response = self
                    .client
                    .get(CITICS_LIST_URL)
                    .query(&[("page", page)])
                    .send()?
                    .error_for_status()?;
                parse_citics_list_html(&response.text()?)
            }
            AnnouncementSource::Htfc => {
                let response = self
                    .client
                    .post(HTFC_API_URL)
                    .form(&[
                        ("funcNo", "2000065"),
                        ("catalogId", "10320"),
                        ("pageNum", &page.to_string()),
                        ("pageSize", "20"),
                        ("searchWord", ""),
                    ])
                    .send()?
                    .error_for_status()?;
                parse_htfc_api_page(&response.text()?)
            }
        }
    }

    fn fetch_body(&self, item: &AnnouncementListItem) -> Result<String> {
        Ok(self
            .client
            .get(&item.url)
            .send()?
            .error_for_status()?
            .text()?)
    }

    fn fetch_official(&self, url: &str) -> Result<String> {
        Ok(self.client.get(url).send()?.error_for_status()?.text()?)
    }
}

/// Scan CITIC first and use HTFC as a same-run fallback or explicit reconciliation.
///
/// The scanner persists only broker discovery evidence and candidate metadata.
/// It never interprets fee values or updates `fee_versions`.
///
/// # Errors
///
/// Returns an error when CITIC fails and HTFC cannot complete a fallback scan,
/// or when explicitly requested HTFC reconciliation fails.
pub fn scan_announcements(
    conn: &rusqlite::Connection,
    transport: &dyn AnnouncementTransport,
    observed_at: &str,
    reconcile_htfc: bool,
) -> Result<AnnouncementScan> {
    let citics = scan_source(conn, transport, AnnouncementSource::Citics, observed_at);
    match citics {
        Ok(mut summary) => {
            if reconcile_htfc {
                let htfc = scan_source(conn, transport, AnnouncementSource::Htfc, observed_at)?;
                summary.documents += htfc.documents;
                summary.candidates += htfc.candidates;
                summary.sources.push(AnnouncementSource::Htfc);
            }
            Ok(summary)
        }
        Err(citics_error) => {
            let htfc = scan_source(conn, transport, AnnouncementSource::Htfc, observed_at)
                .map_err(|htfc_error| {
                    anyhow!(
                        "CITIC announcement scan failed: {citics_error:#}; HTFC fallback failed: {htfc_error:#}"
                    )
                })?;
            Ok(AnnouncementScan {
                documents: htfc.documents,
                candidates: htfc.candidates,
                sources: vec![AnnouncementSource::Htfc],
                used_fallback: true,
            })
        }
    }
}

fn scan_source(
    conn: &rusqlite::Connection,
    transport: &dyn AnnouncementTransport,
    source: AnnouncementSource,
    observed_at: &str,
) -> Result<AnnouncementScan> {
    match scan_source_inner(conn, transport, source, observed_at) {
        Ok(summary) => Ok(summary),
        Err(error) => {
            let message = format!("{} announcement scan failed: {error:#}", source.as_str());
            db::record_announcement_source_error(conn, source, &message, observed_at)
                .context("failed to retain announcement source error")?;
            Err(anyhow!(message))
        }
    }
}

fn scan_source_inner(
    conn: &rusqlite::Connection,
    transport: &dyn AnnouncementTransport,
    source: AnnouncementSource,
    observed_at: &str,
) -> Result<AnnouncementScan> {
    let observed = OffsetDateTime::parse(observed_at, &Rfc3339)
        .context("invalid announcement scan timestamp")?;
    let floor = scan_floor(conn, source, observed)?;
    let mut documents = 0usize;
    let mut candidates = 0usize;
    let mut newest_published_at = None::<String>;

    for page in 1..=MAX_SCAN_PAGES {
        let items = match transport.fetch_list(source, page) {
            Ok(items) if !items.is_empty() => items,
            Ok(_) if page == 1 => {
                let error = anyhow!("{} announcement page 1 is empty", source.as_str());
                return Err(error);
            }
            Ok(_) => break,
            Err(error) => return Err(error),
        };
        if page == 1 {
            newest_published_at = items
                .iter()
                .map(|item| item.published_at.as_str())
                .max()
                .map(str::to_owned);
        }
        let complete_boundary_page = items
            .iter()
            .all(|item| parse_publication_date(&item.published_at).is_ok_and(|date| date <= floor));
        for item in &items {
            if parse_publication_date(&item.published_at).is_ok_and(|date| date <= floor) {
                continue;
            }
            if db::announcement_document_exists(conn, source, &item.article_id)? {
                retry_missing_official_snapshots(
                    conn,
                    transport,
                    source,
                    &item.article_id,
                    observed_at,
                )?;
                continue;
            }
            let html = transport.fetch_body(item)?;
            let document = match source {
                AnnouncementSource::Citics => parse_citics_article_html(item, &html),
                AnnouncementSource::Htfc => parse_htfc_article_html(item, &html),
            };
            let document = document.with_context(|| {
                format!(
                    "{} announcement body is invalid: {}",
                    source.as_str(),
                    item.article_id
                )
            })?;
            if db::record_announcement_document(conn, &document, observed_at)? {
                documents += 1;
            }
            if let Some(candidate) = classify_fee_candidate(&document) {
                db::record_announcement_candidate(conn, &candidate, observed_at)?;
                retry_missing_official_snapshots(
                    conn,
                    transport,
                    source,
                    &item.article_id,
                    observed_at,
                )?;
                candidates += 1;
            }
        }
        if complete_boundary_page {
            db::record_announcement_source_success(
                conn,
                source,
                newest_published_at.as_deref(),
                observed_at,
            )?;
            return Ok(AnnouncementScan {
                documents,
                candidates,
                sources: vec![source],
                used_fallback: false,
            });
        }
    }
    let error = anyhow!(
        "{} announcement scan exceeded {MAX_SCAN_PAGES} pages",
        source.as_str()
    );
    Err(error)
}

fn retry_missing_official_snapshots(
    conn: &rusqlite::Connection,
    transport: &dyn AnnouncementTransport,
    source: AnnouncementSource,
    article_id: &str,
    observed_at: &str,
) -> Result<()> {
    for official_url in db::pending_candidate_official_urls(conn, source, article_id)? {
        if let Ok(body) = transport.fetch_official(&official_url) {
            db::record_official_document_snapshot(conn, &official_url, &body, observed_at)?;
        }
    }
    Ok(())
}

fn scan_floor(
    conn: &rusqlite::Connection,
    source: AnnouncementSource,
    observed_at: OffsetDateTime,
) -> Result<Date> {
    if let Some(watermark) = db::announcement_source_watermark(conn, source)? {
        return Ok(parse_publication_date(&watermark)? - TimeDuration::days(REPLAY_DAYS));
    }
    if let Some(effective_at) = db::latest_fee_effective_at(conn)? {
        let effective = OffsetDateTime::parse(&effective_at, &Rfc3339)
            .context("invalid newest fee effective timestamp")?;
        return Ok(effective.date() - TimeDuration::days(INITIAL_LOOKBACK_DAYS));
    }
    let china = UtcOffset::from_hms(8, 0, 0).expect("valid China offset");
    Ok(observed_at.to_offset(china).date() - TimeDuration::days(INITIAL_LOOKBACK_DAYS))
}

fn parse_publication_date(value: &str) -> Result<Date> {
    let format = format_description::parse("[year]-[month]-[day]")?;
    Date::parse(value, &format)
        .map_err(|error| anyhow!("invalid announcement date {value}: {error}"))
}

/// Parse CITIC's exchange-notice listing page.
///
/// # Errors
///
/// Returns an error when the page contains no stable article IDs.
pub fn parse_citics_list_html(html: &str) -> Result<Vec<AnnouncementListItem>> {
    let document = Html::parse_document(html);
    let item_selector = selector(".bottom-section ul.list > li[data-id]")?;
    let fallback_item_selector = selector("ul.list > li[data-id]")?;
    let anchor_selector = selector("a")?;
    let date_selector = selector("span")?;
    let selected_items = document.select(&item_selector).collect::<Vec<_>>();
    let listing_items = if selected_items.is_empty() {
        document.select(&fallback_item_selector).collect()
    } else {
        selected_items
    };
    let mut items = Vec::new();
    for item in listing_items {
        let Some(article_id) = item.value().attr("data-id") else {
            continue;
        };
        let Some(anchor) = item.select(&anchor_selector).next() else {
            continue;
        };
        let title = normalized_text(&anchor.text().collect::<Vec<_>>().join(" "));
        let published_at = item
            .select(&date_selector)
            .next()
            .map(|date| normalized_text(&date.text().collect::<Vec<_>>().join(" ")))
            .unwrap_or_default();
        if article_id.trim().is_empty() || title.is_empty() || published_at.is_empty() {
            continue;
        }
        items.push(AnnouncementListItem {
            source: AnnouncementSource::Citics,
            article_id: article_id.trim().to_owned(),
            title,
            published_at,
            url: format!("{CITICS_DETAIL_BASE_URL}{}", article_id.trim()),
        });
    }

    if items.is_empty() {
        bail!("CITIC announcement list contains no valid article IDs");
    }
    Ok(items)
}

/// Parse the selected body of a CITIC announcement detail page.
///
/// # Errors
///
/// Returns an error when the expected selected body is absent.
pub fn parse_citics_article_html(
    item: &AnnouncementListItem,
    html: &str,
) -> Result<AnnouncementDocument> {
    parse_article_body(
        item,
        html,
        ".detailed-l3 .TRS_Editor, .detailed-l3 .InfoContent",
    )
}

/// Parse one HTFC `funcNo=2000065` API response.
///
/// # Errors
///
/// Returns an error when the response reports failure or has no valid records.
pub fn parse_htfc_api_page(json: &str) -> Result<Vec<AnnouncementListItem>> {
    let response: HtfcResponse = serde_json::from_str(json).context("invalid HTFC API JSON")?;
    if response.error_no.as_deref() != Some("0") {
        bail!("HTFC API rejected announcement query");
    }
    let data = response
        .results
        .first()
        .and_then(|result| result.data.as_ref())
        .ok_or_else(|| anyhow!("HTFC API response is missing result data"))?;
    let items = match data {
        HtfcData::Rows(rows) => rows
            .iter()
            .filter_map(|row| htfc_item(&row.article_id, &row.publish_date, &row.title, &row.url))
            .collect(),
        HtfcData::LegacyText(data) => parse_htfc_legacy_records(data)?,
    };

    if items.is_empty() {
        bail!("HTFC announcement page contains no valid records");
    }
    Ok(items)
}

fn parse_htfc_legacy_records(data: &str) -> Result<Vec<AnnouncementListItem>> {
    let records = data.lines().skip(1).collect::<Vec<_>>().join("\n");
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(records.as_bytes());
    let mut items = Vec::new();
    for record in reader.records() {
        let record = record.context("invalid HTFC announcement row")?;
        if let Some(item) = htfc_item(
            record.get(0).unwrap_or_default(),
            record.get(8).unwrap_or_default(),
            record.get(9).unwrap_or_default(),
            record.get(11).unwrap_or_default(),
        ) {
            items.push(item);
        }
    }
    Ok(items)
}

fn htfc_item(
    article_id: &str,
    published_at: &str,
    title: &str,
    relative_url: &str,
) -> Option<AnnouncementListItem> {
    let article_id = article_id.trim();
    let published_at = published_at.trim();
    let title = normalized_text(title);
    let relative_url = relative_url.trim();
    if article_id.is_empty()
        || published_at.is_empty()
        || title.is_empty()
        || relative_url.is_empty()
    {
        return None;
    }
    let url = reqwest::Url::parse(HTFC_BASE_URL)
        .ok()?
        .join(relative_url)
        .ok()?
        .into();
    Some(AnnouncementListItem {
        source: AnnouncementSource::Htfc,
        article_id: article_id.to_owned(),
        title,
        published_at: published_at.to_owned(),
        url,
    })
}

/// Parse the selected body of an HTFC announcement detail page.
///
/// # Errors
///
/// Returns an error when the expected selected body is absent.
pub fn parse_htfc_article_html(
    item: &AnnouncementListItem,
    html: &str,
) -> Result<AnnouncementDocument> {
    parse_article_body(item, html, ".wz_content")
}

/// Classify a selected broker body as a fee-change candidate.
#[must_use]
pub fn classify_fee_candidate(document: &AnnouncementDocument) -> Option<AnnouncementCandidate> {
    let keywords = FEE_KEYWORDS
        .iter()
        .filter(|keyword| document.body_text.contains(**keyword))
        .map(|keyword| (*keyword).to_owned())
        .collect::<Vec<_>>();
    let has_adjustment = ADJUSTMENT_KEYWORDS
        .iter()
        .any(|keyword| document.body_text.contains(keyword));
    let is_linked_fee_standard = !document.official_urls.is_empty()
        && FEE_STANDARD_PHRASES
            .iter()
            .any(|phrase| document.body_text.contains(phrase));
    (!keywords.is_empty() && (has_adjustment || is_linked_fee_standard)).then(|| {
        AnnouncementCandidate {
            source: document.item.source,
            article_id: document.item.article_id.clone(),
            keywords,
            official_urls: document.official_urls.clone(),
        }
    })
}

fn parse_article_body(
    item: &AnnouncementListItem,
    html: &str,
    body_selector: &str,
) -> Result<AnnouncementDocument> {
    let document = Html::parse_document(html);
    let body_selector = selector(body_selector)?;
    let link_selector = selector("a[href]")?;
    let body = document
        .select(&body_selector)
        .next()
        .ok_or_else(|| anyhow!("announcement selected body is missing"))?;
    let body_html = body.inner_html();
    let body_text = normalized_text(&body.text().collect::<Vec<_>>().join(" "));
    let mut official_urls = body
        .select(&link_selector)
        .filter_map(|link| link.value().attr("href"))
        .filter_map(normalize_official_url)
        .collect::<Vec<_>>();
    official_urls.sort();
    official_urls.dedup();
    Ok(AnnouncementDocument {
        item: item.clone(),
        body_html,
        body_text,
        official_urls,
    })
}

fn selector(value: &str) -> Result<Selector> {
    Selector::parse(value).map_err(|_| anyhow!("invalid static selector: {value}"))
}

fn normalized_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_official_url(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let official = [
        "shfe.com.cn",
        "ine.cn",
        "dce.com.cn",
        "czce.com.cn",
        "cffex.com.cn",
        "gfex.com.cn",
    ];
    official
        .iter()
        .any(|domain| host == *domain || host == format!("www.{domain}"))
        .then(|| url.into())
}

#[derive(Debug, Deserialize)]
struct HtfcResponse {
    error_no: Option<String>,
    results: Vec<HtfcResult>,
}

#[derive(Debug, Deserialize)]
struct HtfcResult {
    data: Option<HtfcData>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HtfcData {
    LegacyText(String),
    Rows(Vec<HtfcRow>),
}

#[derive(Debug, Deserialize)]
struct HtfcRow {
    article_id: String,
    publish_date: String,
    title: String,
    url: String,
}
