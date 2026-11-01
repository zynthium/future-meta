#![cfg(feature = "download")]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use future_meta::archive::{encode_archive_bytes, sha256_hex};
use future_meta::{DownloadClient, DownloadConfig, FutureMetaError, load_or_fetch};
use serde_json::json;

const SYMBOL: &str = "SHFE.cu2607";
const AT: &str = "2026-06-04T12:00:00+08:00";

fn archive(value: f64) -> Vec<u8> {
    let fee = json!({"kind": "CnyPerLot", "value": value, "raw_text": null});
    let payload: future_meta::FeeArchiveV3 = serde_json::from_value(json!({
        "schema_version": future_meta::model::SCHEMA_VERSION,
        "generated_at": AT, "history_start": AT, "history_end": AT,
        "contracts": [{"id": 1, "symbol": SYMBOL, "listing_date": "20250101",
            "expiry_date": "20261231", "lot_size": 5.0, "tick_size": 10.0, "active": true}],
        "contract_spec_versions": [{"contract_id": 1, "lot_size": 5.0, "tick_size": 10.0,
            "valid_from": "2025-01-01T00:00:00+08:00", "valid_to": null}],
        "fee_versions": [{"contract_id": 1, "rule_hash": "test", "buy_margin_rate": 12.0,
            "sell_margin_rate": 12.0, "open_fee": fee, "close_yesterday_fee": fee,
            "close_today_fee": fee, "intraday_open_fee": null, "trading_status": "Unknown",
            "is_main_contract": false, "source_updated_at": null,
            "valid_from": AT, "valid_to": null}]
    }))
    .unwrap();
    encode_archive_bytes(&payload).unwrap()
}

fn manifest(base: &str, body: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version": future_meta::model::SCHEMA_VERSION,
        "data_version": "test", "generated_at": AT, "history_start": AT, "history_end": AT,
        "fee_effective_from": AT, "artifact": "latest.fmeta.zst", "sha256": sha256_hex(body),
        "size": body.len(), "mirrors": [format!("{base}/archive")]
    }))
    .unwrap()
}

struct Reply {
    path: &'static str,
    status: u16,
    body: Vec<u8>,
    etag: Option<&'static str>,
    expected_etag: Option<&'static str>,
}

fn reply(
    path: &'static str,
    status: u16,
    body: Vec<u8>,
    etag: Option<&'static str>,
    expected_etag: Option<&'static str>,
) -> Reply {
    Reply {
        path,
        status,
        body,
        etag,
        expected_etag,
    }
}

struct Server {
    url: String,
    thread: JoinHandle<()>,
}

impl Server {
    fn start(plan: impl FnOnce(&str) -> Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let replies = plan(&base);
        let thread = thread::spawn(move || {
            // All requests must reuse this connection, including refreshes.
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            for response in replies {
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert_eq!(request.trim(), format!("GET {} HTTP/1.1", response.path));
                let mut etag = None;
                loop {
                    let mut header = String::new();
                    assert!(reader.read_line(&mut header).unwrap() > 0);
                    if header == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("if-none-match")
                    {
                        etag = Some(value.trim().to_owned());
                    }
                }
                assert_eq!(etag.as_deref(), response.expected_etag);
                write!(
                    socket,
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: keep-alive\r\n",
                    response.status,
                    response.body.len()
                )
                .unwrap();
                if let Some(etag) = response.etag {
                    write!(socket, "ETag: {etag}\r\n").unwrap();
                }
                socket.write_all(b"\r\n").unwrap();
                socket.write_all(&response.body).unwrap();
                socket.flush().unwrap();
            }
        });
        Self {
            url: format!("{base}/manifest"),
            thread,
        }
    }

    fn config(&self, cache_dir: &std::path::Path) -> DownloadConfig {
        DownloadConfig {
            manifest_url: self.url.clone(),
            cache_dir: cache_dir.to_path_buf(),
        }
    }

    fn finish(self) {
        self.thread.join().unwrap();
    }
}

#[tokio::test]
async fn unchanged_sha_and_304_preserve_handles_without_touching_disk() {
    let bytes = archive(1.0);
    let server = Server::start(|base| {
        vec![
            reply(
                "/manifest",
                200,
                manifest(base, &bytes),
                Some("\"v1\""),
                None,
            ),
            reply("/archive", 200, bytes.clone(), None, None),
            reply(
                "/manifest",
                200,
                manifest(base, &bytes),
                Some("\"v2\""),
                Some("\"v1\""),
            ),
            reply("/manifest", 304, vec![], Some("\"v3\""), Some("\"v2\"")),
            reply(
                "/manifest",
                200,
                manifest(base, &bytes),
                None,
                Some("\"v3\""),
            ),
            reply("/manifest", 200, manifest(base, &bytes), None, None),
        ]
    });
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let mut client = DownloadClient::load(server.config(&cache)).await.unwrap();
    let handle = client.meta().resolve_contract(SYMBOL).unwrap();
    std::fs::remove_dir_all(&cache).unwrap();
    assert!(!client.refresh().await.unwrap());
    assert!(!client.refresh().await.unwrap());
    assert!(!client.refresh().await.unwrap());
    assert!(!client.refresh().await.unwrap());
    assert!(!cache.exists());
    assert_eq!(client.meta().resolve_contract(SYMBOL).unwrap(), handle);
    assert!(client.meta().contract_for_handle(handle).is_ok());
    server.finish();
}

#[tokio::test]
async fn checksum_failure_preserves_snapshot_validator_and_cache_then_retry_replaces() {
    let old = archive(1.0);
    let new = archive(2.0);
    let server = Server::start(|base| {
        vec![
            reply("/manifest", 200, manifest(base, &old), Some("\"v1\""), None),
            reply("/archive", 200, old.clone(), None, None),
            reply(
                "/manifest",
                200,
                manifest(base, &new),
                Some("\"v2\""),
                Some("\"v1\""),
            ),
            reply("/archive", 200, b"wrong checksum".to_vec(), None, None),
            reply(
                "/manifest",
                200,
                manifest(base, &new),
                Some("\"v2\""),
                Some("\"v1\""),
            ),
            reply("/archive", 200, new.clone(), None, None),
        ]
    });
    let temp = tempfile::tempdir().unwrap();
    let mut client = DownloadClient::load(server.config(temp.path()))
        .await
        .unwrap();
    let previous = client.meta().clone();
    let handle = previous.resolve_contract(SYMBOL).unwrap();
    assert!(matches!(
        client.refresh().await,
        Err(FutureMetaError::ChecksumMismatch { .. })
    ));
    assert_eq!(
        std::fs::read(temp.path().join("latest.fmeta.zst")).unwrap(),
        old
    );
    assert!(client.meta().contract_for_handle(handle).is_ok());
    assert!(client.refresh().await.unwrap());
    assert_eq!(
        std::fs::read(temp.path().join("latest.fmeta.zst")).unwrap(),
        new
    );
    assert!(matches!(
        client.meta().contract_for_handle(handle),
        Err(FutureMetaError::InvalidContractHandle)
    ));
    assert!(previous.contract_for_handle(handle).is_ok());
    assert_eq!(
        client
            .meta()
            .contract_fee_asof(SYMBOL, AT)
            .unwrap()
            .open_fee
            .value,
        Some(2.0)
    );
    server.finish();
}

#[tokio::test]
async fn invalid_archive_with_valid_checksum_does_not_replace_good_cache() {
    let old = archive(1.0);
    let bad = b"not a compressed archive";
    let server = Server::start(|base| {
        vec![
            reply("/manifest", 200, manifest(base, &old), Some("\"v1\""), None),
            reply("/archive", 200, old.clone(), None, None),
            reply(
                "/manifest",
                200,
                manifest(base, bad),
                Some("\"bad\""),
                Some("\"v1\""),
            ),
            reply("/archive", 200, bad.to_vec(), None, None),
            reply("/manifest", 304, vec![], None, Some("\"v1\"")),
        ]
    });
    let temp = tempfile::tempdir().unwrap();
    let mut client = DownloadClient::load(server.config(temp.path()))
        .await
        .unwrap();
    let handle = client.meta().resolve_contract(SYMBOL).unwrap();
    assert!(client.refresh().await.is_err());
    assert_eq!(
        std::fs::read(temp.path().join("latest.fmeta.zst")).unwrap(),
        old
    );
    assert!(!client.refresh().await.unwrap());
    assert!(client.meta().contract_for_handle(handle).is_ok());
    server.finish();
}

#[tokio::test]
async fn one_shot_api_uses_verified_disk_cache_without_downloading_archive() {
    let bytes = archive(1.0);
    let server =
        Server::start(|base| vec![reply("/manifest", 200, manifest(base, &bytes), None, None)]);
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("latest.fmeta.zst"), &bytes).unwrap();
    let meta = load_or_fetch(server.config(temp.path())).await.unwrap();
    assert_eq!(
        meta.contract_fee_asof(SYMBOL, AT).unwrap().open_fee.value,
        Some(1.0)
    );
    server.finish();
}

#[tokio::test]
async fn initial_304_without_a_validator_is_an_error() {
    let server = Server::start(|_| vec![reply("/manifest", 304, vec![], None, None)]);
    let temp = tempfile::tempdir().unwrap();
    assert!(matches!(
        DownloadClient::load(server.config(temp.path())).await,
        Err(FutureMetaError::DownloadFailed(_))
    ));
    server.finish();
}

#[tokio::test]
async fn invalid_manifest_metadata_cannot_bypass_validation_with_an_unchanged_sha() {
    let bytes = archive(1.0);
    let server = Server::start(|base| {
        let valid = manifest(base, &bytes);
        let mut bad_schema: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        bad_schema["schema_version"] = json!(future_meta::model::SCHEMA_VERSION + 1);
        let mut bad_path: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        bad_path["artifact"] = json!("../latest.fmeta.zst");
        vec![
            reply("/manifest", 200, valid, Some("\"v1\""), None),
            reply("/archive", 200, bytes.clone(), None, None),
            reply(
                "/manifest",
                200,
                serde_json::to_vec(&bad_schema).unwrap(),
                Some("\"bad\""),
                Some("\"v1\""),
            ),
            reply(
                "/manifest",
                200,
                serde_json::to_vec(&bad_path).unwrap(),
                Some("\"bad\""),
                Some("\"v1\""),
            ),
            reply("/manifest", 304, vec![], None, Some("\"v1\"")),
        ]
    });
    let temp = tempfile::tempdir().unwrap();
    let mut client = DownloadClient::load(server.config(temp.path()))
        .await
        .unwrap();
    let handle = client.meta().resolve_contract(SYMBOL).unwrap();
    assert!(matches!(
        client.refresh().await,
        Err(FutureMetaError::UnsupportedSchemaVersion { .. })
    ));
    assert!(matches!(
        client.refresh().await,
        Err(FutureMetaError::DownloadFailed(_))
    ));
    assert!(!client.refresh().await.unwrap());
    assert!(client.meta().contract_for_handle(handle).is_ok());
    assert_eq!(
        std::fs::read(temp.path().join("latest.fmeta.zst")).unwrap(),
        bytes
    );
    server.finish();
}

#[tokio::test]
async fn cache_write_failure_preserves_current_snapshot_and_successful_retry_updates_it() {
    let old = archive(1.0);
    let new = archive(2.0);
    let server = Server::start(|base| {
        vec![
            reply("/manifest", 200, manifest(base, &old), Some("\"v1\""), None),
            reply("/archive", 200, old.clone(), None, None),
            reply(
                "/manifest",
                200,
                manifest(base, &new),
                Some("\"v2\""),
                Some("\"v1\""),
            ),
            reply("/archive", 200, new.clone(), None, None),
            reply(
                "/manifest",
                200,
                manifest(base, &new),
                Some("\"v2\""),
                Some("\"v1\""),
            ),
            reply("/archive", 200, new.clone(), None, None),
        ]
    });
    let temp = tempfile::tempdir().unwrap();
    let mut client = DownloadClient::load(server.config(temp.path()))
        .await
        .unwrap();
    let handle = client.meta().resolve_contract(SYMBOL).unwrap();
    let blocked = temp.path().join("latest.fmeta.tmp");
    std::fs::create_dir(&blocked).unwrap();
    assert!(matches!(
        client.refresh().await,
        Err(FutureMetaError::Io(_))
    ));
    assert!(client.meta().contract_for_handle(handle).is_ok());
    assert_eq!(
        std::fs::read(temp.path().join("latest.fmeta.zst")).unwrap(),
        old
    );
    std::fs::remove_dir(blocked).unwrap();
    assert!(client.refresh().await.unwrap());
    assert_eq!(
        std::fs::read(temp.path().join("latest.fmeta.zst")).unwrap(),
        new
    );
    server.finish();
}
