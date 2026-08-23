use future_meta_daemon::announcement::{
    AnnouncementListItem, AnnouncementSource, AnnouncementTransport, classify_fee_candidate,
    parse_citics_article_html, parse_citics_list_html, parse_htfc_api_page,
    parse_htfc_article_html, scan_announcements,
};
use future_meta_daemon::db::{
    announcement_health, connect, ensure_schema, record_announcement_candidate,
    record_announcement_document, record_announcement_source_success,
    resolve_announcement_candidates_for_official_urls,
};
use std::cell::RefCell;
use std::collections::BTreeMap;

const CITICS_LIST: &str = r#"
<ul class="list">
  <li data-id="825999"><a>上期所：关于调整纸浆期货交易手续费的通知</a><span>2026-08-21</span></li>
</ul>
"#;

const CITICS_LIST_WITH_NAVIGATION_NOISE: &str = r#"
<nav><li data-id="not-an-exchange-notice"><a>公司手续费标准</a><span>2026-08-22</span></li></nav>
<section class="bottom-section"><ul class="list">
  <li data-id="825999"><a>上期所：关于调整纸浆期货交易手续费的通知</a><span>2026-08-21</span></li>
</ul></section>
"#;

const CITICS_ARTICLE: &str = r#"
<main class="detailed-l3">
  <div class="TRS_Editor">
    <p>自2026年8月24日起调整纸浆期货交易手续费。</p>
    <p><a href="https://www.shfe.com.cn/publicnotice/notice/202608/t20260821_833068.html">交易所原文</a></p>
  </div>
</main>
<footer>公司手续费标准</footer>
"#;

const CITICS_FOOTER_ONLY: &str = r#"
<main class="detailed-l3"><div class="TRS_Editor"><p>市场风险提示。</p></div></main>
<footer>公司手续费标准</footer>
"#;

const CITICS_LEGACY_ARTICLE: &str = r#"
<main>
  <div class="detailed-l3">
    <div class="zhengwen"><div class="content"><div class="InfoContent">
      <p>广期所：关于调整多晶硅期货相关合约交易手续费的通知。</p>
      <a href="https://www.gfex.com.cn/gfex/tzts/202608/notice.shtml">交易所原文</a>
    </div></div></div>
  </div>
</main>
<footer>公司手续费标准</footer>
"#;

const HTFC_PAGE: &str = r#"{
  "error_no": "0",
  "results": [{
    "data": "[1]{article_id:string,author:string,brief:string,create_date:string,link_url:string,modified_date:string,newbrief:string,picture_url:string,publish_date:string,title:string,type:string,url:string}\n80180995,华泰期货有限公司,,2026-07-09,,2026-07-09 16:02:52,,,2026-07-09,关于调整苯乙烯等期货相关合约交易手续费标准的通知,0,/main/a/20260709/80180995.shtml\n"
  }]
}"#;

const HTFC_PAGE_WITH_ARRAY_DATA: &str = r#"{
  "error_no": "0",
  "results": [{
    "currentPage": 1,
    "totalPages": 149,
    "data": [{
      "article_id": "80181370",
      "author": "华泰期货有限公司",
      "publish_date": "2026-08-20",
      "title": "关于华泰期货尊享版基金商城恢复服务的通知",
      "url": "/main/a/20260820/80181370.shtml"
    }]
  }]
}"#;

const HTFC_ARTICLE: &str = r#"
<div class="wz_content">
  <p>接大连商品交易所通知，自2026年7月10日交易时起，对苯乙烯期货品种手续费费率进行调整。</p>
  <a href="http://www.dce.com.cn/dce/content/2026/ywggytz/18631364.html">交易所官网通知</a>
</div>
"#;

#[test]
fn citics_selected_body_creates_fee_candidate_with_official_link() {
    let items = parse_citics_list_html(CITICS_LIST).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].source, AnnouncementSource::Citics);
    assert_eq!(items[0].article_id, "825999");
    assert_eq!(
        items[0].url,
        "https://www.citicsf.com/e-futures/content/000509/825999"
    );

    let document = parse_citics_article_html(&items[0], CITICS_ARTICLE).unwrap();
    let candidate = classify_fee_candidate(&document).expect("selected body has a fee change");

    assert!(candidate.keywords.iter().any(|keyword| keyword == "手续费"));
    assert_eq!(
        candidate.official_urls,
        vec!["https://www.shfe.com.cn/publicnotice/notice/202608/t20260821_833068.html"]
    );
    assert!(!document.body_text.contains("公司手续费标准"));
}

#[test]
fn citics_legacy_detail_body_is_classified_without_using_footer_text() {
    let item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let document = parse_citics_article_html(&item, CITICS_LEGACY_ARTICLE).unwrap();

    assert!(
        document
            .body_text
            .contains("调整多晶硅期货相关合约交易手续费")
    );
    assert!(!document.body_text.contains("公司手续费标准"));
    assert_eq!(
        document.official_urls,
        vec!["https://www.gfex.com.cn/gfex/tzts/202608/notice.shtml"]
    );
    assert!(classify_fee_candidate(&document).is_some());
}

#[test]
fn citics_listing_ignores_navigation_items_with_data_ids() {
    let items = parse_citics_list_html(CITICS_LIST_WITH_NAVIGATION_NOISE).unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].article_id, "825999");
}

#[test]
fn citics_footer_keyword_does_not_create_candidate() {
    let item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let document = parse_citics_article_html(&item, CITICS_FOOTER_ONLY).unwrap();

    assert!(classify_fee_candidate(&document).is_none());
}

#[test]
fn expiry_and_delivery_reminders_do_not_create_fee_change_candidates() {
    let item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    for body in [
        "<main class=\"detailed-l3\"><div class=\"TRS_Editor\"><p>股指期权到期提示：行权手续费作为最低盈利金额。</p></div></main>",
        "<main class=\"detailed-l3\"><div class=\"TRS_Editor\"><p>国债期货交割提示：对冲平仓按成交收取手续费。</p></div></main>",
    ] {
        let document = parse_citics_article_html(&item, body).unwrap();
        assert!(classify_fee_candidate(&document).is_none());
    }
}

#[test]
fn htfc_api_and_selected_body_preserve_exchange_original_link() {
    let items = parse_htfc_api_page(HTFC_PAGE).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].source, AnnouncementSource::Htfc);
    assert_eq!(items[0].article_id, "80180995");
    assert_eq!(items[0].published_at, "2026-07-09");

    let document = parse_htfc_article_html(&items[0], HTFC_ARTICLE).unwrap();
    let candidate = classify_fee_candidate(&document).expect("HTFC fee notice");

    assert_eq!(
        candidate.official_urls,
        vec!["http://www.dce.com.cn/dce/content/2026/ywggytz/18631364.html"]
    );
}

#[test]
fn htfc_api_accepts_current_object_array_response() {
    let items = parse_htfc_api_page(HTFC_PAGE_WITH_ARRAY_DATA).unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].article_id, "80181370");
    assert_eq!(items[0].published_at, "2026-08-20");
}

#[test]
fn announcement_state_is_idempotent_and_retains_changed_body_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("announcements.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let document = parse_citics_article_html(&item, CITICS_ARTICLE).unwrap();
    let candidate = classify_fee_candidate(&document).unwrap();

    assert!(record_announcement_document(&conn, &document, "2026-08-21T12:00:00+08:00").unwrap());
    assert!(!record_announcement_document(&conn, &document, "2026-08-21T12:01:00+08:00").unwrap());
    record_announcement_candidate(&conn, &candidate, "2026-08-21T12:00:00+08:00").unwrap();
    record_announcement_source_success(
        &conn,
        AnnouncementSource::Citics,
        Some("2026-08-21"),
        "2026-08-21T12:00:00+08:00",
    )
    .unwrap();

    let mut revised = document.clone();
    revised.body_html.push_str("<p>修订说明</p>");
    revised.body_text.push_str(" 修订说明");
    assert!(record_announcement_document(&conn, &revised, "2026-08-21T12:02:00+08:00").unwrap());

    let snapshots: i64 = conn
        .query_row(
            "select count(*) from announcement_document_snapshots",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let candidates: i64 = conn
        .query_row("select count(*) from announcement_candidates", [], |row| {
            row.get(0)
        })
        .unwrap();
    let source_watermark: String = conn
        .query_row(
            "select last_published_at from announcement_source_state where source = 'citics'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(snapshots, 2);
    assert_eq!(candidates, 1);
    assert_eq!(source_watermark, "2026-08-21");
}

#[test]
fn candidate_inherits_resolution_from_a_matching_official_url() {
    let dir = tempfile::tempdir().unwrap();
    let conn = connect(&dir.path().join("announcements.sqlite")).unwrap();
    ensure_schema(&conn).unwrap();
    let citics_item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let citics_document = parse_citics_article_html(&citics_item, CITICS_ARTICLE).unwrap();
    let citics_candidate = classify_fee_candidate(&citics_document).unwrap();
    record_announcement_document(&conn, &citics_document, "2026-08-21T12:00:00+08:00").unwrap();
    record_announcement_candidate(&conn, &citics_candidate, "2026-08-21T12:00:00+08:00").unwrap();
    resolve_announcement_candidates_for_official_urls(
        &conn,
        &citics_candidate.official_urls,
        "2026-08-21T12:30:00+08:00",
    )
    .unwrap();

    let htfc_item = AnnouncementListItem {
        source: AnnouncementSource::Htfc,
        article_id: "80181408".to_owned(),
        title: "关于调整纸浆期货交易手续费的通知".to_owned(),
        published_at: "2026-08-21".to_owned(),
        url: "https://htfc.com/main/a/20260821/80181408.shtml".to_owned(),
    };
    let htfc_html = format!(
        "<div class=\"wz_content\"><p>调整纸浆期货交易手续费。</p><a href=\"{}\">交易所原文</a></div>",
        citics_candidate.official_urls[0]
    );
    let htfc_document = parse_htfc_article_html(&htfc_item, &htfc_html).unwrap();
    let htfc_candidate = classify_fee_candidate(&htfc_document).unwrap();
    record_announcement_document(&conn, &htfc_document, "2026-08-23T12:00:00+08:00").unwrap();
    record_announcement_candidate(&conn, &htfc_candidate, "2026-08-23T12:00:00+08:00").unwrap();

    let resolved_at: Option<String> = conn
        .query_row(
            "select resolved_at from announcement_candidates where source = 'htfc' and article_id = '80181408'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(resolved_at.as_deref(), Some("2026-08-23T12:00:00+08:00"));
}

#[test]
fn candidate_with_distinct_official_url_remains_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let conn = connect(&dir.path().join("announcements.sqlite")).unwrap();
    ensure_schema(&conn).unwrap();
    let item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let document = parse_citics_article_html(&item, CITICS_ARTICLE).unwrap();
    let candidate = classify_fee_candidate(&document).unwrap();
    record_announcement_document(&conn, &document, "2026-08-21T12:00:00+08:00").unwrap();
    record_announcement_candidate(&conn, &candidate, "2026-08-21T12:00:00+08:00").unwrap();

    let resolved_at: Option<String> = conn
        .query_row(
            "select resolved_at from announcement_candidates where source = 'citics' and article_id = '825999'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(resolved_at.is_none());
}

#[test]
fn announcement_health_rejects_candidate_unresolved_for_more_than_a_day() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("announcements.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let document = parse_citics_article_html(&item, CITICS_ARTICLE).unwrap();
    let candidate = classify_fee_candidate(&document).unwrap();
    record_announcement_document(&conn, &document, "2026-08-21T00:00:00+08:00").unwrap();
    record_announcement_candidate(&conn, &candidate, "2026-08-21T00:00:00+08:00").unwrap();
    record_announcement_source_success(
        &conn,
        AnnouncementSource::Citics,
        Some("2026-08-23"),
        "2026-08-23T00:00:00+08:00",
    )
    .unwrap();

    let error = announcement_health(&conn, "2026-08-23T00:30:00+08:00").unwrap_err();

    assert!(error.to_string().contains("unresolved fee candidate"));
    assert!(error.to_string().contains("825999"));
}

#[derive(Default)]
struct FakeTransport {
    lists: BTreeMap<(AnnouncementSource, usize), Vec<AnnouncementListItem>>,
    bodies: BTreeMap<(AnnouncementSource, String), String>,
    official_bodies: BTreeMap<String, String>,
    calls: RefCell<Vec<(AnnouncementSource, usize)>>,
}

impl AnnouncementTransport for FakeTransport {
    fn fetch_list(
        &self,
        source: AnnouncementSource,
        page: usize,
    ) -> anyhow::Result<Vec<AnnouncementListItem>> {
        self.calls.borrow_mut().push((source, page));
        self.lists
            .get(&(source, page))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing fake list"))
    }

    fn fetch_body(&self, item: &AnnouncementListItem) -> anyhow::Result<String> {
        self.bodies
            .get(&(item.source, item.article_id.clone()))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing fake body"))
    }

    fn fetch_official(&self, url: &str) -> anyhow::Result<String> {
        self.official_bodies
            .get(url)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing fake official body"))
    }
}

#[test]
fn announcement_scan_uses_citics_first_and_only_uses_htfc_for_requested_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("announcements.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let citics_item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let boundary_item = AnnouncementListItem {
        article_id: "825998".to_owned(),
        title: "市场风险提示".to_owned(),
        published_at: "2026-07-01".to_owned(),
        url: "https://www.citicsf.com/e-futures/content/000509/825998".to_owned(),
        source: AnnouncementSource::Citics,
    };
    let mut htfc_item = parse_htfc_api_page(HTFC_PAGE).unwrap().remove(0);
    htfc_item.published_at = "2026-08-20".to_owned();
    let mut htfc_boundary = htfc_item.clone();
    htfc_boundary.article_id = "80179999".to_owned();
    htfc_boundary.published_at = "2026-07-01".to_owned();
    let transport = FakeTransport {
        lists: BTreeMap::from([
            ((AnnouncementSource::Citics, 1), vec![citics_item.clone()]),
            ((AnnouncementSource::Citics, 2), vec![boundary_item]),
            ((AnnouncementSource::Htfc, 1), vec![htfc_item.clone()]),
            ((AnnouncementSource::Htfc, 2), vec![htfc_boundary]),
        ]),
        bodies: BTreeMap::from([
            (
                (AnnouncementSource::Citics, citics_item.article_id.clone()),
                CITICS_ARTICLE.to_owned(),
            ),
            (
                (AnnouncementSource::Citics, "825998".to_owned()),
                CITICS_FOOTER_ONLY.to_owned(),
            ),
            (
                (AnnouncementSource::Htfc, htfc_item.article_id.clone()),
                HTFC_ARTICLE.to_owned(),
            ),
        ]),
        ..FakeTransport::default()
    };

    let first = scan_announcements(&conn, &transport, "2026-08-23T00:00:00+08:00", false).unwrap();
    assert_eq!(first.candidates, 1);
    assert_eq!(
        transport.calls.borrow().as_slice(),
        &[
            (AnnouncementSource::Citics, 1),
            (AnnouncementSource::Citics, 2)
        ]
    );

    transport.calls.borrow_mut().clear();
    let reconciled =
        scan_announcements(&conn, &transport, "2026-08-23T01:00:00+08:00", true).unwrap();
    assert_eq!(reconciled.candidates, 1);
    assert_eq!(
        transport.calls.borrow().as_slice(),
        &[
            (AnnouncementSource::Citics, 1),
            (AnnouncementSource::Citics, 2),
            (AnnouncementSource::Htfc, 1),
            (AnnouncementSource::Htfc, 2),
        ]
    );
}

#[test]
fn announcement_scan_does_not_persist_items_at_or_before_the_floor() {
    let dir = tempfile::tempdir().unwrap();
    let conn = connect(&dir.path().join("announcements.sqlite")).unwrap();
    ensure_schema(&conn).unwrap();
    record_announcement_source_success(
        &conn,
        AnnouncementSource::Citics,
        Some("2026-08-21"),
        "2026-08-21T12:00:00+08:00",
    )
    .unwrap();

    let current = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let at_floor = AnnouncementListItem {
        article_id: "825900".to_owned(),
        title: "历史手续费通知".to_owned(),
        published_at: "2026-08-07".to_owned(),
        url: "https://www.citicsf.com/e-futures/content/000509/825900".to_owned(),
        source: AnnouncementSource::Citics,
    };
    let before_floor = AnnouncementListItem {
        article_id: "825899".to_owned(),
        title: "更早手续费通知".to_owned(),
        published_at: "2026-08-06".to_owned(),
        url: "https://www.citicsf.com/e-futures/content/000509/825899".to_owned(),
        source: AnnouncementSource::Citics,
    };
    let transport = FakeTransport {
        lists: BTreeMap::from([
            (
                (AnnouncementSource::Citics, 1),
                vec![current.clone(), at_floor.clone()],
            ),
            ((AnnouncementSource::Citics, 2), vec![before_floor.clone()]),
        ]),
        bodies: BTreeMap::from([
            (
                (AnnouncementSource::Citics, current.article_id.clone()),
                CITICS_ARTICLE.to_owned(),
            ),
            (
                (AnnouncementSource::Citics, at_floor.article_id.clone()),
                CITICS_ARTICLE.to_owned(),
            ),
            (
                (AnnouncementSource::Citics, before_floor.article_id.clone()),
                CITICS_ARTICLE.to_owned(),
            ),
        ]),
        ..FakeTransport::default()
    };

    let summary =
        scan_announcements(&conn, &transport, "2026-08-23T00:00:00+08:00", false).unwrap();

    assert_eq!(summary.documents, 1);
    assert_eq!(summary.candidates, 1);
    let stored: i64 = conn
        .query_row("select count(*) from announcement_documents", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, 1);
}

#[test]
fn announcement_scan_falls_back_to_htfc_when_citics_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("announcements.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut htfc_item = parse_htfc_api_page(HTFC_PAGE).unwrap().remove(0);
    htfc_item.published_at = "2026-08-20".to_owned();
    let mut htfc_boundary = htfc_item.clone();
    htfc_boundary.article_id = "80179999".to_owned();
    htfc_boundary.published_at = "2026-07-01".to_owned();
    let transport = FakeTransport {
        lists: BTreeMap::from([
            ((AnnouncementSource::Htfc, 1), vec![htfc_item.clone()]),
            ((AnnouncementSource::Htfc, 2), vec![htfc_boundary]),
        ]),
        bodies: BTreeMap::from([(
            (AnnouncementSource::Htfc, htfc_item.article_id.clone()),
            HTFC_ARTICLE.to_owned(),
        )]),
        ..FakeTransport::default()
    };

    let summary =
        scan_announcements(&conn, &transport, "2026-08-23T00:00:00+08:00", false).unwrap();

    assert!(summary.used_fallback);
    assert_eq!(summary.sources, vec![AnnouncementSource::Htfc]);
    assert_eq!(summary.candidates, 1);
    assert_eq!(
        transport.calls.borrow().as_slice(),
        &[
            (AnnouncementSource::Citics, 1),
            (AnnouncementSource::Htfc, 1),
            (AnnouncementSource::Htfc, 2)
        ]
    );
    let citics_error: String = conn
        .query_row(
            "select last_error_message from announcement_source_state where source = 'citics'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(citics_error.contains("missing fake list"));
}

#[test]
fn announcement_scan_records_citics_detail_failure_before_falling_back() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("announcements.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let citics_item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let mut htfc_item = parse_htfc_api_page(HTFC_PAGE).unwrap().remove(0);
    htfc_item.published_at = "2026-08-20".to_owned();
    let mut htfc_boundary = htfc_item.clone();
    htfc_boundary.article_id = "80179999".to_owned();
    htfc_boundary.published_at = "2026-07-01".to_owned();
    let transport = FakeTransport {
        lists: BTreeMap::from([
            ((AnnouncementSource::Citics, 1), vec![citics_item.clone()]),
            ((AnnouncementSource::Htfc, 1), vec![htfc_item.clone()]),
            ((AnnouncementSource::Htfc, 2), vec![htfc_boundary]),
        ]),
        bodies: BTreeMap::from([
            (
                (AnnouncementSource::Citics, citics_item.article_id.clone()),
                "<main>selected detail container is absent</main>".to_owned(),
            ),
            (
                (AnnouncementSource::Htfc, htfc_item.article_id.clone()),
                HTFC_ARTICLE.to_owned(),
            ),
        ]),
        ..FakeTransport::default()
    };

    let summary =
        scan_announcements(&conn, &transport, "2026-08-23T00:00:00+08:00", false).unwrap();

    assert!(summary.used_fallback);
    let citics_error: String = conn
        .query_row(
            "select last_error_message from announcement_source_state where source = 'citics'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(citics_error.contains("825999"));
    assert!(citics_error.contains("selected body is missing"));
}

#[test]
fn announcement_scan_retains_exchange_original_snapshot_before_candidate_queueing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("announcements.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let citics_item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let boundary_item = AnnouncementListItem {
        article_id: "825998".to_owned(),
        title: "市场风险提示".to_owned(),
        published_at: "2026-07-01".to_owned(),
        url: "https://www.citicsf.com/e-futures/content/000509/825998".to_owned(),
        source: AnnouncementSource::Citics,
    };
    let official_url = "https://www.shfe.com.cn/publicnotice/notice/202608/t20260821_833068.html";
    let transport = FakeTransport {
        lists: BTreeMap::from([
            ((AnnouncementSource::Citics, 1), vec![citics_item.clone()]),
            ((AnnouncementSource::Citics, 2), vec![boundary_item]),
        ]),
        bodies: BTreeMap::from([
            (
                (AnnouncementSource::Citics, citics_item.article_id.clone()),
                CITICS_ARTICLE.to_owned(),
            ),
            (
                (AnnouncementSource::Citics, "825998".to_owned()),
                CITICS_FOOTER_ONLY.to_owned(),
            ),
        ]),
        official_bodies: BTreeMap::from([(
            official_url.to_owned(),
            "<html><body>上期所正式手续费公告</body></html>".to_owned(),
        )]),
        ..FakeTransport::default()
    };

    scan_announcements(&conn, &transport, "2026-08-23T00:00:00+08:00", false).unwrap();

    let snapshot: (String, String) = conn
        .query_row(
            "select canonical_url, body from official_document_snapshots",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(snapshot.0, official_url);
    assert_eq!(snapshot.1, "<html><body>上期所正式手续费公告</body></html>");
}

#[test]
fn announcement_scan_retries_missing_official_snapshot_for_existing_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("announcements.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let citics_item = parse_citics_list_html(CITICS_LIST).unwrap().remove(0);
    let boundary_item = AnnouncementListItem {
        article_id: "825998".to_owned(),
        title: "市场风险提示".to_owned(),
        published_at: "2026-07-01".to_owned(),
        url: "https://www.citicsf.com/e-futures/content/000509/825998".to_owned(),
        source: AnnouncementSource::Citics,
    };
    let lists = BTreeMap::from([
        ((AnnouncementSource::Citics, 1), vec![citics_item.clone()]),
        ((AnnouncementSource::Citics, 2), vec![boundary_item]),
    ]);
    let bodies = BTreeMap::from([
        (
            (AnnouncementSource::Citics, citics_item.article_id.clone()),
            CITICS_ARTICLE.to_owned(),
        ),
        (
            (AnnouncementSource::Citics, "825998".to_owned()),
            CITICS_FOOTER_ONLY.to_owned(),
        ),
    ]);
    let missing_official = FakeTransport {
        lists: lists.clone(),
        bodies: bodies.clone(),
        ..FakeTransport::default()
    };
    scan_announcements(&conn, &missing_official, "2026-08-23T00:00:00+08:00", false).unwrap();

    let official_url = "https://www.shfe.com.cn/publicnotice/notice/202608/t20260821_833068.html";
    let recovered_official = FakeTransport {
        lists,
        bodies,
        official_bodies: BTreeMap::from([(
            official_url.to_owned(),
            "<html><body>已恢复的交易所原文</body></html>".to_owned(),
        )]),
        ..FakeTransport::default()
    };
    scan_announcements(
        &conn,
        &recovered_official,
        "2026-08-23T01:00:00+08:00",
        false,
    )
    .unwrap();

    let snapshots: i64 = conn
        .query_row(
            "select count(*) from official_document_snapshots",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(snapshots, 1);
}
