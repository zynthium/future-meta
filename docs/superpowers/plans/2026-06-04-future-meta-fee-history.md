# Future Meta Fee History Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the first usable `future-meta` system: daemon-maintained futures fee-rule history, compressed binary archive publishing, and local high-performance client as-of queries using TqSdk-style symbols.

**Architecture:** `future-meta-daemon` fetches 9qihuo, normalizes only non-derived rule fields, maintains a SQLite SCD2-style history, and exports `manifest.json` plus `latest.fmeta.zst`. `future-meta` defines shared models, archive encoding, symbol parsing, local indexes, optional download/cache support, and as-of APIs. GitHub Actions runs the daemon; Cloudflare Pages serves static artifacts on the free tier.

**Tech Stack:** Rust 2024 workspace, `serde`, `bincode 2`, `zstd`, `sha2`, `time`, `thiserror`, optional `reqwest`/`tokio` client download, daemon `clap`, `csv`, `scraper`, `rusqlite`, GitHub Actions, Cloudflare Pages via Wrangler.

---

## Scope Check

This plan implements the approved spec in one coherent first version. It has three connected subsystems, but each task produces independently testable software:

- `future-meta` client/library core.
- `future-meta-daemon` ingestion/history/export.
- CI/deployment files for GitHub Actions and Cloudflare Pages.

Current workspace note: this directory currently reports `fatal: not a git repository`. Commit steps are still included. If `git rev-parse --is-inside-work-tree` fails during execution, skip the commit and list changed files in the task handoff.

## File Structure

Create or modify these files:

- `Cargo.toml`: workspace dependencies and shared lints.
- `.gitignore`: ignore generated daemon data and Pages output.
- `crates/future-meta/Cargo.toml`: library dependencies and `download` feature.
- `crates/future-meta/src/lib.rs`: public module exports and top-level `FutureMeta`.
- `crates/future-meta/src/model.rs`: archive, manifest, contract, fee, and version data types.
- `crates/future-meta/src/error.rs`: client/library error types.
- `crates/future-meta/src/symbol.rs`: TqSdk symbol parser and `underlying_symbol` derivation.
- `crates/future-meta/src/fee.rs`: fee string parser and `FeeSpec` normalization.
- `crates/future-meta/src/archive.rs`: bincode/zstd encode/decode and sha256 helpers.
- `crates/future-meta/src/query.rs`: in-memory indexes and as-of query methods.
- `crates/future-meta/src/download.rs`: optional manifest/artifact download and local cache.
- `crates/future-meta/tests/client_archive.rs`: archive and query integration tests.
- `crates/future-meta-daemon/Cargo.toml`: daemon dependencies.
- `crates/future-meta-daemon/src/main.rs`: CLI entrypoint.
- `crates/future-meta-daemon/src/lib.rs`: daemon modules for tests.
- `crates/future-meta-daemon/src/source.rs`: HTML discovery and fetch abstraction.
- `crates/future-meta-daemon/src/parse.rs`: CSV parsing into allowed-field rows.
- `crates/future-meta-daemon/src/hash.rs`: canonical allowed-field hashing.
- `crates/future-meta-daemon/src/db.rs`: SQLite schema and version maintenance.
- `crates/future-meta-daemon/src/refresh.rs`: refresh orchestration.
- `crates/future-meta-daemon/src/export.rs`: archive and manifest export.
- `crates/future-meta-daemon/tests/daemon_pipeline.rs`: fixture-driven daemon integration tests.
- `.github/workflows/update-fee-data.yml`: scheduled refresh/export/deploy workflow.

## Implementation Tasks

### Task 1: Workspace Dependencies and Module Shells

**Files:**
- Modify: `Cargo.toml`
- Modify: `.gitignore`
- Modify: `crates/future-meta/Cargo.toml`
- Modify: `crates/future-meta/src/lib.rs`
- Modify: `crates/future-meta-daemon/Cargo.toml`
- Modify: `crates/future-meta-daemon/src/main.rs`
- Create: `crates/future-meta-daemon/src/lib.rs`

- [ ] **Step 1: Add workspace dependencies**

Update root `Cargo.toml` to include:

```toml
[workspace.dependencies]
anyhow = "1"
bincode = { version = "2.0.1", features = ["serde"] }
clap = { version = "4.5", features = ["derive"] }
csv = "1.3"
hex = "0.4"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
rusqlite = { version = "0.32", features = ["bundled"] }
scraper = "0.22"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
tempfile = "3"
thiserror = "2"
time = { version = "0.3", features = ["formatting", "parsing", "serde"] }
tokio = { version = "1", features = ["fs", "io-util", "macros", "rt-multi-thread"] }
zstd = "0.13"
```

Expected: workspace still has existing `[workspace]`, `[workspace.package]`, and lint sections.

- [ ] **Step 2: Ignore generated artifacts**

Update `.gitignore` to include:

```gitignore
/target
/data
/public
/.env
```

Expected: generated SQLite and Cloudflare Pages output are not accidentally committed.

- [ ] **Step 3: Configure `future-meta` crate**

Update `crates/future-meta/Cargo.toml`:

```toml
[package]
name = "future-meta"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true

[lib]
path = "src/lib.rs"

[features]
default = []
download = ["dep:reqwest", "dep:tokio"]

[dependencies]
bincode.workspace = true
hex.workspace = true
reqwest = { workspace = true, optional = true }
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
time.workspace = true
tokio = { workspace = true, optional = true }
zstd.workspace = true

[dev-dependencies]
tempfile.workspace = true
tokio.workspace = true

[lints]
workspace = true
```

- [ ] **Step 4: Configure `future-meta-daemon` crate**

Update `crates/future-meta-daemon/Cargo.toml`:

```toml
[package]
name = "future-meta-daemon"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "future-meta-daemon"
path = "src/main.rs"

[dependencies]
anyhow.workspace = true
clap.workspace = true
csv.workspace = true
future-meta = { path = "../future-meta" }
hex.workspace = true
reqwest = { workspace = true, features = ["blocking"] }
rusqlite.workspace = true
scraper.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
time.workspace = true

[dev-dependencies]
tempfile.workspace = true

[lints]
workspace = true
```

- [ ] **Step 5: Create library module shells**

Replace `crates/future-meta/src/lib.rs` with:

```rust
#![doc = "Shared data types and high-performance client APIs for future-meta."]

pub mod archive;
#[cfg(feature = "download")]
pub mod download;
pub mod error;
pub mod fee;
pub mod model;
pub mod query;
pub mod symbol;

pub use crate::error::{AsOfError, FutureMetaError};
pub use crate::model::{Contract, ContractFee, FeeArchiveV1, FeeSpec, Manifest};
pub use crate::query::FutureMeta;

/// Crate version exported for clients and compatibility checks.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
```

Create empty module files with a single module-level doc comment:

```rust
//! Module implementation is added by the implementation plan.
```

Files: `archive.rs`, `error.rs`, `fee.rs`, `model.rs`, `query.rs`, `symbol.rs`, `download.rs`.

- [ ] **Step 6: Create daemon module shells**

Replace `crates/future-meta-daemon/src/main.rs` with:

```rust
fn main() -> anyhow::Result<()> {
    future_meta_daemon::run()
}
```

Create `crates/future-meta-daemon/src/lib.rs`:

```rust
pub mod db;
pub mod export;
pub mod hash;
pub mod parse;
pub mod refresh;
pub mod source;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "future-meta-daemon")]
#[command(about = "Maintain and export future-meta fee history")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Discover { #[arg(long)] out: PathBuf },
    Refresh {
        #[arg(long)] db: PathBuf,
        #[arg(long)] force_full: bool,
    },
    Export {
        #[arg(long)] db: PathBuf,
        #[arg(long)] out: PathBuf,
    },
    Inspect { #[arg(long)] db: PathBuf },
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Discover { out } => source::discover_to_file(&out),
        Command::Refresh { db, force_full } => refresh::refresh(&db, force_full),
        Command::Export { db, out } => export::export_archive(&db, &out),
        Command::Inspect { db } => db::inspect(&db),
    }
}
```

Create empty daemon module files with:

```rust
//! Module implementation is added by the implementation plan.
```

- [ ] **Step 7: Run compile check**

Run:

```bash
cargo test --workspace
```

Expected: compile fails because referenced daemon functions are not implemented. This confirms module wiring is active.

- [ ] **Step 8: Add temporary stubs to pass compile**

In daemon modules add temporary public stubs:

```rust
use anyhow::Result;
use std::path::Path;

pub fn discover_to_file(_out: &Path) -> Result<()> {
    Ok(())
}
```

Use the matching signatures:

- `refresh::refresh(_db: &Path, _force_full: bool) -> Result<()>`
- `export::export_archive(_db: &Path, _out: &Path) -> Result<()>`
- `db::inspect(_db: &Path) -> Result<()>`

Run:

```bash
cargo test --workspace
```

Expected: PASS.

- [ ] **Step 9: Commit if git is available**

Run:

```bash
git rev-parse --is-inside-work-tree
```

If output is `true`, commit:

```bash
git add Cargo.toml .gitignore crates/future-meta crates/future-meta-daemon
git commit -m "chore: configure future-meta workspace modules"
```

If git fails with `fatal: not a git repository`, skip commit and record changed files.

### Task 2: TqSdk Symbol Parsing

**Files:**
- Modify: `crates/future-meta/src/symbol.rs`
- Modify: `crates/future-meta/src/error.rs`

- [ ] **Step 1: Write failing symbol parser tests**

Replace `crates/future-meta/src/symbol.rs` with tests first:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Futures,
    MainContinuous,
    Index,
    Option,
    Spread,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSymbol {
    pub raw: String,
    pub kind: SymbolKind,
    pub exchange: String,
    pub local: String,
    pub underlying_symbol: Option<String>,
}

pub fn parse_symbol(_symbol: &str) -> Result<ParsedSymbol, crate::error::FutureMetaError> {
    panic!("red phase: symbol parser not implemented")
}

pub fn normalize_futures_symbol(
    _exchange: &str,
    _local_contract: &str,
) -> Result<String, crate::error::FutureMetaError> {
    panic!("red phase: symbol parser not implemented")
}

pub fn derive_underlying_symbol(_symbol: &str) -> Result<String, crate::error::FutureMetaError> {
    panic!("red phase: symbol parser not implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_futures_symbol() {
        let parsed = parse_symbol("SHFE.cu2607").unwrap();
        assert_eq!(parsed.kind, SymbolKind::Futures);
        assert_eq!(parsed.exchange, "SHFE");
        assert_eq!(parsed.local, "cu2607");
        assert_eq!(parsed.underlying_symbol.as_deref(), Some("SHFE.cu"));
    }

    #[test]
    fn normalizes_czce_four_digit_year_to_tqsdk_three_digit() {
        assert_eq!(
            normalize_futures_symbol("CZCE", "SR2609").unwrap(),
            "CZCE.SR609"
        );
    }

    #[test]
    fn keeps_non_czce_four_digit_contracts() {
        assert_eq!(
            normalize_futures_symbol("SHFE", "cu2607").unwrap(),
            "SHFE.cu2607"
        );
        assert_eq!(
            normalize_futures_symbol("CFFEX", "IF2406").unwrap(),
            "CFFEX.IF2406"
        );
    }

    #[test]
    fn parses_main_continuous_alias() {
        let parsed = parse_symbol("KQ.m@SHFE.cu").unwrap();
        assert_eq!(parsed.kind, SymbolKind::MainContinuous);
        assert_eq!(parsed.exchange, "KQ");
        assert_eq!(parsed.local, "m@SHFE.cu");
        assert_eq!(parsed.underlying_symbol.as_deref(), Some("SHFE.cu"));
    }

    #[test]
    fn rejects_index_option_and_spread_as_unsupported() {
        assert!(matches!(
            parse_symbol("KQ.i@SHFE.bu").unwrap().kind,
            SymbolKind::Index
        ));
        assert!(matches!(
            parse_symbol("DCE.m1807-C-2450").unwrap().kind,
            SymbolKind::Option
        ));
        assert!(matches!(
            parse_symbol("DCE.SP a1709&a1801").unwrap().kind,
            SymbolKind::Spread
        ));
    }
}
```

- [ ] **Step 2: Add error variants**

Replace `crates/future-meta/src/error.rs` with:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FutureMetaError {
    #[error("invalid symbol: {0}")]
    InvalidSymbol(String),
    #[error("unsupported symbol kind: {0}")]
    UnsupportedSymbolKind(String),
    #[error("unknown contract: {0}")]
    UnknownContract(String),
    #[error("unknown underlying symbol: {0}")]
    UnknownUnderlyingSymbol(String),
    #[error("no version available at requested time for {0}")]
    NoVersionAt(String),
    #[error("query time is before archive history start")]
    NotAvailableBeforeHistoryStart,
    #[error("unsupported schema version {found}; supported {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("checksum mismatch for {path}: expected {expected}, actual {actual}")]
    ChecksumMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("corrupt archive: {0}")]
    CorruptArchive(String),
    #[error("download failed: {0}")]
    DownloadFailed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type AsOfError = FutureMetaError;
```

- [ ] **Step 3: Run tests to verify failure**

Run:

```bash
cargo test -p future-meta symbol::tests
```

Expected: FAIL with unimplemented parser functions.

- [ ] **Step 4: Implement parser**

Replace the three unimplemented functions in `symbol.rs`:

```rust
const FUTURES_EXCHANGES: &[&str] = &["SHFE", "DCE", "CZCE", "CFFEX", "INE", "GFEX"];

pub fn parse_symbol(symbol: &str) -> Result<ParsedSymbol, crate::error::FutureMetaError> {
    if let Some(rest) = symbol.strip_prefix("KQ.m@") {
        validate_underlying(rest)?;
        return Ok(ParsedSymbol {
            raw: symbol.to_owned(),
            kind: SymbolKind::MainContinuous,
            exchange: "KQ".to_owned(),
            local: format!("m@{rest}"),
            underlying_symbol: Some(rest.to_owned()),
        });
    }

    if let Some(rest) = symbol.strip_prefix("KQ.i@") {
        validate_underlying(rest)?;
        return Ok(ParsedSymbol {
            raw: symbol.to_owned(),
            kind: SymbolKind::Index,
            exchange: "KQ".to_owned(),
            local: format!("i@{rest}"),
            underlying_symbol: Some(rest.to_owned()),
        });
    }

    if symbol.contains(".SP ") || symbol.contains('&') {
        let (exchange, local) = split_exchange_local(symbol)?;
        return Ok(ParsedSymbol {
            raw: symbol.to_owned(),
            kind: SymbolKind::Spread,
            exchange,
            local,
            underlying_symbol: None,
        });
    }

    if symbol.contains("-C-")
        || symbol.contains("-P-")
        || symbol.chars().any(|c| c == 'C' || c == 'P') && symbol.chars().any(|c| c.is_ascii_digit())
    {
        let (exchange, local) = split_exchange_local(symbol)?;
        if !FUTURES_EXCHANGES.contains(&exchange.as_str()) {
            return Err(crate::error::FutureMetaError::InvalidSymbol(symbol.to_owned()));
        }
        if local.contains("-C-") || local.contains("-P-") || looks_like_shfe_option(&local) {
            return Ok(ParsedSymbol {
                raw: symbol.to_owned(),
                kind: SymbolKind::Option,
                exchange,
                local,
                underlying_symbol: None,
            });
        }
    }

    let (exchange, local) = split_exchange_local(symbol)?;
    if !FUTURES_EXCHANGES.contains(&exchange.as_str()) {
        return Err(crate::error::FutureMetaError::InvalidSymbol(symbol.to_owned()));
    }
    let underlying = derive_underlying_from_exchange_local(&exchange, &local)?;
    Ok(ParsedSymbol {
        raw: symbol.to_owned(),
        kind: SymbolKind::Futures,
        exchange,
        local,
        underlying_symbol: Some(underlying),
    })
}

pub fn normalize_futures_symbol(
    exchange: &str,
    local_contract: &str,
) -> Result<String, crate::error::FutureMetaError> {
    let exchange = exchange.trim().to_ascii_uppercase();
    if !FUTURES_EXCHANGES.contains(&exchange.as_str()) {
        return Err(crate::error::FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local_contract}"
        )));
    }

    let local = if exchange == "CZCE" {
        normalize_czce_local(local_contract)?
    } else {
        local_contract.trim().to_owned()
    };

    let symbol = format!("{exchange}.{local}");
    parse_symbol(&symbol)?;
    Ok(symbol)
}

pub fn derive_underlying_symbol(symbol: &str) -> Result<String, crate::error::FutureMetaError> {
    let parsed = parse_symbol(symbol)?;
    parsed
        .underlying_symbol
        .ok_or_else(|| crate::error::FutureMetaError::UnsupportedSymbolKind(symbol.to_owned()))
}

fn split_exchange_local(symbol: &str) -> Result<(String, String), crate::error::FutureMetaError> {
    let Some((exchange, local)) = symbol.split_once('.') else {
        return Err(crate::error::FutureMetaError::InvalidSymbol(symbol.to_owned()));
    };
    if exchange.is_empty() || local.is_empty() {
        return Err(crate::error::FutureMetaError::InvalidSymbol(symbol.to_owned()));
    }
    Ok((exchange.to_ascii_uppercase(), local.to_owned()))
}

fn validate_underlying(underlying: &str) -> Result<(), crate::error::FutureMetaError> {
    let (exchange, local) = split_exchange_local(underlying)?;
    if exchange == "KQ" || !FUTURES_EXCHANGES.contains(&exchange.as_str()) || local.is_empty() {
        return Err(crate::error::FutureMetaError::InvalidSymbol(underlying.to_owned()));
    }
    Ok(())
}

fn derive_underlying_from_exchange_local(
    exchange: &str,
    local: &str,
) -> Result<String, crate::error::FutureMetaError> {
    let product_len = local
        .find(|c: char| c.is_ascii_digit())
        .ok_or_else(|| crate::error::FutureMetaError::InvalidSymbol(format!("{exchange}.{local}")))?;
    if product_len == 0 {
        return Err(crate::error::FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }
    Ok(format!("{exchange}.{}", &local[..product_len]))
}

fn normalize_czce_local(local: &str) -> Result<String, crate::error::FutureMetaError> {
    let local = local.trim();
    let digit_start = local
        .find(|c: char| c.is_ascii_digit())
        .ok_or_else(|| crate::error::FutureMetaError::InvalidSymbol(format!("CZCE.{local}")))?;
    let (product, digits) = local.split_at(digit_start);
    if product.is_empty() {
        return Err(crate::error::FutureMetaError::InvalidSymbol(format!("CZCE.{local}")));
    }
    let normalized_digits = match digits.len() {
        3 => digits.to_owned(),
        4 => format!("{}{}", &digits[1..2], &digits[2..4]),
        _ => {
            return Err(crate::error::FutureMetaError::InvalidSymbol(format!(
                "CZCE.{local}"
            )));
        }
    };
    Ok(format!("{product}{normalized_digits}"))
}

fn looks_like_shfe_option(local: &str) -> bool {
    let mut chars = local.chars().peekable();
    while let Some(c) = chars.next() {
        if (c == 'C' || c == 'P') && chars.peek().is_some_and(char::is_ascii_digit) {
            return true;
        }
    }
    false
}
```

- [ ] **Step 5: Run symbol tests**

Run:

```bash
cargo test -p future-meta symbol::tests
```

Expected: PASS.

- [ ] **Step 6: Commit if git is available**

Run git check and commit:

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta/src/error.rs crates/future-meta/src/symbol.rs
git commit -m "feat: add tq sdk symbol parsing"
```

If not a git repo, skip commit and record changed files.

### Task 3: Fee Model and Fee Parsing

**Files:**
- Modify: `crates/future-meta/src/model.rs`
- Modify: `crates/future-meta/src/fee.rs`

- [ ] **Step 1: Define model types**

Replace `model.rs` with:

```rust
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub data_version: String,
    pub generated_at: String,
    pub history_start: String,
    pub history_end: String,
    pub artifact: String,
    pub sha256: String,
    pub size: u64,
    pub mirrors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeArchiveV1 {
    pub schema_version: u32,
    pub generated_at: String,
    pub history_start: String,
    pub history_end: String,
    pub contracts: Vec<Contract>,
    pub fee_versions: Vec<ContractFee>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contract {
    pub id: u32,
    pub symbol: String,
    pub listing_date: Option<String>,
    pub expiry_date: Option<String>,
    pub lot_size: f64,
    pub tick_size: f64,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractFee {
    pub contract_id: u32,
    pub rule_hash: String,
    pub buy_margin_rate: Option<f64>,
    pub sell_margin_rate: Option<f64>,
    pub open_fee: FeeSpec,
    pub close_yesterday_fee: FeeSpec,
    pub close_today_fee: FeeSpec,
    pub trading_status: TradingStatus,
    pub is_main_contract: bool,
    pub source_updated_at: Option<String>,
    pub valid_from: String,
    pub valid_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradingStatus {
    Trading,
    NotTrading,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeSpec {
    pub kind: FeeKind,
    pub value: Option<f64>,
    pub raw_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeeKind {
    CnyPerLot,
    TurnoverRatePerTenThousand,
    Zero,
    Unknown,
}
```

- [ ] **Step 2: Write fee parser tests**

Replace `fee.rs` with:

```rust
use crate::model::{FeeKind, FeeSpec};

pub fn parse_fee_spec(_text: &str) -> FeeSpec {
    panic!("red phase: implementation not added yet")
}

pub fn parse_optional_f64(_text: &str) -> Option<f64> {
    panic!("red phase: implementation not added yet")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cny_per_lot() {
        let fee = parse_fee_spec("2元");
        assert_eq!(fee.kind, FeeKind::CnyPerLot);
        assert_eq!(fee.value, Some(2.0));
        assert_eq!(fee.raw_text.as_deref(), Some("2元"));
    }

    #[test]
    fn parses_turnover_rate_per_ten_thousand() {
        let fee = parse_fee_spec("0.51/万分之");
        assert_eq!(fee.kind, FeeKind::TurnoverRatePerTenThousand);
        assert_eq!(fee.value, Some(0.51));
        assert_eq!(fee.raw_text.as_deref(), Some("0.51/万分之"));
    }

    #[test]
    fn parses_zero_and_unknown() {
        assert_eq!(parse_fee_spec("0").kind, FeeKind::Zero);
        let unknown = parse_fee_spec("按交易所通知");
        assert_eq!(unknown.kind, FeeKind::Unknown);
        assert_eq!(unknown.value, None);
        assert_eq!(unknown.raw_text.as_deref(), Some("按交易所通知"));
    }

    #[test]
    fn parses_optional_floats() {
        assert_eq!(parse_optional_f64(" 12 "), Some(12.0));
        assert_eq!(parse_optional_f64(""), None);
        assert_eq!(parse_optional_f64("-"), None);
    }
}
```

- [ ] **Step 3: Run parser tests to verify failure**

Run:

```bash
cargo test -p future-meta fee::tests
```

Expected: FAIL with unimplemented functions.

- [ ] **Step 4: Implement fee parsing**

Replace parser functions:

```rust
pub fn parse_fee_spec(text: &str) -> FeeSpec {
    let raw = text.trim();
    if raw.is_empty() || raw == "-" {
        return FeeSpec {
            kind: FeeKind::Unknown,
            value: None,
            raw_text: None,
        };
    }

    let raw_text = Some(raw.to_owned());
    if raw == "0" || raw == "0元" {
        return FeeSpec {
            kind: FeeKind::Zero,
            value: Some(0.0),
            raw_text,
        };
    }

    if let Some(number) = raw.strip_suffix('元') {
        return FeeSpec {
            kind: FeeKind::CnyPerLot,
            value: parse_optional_f64(number),
            raw_text,
        };
    }

    if let Some(number) = raw.strip_suffix("/万分之") {
        return FeeSpec {
            kind: FeeKind::TurnoverRatePerTenThousand,
            value: parse_optional_f64(number),
            raw_text,
        };
    }

    FeeSpec {
        kind: FeeKind::Unknown,
        value: None,
        raw_text,
    }
}

pub fn parse_optional_f64(text: &str) -> Option<f64> {
    let cleaned = text.trim().replace(',', "");
    if cleaned.is_empty() || cleaned == "-" {
        return None;
    }
    cleaned.parse::<f64>().ok()
}
```

- [ ] **Step 5: Run tests**

Run:

```bash
cargo test -p future-meta fee::tests
```

Expected: PASS.

- [ ] **Step 6: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta/src/model.rs crates/future-meta/src/fee.rs
git commit -m "feat: parse fee rule fields"
```

If not a git repo, skip commit and record changed files.

### Task 4: Archive Encoding, Decoding, and Checksum

**Files:**
- Modify: `crates/future-meta/src/archive.rs`
- Create: `crates/future-meta/tests/client_archive.rs`

- [ ] **Step 1: Write archive roundtrip test**

Create `crates/future-meta/tests/client_archive.rs`:

```rust
use future_meta::archive::{decode_archive_bytes, encode_archive_bytes, sha256_hex};
use future_meta::model::{
    Contract, ContractFee, FeeArchiveV1, FeeKind, FeeSpec, SCHEMA_VERSION, TradingStatus,
};

fn sample_archive() -> FeeArchiveV1 {
    FeeArchiveV1 {
        schema_version: SCHEMA_VERSION,
        generated_at: "2026-06-04T12:00:00+08:00".to_owned(),
        history_start: "2026-06-04T12:00:00+08:00".to_owned(),
        history_end: "2026-06-04T12:00:00+08:00".to_owned(),
        contracts: vec![Contract {
            id: 1,
            symbol: "SHFE.cu2607".to_owned(),
            listing_date: Some("20250716".to_owned()),
            expiry_date: Some("20260715".to_owned()),
            lot_size: 5.0,
            tick_size: 10.0,
            active: true,
        }],
        fee_versions: vec![ContractFee {
            contract_id: 1,
            rule_hash: "abc".to_owned(),
            buy_margin_rate: Some(12.0),
            sell_margin_rate: Some(12.0),
            open_fee: FeeSpec {
                kind: FeeKind::CnyPerLot,
                value: Some(0.1),
                raw_text: Some("0.1元".to_owned()),
            },
            close_yesterday_fee: FeeSpec {
                kind: FeeKind::CnyPerLot,
                value: Some(0.1),
                raw_text: Some("0.1元".to_owned()),
            },
            close_today_fee: FeeSpec {
                kind: FeeKind::CnyPerLot,
                value: Some(0.1),
                raw_text: Some("0.1元".to_owned()),
            },
            trading_status: TradingStatus::Trading,
            is_main_contract: true,
            source_updated_at: Some("2026-03-27 22:56:54".to_owned()),
            valid_from: "2026-06-04T12:00:00+08:00".to_owned(),
            valid_to: None,
        }],
    }
}

#[test]
fn archive_roundtrips_through_zstd_bincode() {
    let archive = sample_archive();
    let bytes = encode_archive_bytes(&archive).unwrap();
    assert!(bytes.len() > 8);
    let decoded = decode_archive_bytes(&bytes).unwrap();
    assert_eq!(decoded, archive);
}

#[test]
fn sha256_is_stable_hex() {
    assert_eq!(
        sha256_hex(b"future-meta"),
        "10a5a37a5ea699978a141849a55f2ad572037fcfe42d947db6eb2986c893c90c"
    );
}
```

- [ ] **Step 2: Run archive tests to verify failure**

Run:

```bash
cargo test -p future-meta --test client_archive
```

Expected: FAIL because archive helpers are missing.

- [ ] **Step 3: Implement archive helpers**

Replace `archive.rs`:

```rust
use crate::error::FutureMetaError;
use crate::model::{FeeArchiveV1, SCHEMA_VERSION};
use sha2::{Digest, Sha256};

pub fn encode_archive_bytes(archive: &FeeArchiveV1) -> Result<Vec<u8>, FutureMetaError> {
    let encoded = bincode::serde::encode_to_vec(archive, bincode::config::standard())
        .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))?;
    zstd::stream::encode_all(encoded.as_slice(), 19)
        .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))
}

pub fn decode_archive_bytes(bytes: &[u8]) -> Result<FeeArchiveV1, FutureMetaError> {
    let decoded = zstd::stream::decode_all(bytes)
        .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))?;
    let (archive, _): (FeeArchiveV1, usize) =
        bincode::serde::decode_from_slice(&decoded, bincode::config::standard())
            .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))?;
    if archive.schema_version != SCHEMA_VERSION {
        return Err(FutureMetaError::UnsupportedSchemaVersion {
            found: archive.schema_version,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(archive)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}
```

- [ ] **Step 4: Run archive tests**

Run:

```bash
cargo test -p future-meta --test client_archive
```

Expected: PASS.

- [ ] **Step 5: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta/src/archive.rs crates/future-meta/tests/client_archive.rs
git commit -m "feat: encode compressed fee archives"
```

If not a git repo, skip commit and record changed files.

### Task 5: Client As-Of Index and Query API

**Files:**
- Modify: `crates/future-meta/src/query.rs`
- Modify: `crates/future-meta/tests/client_archive.rs`

- [ ] **Step 1: Add failing query tests**

Append to `client_archive.rs`:

```rust
use future_meta::query::FutureMeta;

#[test]
fn queries_contract_fee_asof() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let fee = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T12:00:00+08:00")
        .unwrap();
    assert!(fee.is_main_contract);
    assert_eq!(fee.rule_hash, "abc");
}

#[test]
fn queries_underlying_and_main_continuous() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let fees = meta
        .underlying_fees_asof("SHFE.cu", "2026-06-04T12:00:00+08:00")
        .unwrap();
    assert_eq!(fees.len(), 1);

    let main = meta
        .main_contract_fee_asof("KQ.m@SHFE.cu", "2026-06-04T12:00:00+08:00")
        .unwrap();
    assert_eq!(main.rule_hash, "abc");
}

#[test]
fn rejects_index_for_main_contract_query() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let err = meta
        .main_contract_fee_asof("KQ.i@SHFE.cu", "2026-06-04T12:00:00+08:00")
        .unwrap_err();
    assert!(err.to_string().contains("unsupported symbol kind"));
}
```

- [ ] **Step 2: Run query tests to verify failure**

Run:

```bash
cargo test -p future-meta --test client_archive queries_
```

Expected: FAIL because `FutureMeta` is not implemented.

- [ ] **Step 3: Implement query API**

Replace `query.rs`:

```rust
use crate::error::FutureMetaError;
use crate::model::{Contract, ContractFee, FeeArchiveV1};
use crate::symbol::{derive_underlying_symbol, parse_symbol, SymbolKind};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct FutureMeta {
    archive: FeeArchiveV1,
    contract_by_symbol: HashMap<String, u32>,
    fees_by_contract: HashMap<u32, Vec<usize>>,
    contracts_by_underlying: HashMap<String, Vec<u32>>,
}

impl FutureMeta {
    pub fn from_archive(archive: FeeArchiveV1) -> Result<Self, FutureMetaError> {
        let mut contract_by_symbol = HashMap::new();
        let mut contracts_by_underlying: HashMap<String, Vec<u32>> = HashMap::new();

        for contract in &archive.contracts {
            contract_by_symbol.insert(contract.symbol.clone(), contract.id);
            let underlying = derive_underlying_symbol(&contract.symbol)?;
            contracts_by_underlying
                .entry(underlying)
                .or_default()
                .push(contract.id);
        }

        let mut fees_by_contract: HashMap<u32, Vec<usize>> = HashMap::new();
        for (idx, fee) in archive.fee_versions.iter().enumerate() {
            fees_by_contract.entry(fee.contract_id).or_default().push(idx);
        }
        for indexes in fees_by_contract.values_mut() {
            indexes.sort_by(|left, right| {
                archive.fee_versions[*left]
                    .valid_from
                    .cmp(&archive.fee_versions[*right].valid_from)
            });
        }

        Ok(Self {
            archive,
            contract_by_symbol,
            fees_by_contract,
            contracts_by_underlying,
        })
    }

    pub fn contract_fee_asof(
        &self,
        symbol: &str,
        at: &str,
    ) -> Result<&ContractFee, FutureMetaError> {
        if at < self.archive.history_start.as_str() {
            return Err(FutureMetaError::NotAvailableBeforeHistoryStart);
        }
        let contract_id = self
            .contract_by_symbol
            .get(symbol)
            .copied()
            .ok_or_else(|| FutureMetaError::UnknownContract(symbol.to_owned()))?;
        self.fee_for_contract_id_asof(contract_id, at)
            .ok_or_else(|| FutureMetaError::NoVersionAt(symbol.to_owned()))
    }

    pub fn underlying_fees_asof(
        &self,
        underlying_symbol: &str,
        at: &str,
    ) -> Result<Vec<&ContractFee>, FutureMetaError> {
        if at < self.archive.history_start.as_str() {
            return Err(FutureMetaError::NotAvailableBeforeHistoryStart);
        }
        let contract_ids = self
            .contracts_by_underlying
            .get(underlying_symbol)
            .ok_or_else(|| FutureMetaError::UnknownUnderlyingSymbol(underlying_symbol.to_owned()))?;
        Ok(contract_ids
            .iter()
            .filter_map(|id| self.fee_for_contract_id_asof(*id, at))
            .collect())
    }

    pub fn main_contract_fee_asof(
        &self,
        symbol: &str,
        at: &str,
    ) -> Result<&ContractFee, FutureMetaError> {
        let parsed = parse_symbol(symbol)?;
        if parsed.kind != SymbolKind::MainContinuous {
            return Err(FutureMetaError::UnsupportedSymbolKind(symbol.to_owned()));
        }
        let underlying = parsed
            .underlying_symbol
            .ok_or_else(|| FutureMetaError::UnsupportedSymbolKind(symbol.to_owned()))?;
        let fees = self.underlying_fees_asof(&underlying, at)?;
        fees.into_iter()
            .find(|fee| fee.is_main_contract)
            .ok_or_else(|| FutureMetaError::NoVersionAt(symbol.to_owned()))
    }

    pub fn contracts(&self) -> &[Contract] {
        &self.archive.contracts
    }

    fn fee_for_contract_id_asof(&self, contract_id: u32, at: &str) -> Option<&ContractFee> {
        let indexes = self.fees_by_contract.get(&contract_id)?;
        let mut found = None;
        for idx in indexes {
            let fee = &self.archive.fee_versions[*idx];
            if fee.valid_from.as_str() <= at && fee.valid_to.as_deref().is_none_or(|end| at < end) {
                found = Some(fee);
            }
        }
        found
    }
}
```

- [ ] **Step 4: Run query tests**

Run:

```bash
cargo test -p future-meta --test client_archive
```

Expected: PASS.

- [ ] **Step 5: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta/src/query.rs crates/future-meta/tests/client_archive.rs
git commit -m "feat: add local asof query index"
```

If not a git repo, skip commit and record changed files.

### Task 6: Client Download and Cache Feature

**Files:**
- Modify: `crates/future-meta/src/download.rs`
- Modify: `crates/future-meta/src/query.rs`
- Modify: `crates/future-meta/tests/client_archive.rs`

- [ ] **Step 1: Write checksum/cache helper tests**

Append to `client_archive.rs`:

```rust
#[cfg(feature = "download")]
#[tokio::test]
async fn load_file_decodes_archive() {
    let archive = sample_archive();
    let bytes = encode_archive_bytes(&archive).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("latest.fmeta.zst");
    tokio::fs::write(&path, bytes).await.unwrap();

    let meta = FutureMeta::load_file(&path).await.unwrap();
    assert_eq!(meta.contracts().len(), 1);
}
```

- [ ] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p future-meta --features download --test client_archive load_file_decodes_archive
```

Expected: FAIL because `FutureMeta::load_file` is missing.

- [ ] **Step 3: Implement file loading**

Add to `impl FutureMeta` in `query.rs`:

```rust
#[cfg(feature = "download")]
pub async fn load_file(path: impl AsRef<std::path::Path>) -> Result<Self, FutureMetaError> {
    let bytes = tokio::fs::read(path).await?;
    let archive = crate::archive::decode_archive_bytes(&bytes)?;
    Self::from_archive(archive)
}
```

- [ ] **Step 4: Implement download config**

Replace `download.rs`:

```rust
use crate::archive::{decode_archive_bytes, sha256_hex};
use crate::error::FutureMetaError;
use crate::model::Manifest;
use crate::query::FutureMeta;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    pub manifest_url: String,
    pub cache_dir: PathBuf,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        let cache_dir = std::env::var_os("FUTURE_META_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("future-meta-cache"));
        Self {
            manifest_url: "https://future-meta.pages.dev/manifest.json".to_owned(),
            cache_dir,
        }
    }
}

pub async fn load_or_fetch(config: DownloadConfig) -> Result<FutureMeta, FutureMetaError> {
    tokio::fs::create_dir_all(&config.cache_dir).await?;
    let manifest_text = reqwest::get(&config.manifest_url)
        .await
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?
        .text()
        .await
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?;
    let manifest: Manifest = serde_json::from_str(&manifest_text)?;
    let artifact_path = config.cache_dir.join(&manifest.artifact);

    let bytes = if artifact_path.exists() {
        tokio::fs::read(&artifact_path).await?
    } else {
        let artifact_url = manifest
            .mirrors
            .first()
            .ok_or_else(|| FutureMetaError::DownloadFailed("manifest has no mirrors".to_owned()))?;
        let downloaded = reqwest::get(artifact_url)
            .await
            .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?
            .bytes()
            .await
            .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?
            .to_vec();
        tokio::fs::write(&artifact_path, &downloaded).await?;
        downloaded
    };

    let actual = sha256_hex(&bytes);
    if actual != manifest.sha256 {
        return Err(FutureMetaError::ChecksumMismatch {
            path: artifact_path.display().to_string(),
            expected: manifest.sha256,
            actual,
        });
    }

    let archive = decode_archive_bytes(&bytes)?;
    FutureMeta::from_archive(archive)
}
```

Add to `lib.rs`:

```rust
#[cfg(feature = "download")]
pub use crate::download::{load_or_fetch, DownloadConfig};
```

- [ ] **Step 5: Run download feature tests**

Run:

```bash
cargo test -p future-meta --features download
```

Expected: PASS.

- [ ] **Step 6: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta/src/download.rs crates/future-meta/src/query.rs crates/future-meta/src/lib.rs crates/future-meta/tests/client_archive.rs
git commit -m "feat: add archive download cache"
```

If not a git repo, skip commit and record changed files.

### Task 7: Daemon CSV Parsing and Allowed-Field Hashing

**Files:**
- Modify: `crates/future-meta-daemon/src/parse.rs`
- Modify: `crates/future-meta-daemon/src/hash.rs`

- [ ] **Step 1: Write parser and hash tests**

Replace `parse.rs`:

```rust
use future_meta::model::{ContractFee, TradingStatus};

#[derive(Debug, Clone, PartialEq)]
pub struct AllowedRow {
    pub symbol: String,
    pub listing_date: Option<String>,
    pub expiry_date: Option<String>,
    pub trading_status: TradingStatus,
    pub buy_margin_rate: Option<f64>,
    pub sell_margin_rate: Option<f64>,
    pub open_fee: future_meta::model::FeeSpec,
    pub close_yesterday_fee: future_meta::model::FeeSpec,
    pub close_today_fee: future_meta::model::FeeSpec,
    pub lot_size: f64,
    pub tick_size: f64,
    pub source_updated_at: Option<String>,
    pub is_main_contract: bool,
}

pub fn parse_csv(_csv_text: &str) -> anyhow::Result<Vec<AllowedRow>> {
    panic!("red phase: implementation not added yet")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::row_rule_hash;

    const BASE: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,12,64122,0.1元,0.1元,0.1元,5,10,50,0.2,49.8,2026-03-27 22:56:54,主力合约\n";

    #[test]
    fn parses_allowed_fields_and_drops_derived_fields() {
        let rows = parse_csv(BASE).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.symbol, "SHFE.cu2607");
        assert_eq!(row.listing_date.as_deref(), Some("20250716"));
        assert_eq!(row.expiry_date.as_deref(), Some("20260715"));
        assert!(row.is_main_contract);
        assert_eq!(row.lot_size, 5.0);
        assert_eq!(row.tick_size, 10.0);
    }

    #[test]
    fn derived_field_changes_do_not_change_rule_hash() {
        let changed = BASE
            .replace("106870", "999999")
            .replace("117550/96180", "1/2")
            .replace("64122", "1")
            .replace("49.8", "-999");
        let base_hash = row_rule_hash(&parse_csv(BASE).unwrap()[0]);
        let changed_hash = row_rule_hash(&parse_csv(&changed).unwrap()[0]);
        assert_eq!(base_hash, changed_hash);
    }

    #[test]
    fn rule_field_changes_change_rule_hash() {
        let changed = BASE.replace("0.1元,0.1元,0.1元", "0.2元,0.1元,0.1元");
        let base_hash = row_rule_hash(&parse_csv(BASE).unwrap()[0]);
        let changed_hash = row_rule_hash(&parse_csv(&changed).unwrap()[0]);
        assert_ne!(base_hash, changed_hash);
    }
}
```

- [ ] **Step 2: Add hash skeleton**

Replace `hash.rs`:

```rust
use crate::parse::AllowedRow;

pub fn row_rule_hash(_row: &AllowedRow) -> String {
    panic!("red phase: implementation not added yet")
}

pub fn rule_set_hash(rows: &[AllowedRow]) -> String {
    let mut hashes = rows.iter().map(row_rule_hash).collect::<Vec<_>>();
    hashes.sort();
    digest_text(&hashes.join("\n"))
}

fn digest_text(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))
}
```

- [ ] **Step 3: Run parser tests to verify failure**

Run:

```bash
cargo test -p future-meta-daemon parse::tests
```

Expected: FAIL with unimplemented parser/hash.

- [ ] **Step 4: Implement CSV parser and rule hash**

Implement `parse_csv`:

```rust
pub fn parse_csv(csv_text: &str) -> anyhow::Result<Vec<AllowedRow>> {
    let mut reader = csv::Reader::from_reader(csv_text.as_bytes());
    let headers = reader.headers()?.clone();
    let idx = |name: &str| -> anyhow::Result<usize> {
        headers
            .iter()
            .position(|header| header.trim() == name)
            .ok_or_else(|| anyhow::anyhow!("missing CSV header {name}"))
    };

    let contract_idx = idx("合约代码")?;
    let exchange_idx = idx("交易所编码")?;
    let listing_idx = idx("上市日期")?;
    let expiry_idx = idx("到期日期")?;
    let status_idx = idx("是否正在交易")?;
    let buy_margin_idx = idx("买开保证金%")?;
    let sell_margin_idx = idx("卖开保证金%")?;
    let open_fee_idx = idx("开仓手续费")?;
    let close_yesterday_idx = idx("平昨手续费")?;
    let close_today_idx = idx("平今手续费")?;
    let lot_size_idx = idx("每手数量")?;
    let tick_size_idx = idx("每跳价差")?;
    let source_updated_idx = idx("手续费更新时间")?;
    let remark_idx = idx("备注")?;

    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record?;
        let exchange = record.get(exchange_idx).unwrap_or("").trim();
        let local = record.get(contract_idx).unwrap_or("").trim();
        if local.is_empty() || exchange.is_empty() {
            continue;
        }
        let symbol = future_meta::symbol::normalize_futures_symbol(exchange, local)?;
        rows.push(AllowedRow {
            symbol,
            listing_date: non_empty(record.get(listing_idx)),
            expiry_date: non_empty(record.get(expiry_idx)),
            trading_status: parse_trading_status(record.get(status_idx).unwrap_or("")),
            buy_margin_rate: future_meta::fee::parse_optional_f64(record.get(buy_margin_idx).unwrap_or("")),
            sell_margin_rate: future_meta::fee::parse_optional_f64(record.get(sell_margin_idx).unwrap_or("")),
            open_fee: future_meta::fee::parse_fee_spec(record.get(open_fee_idx).unwrap_or("")),
            close_yesterday_fee: future_meta::fee::parse_fee_spec(record.get(close_yesterday_idx).unwrap_or("")),
            close_today_fee: future_meta::fee::parse_fee_spec(record.get(close_today_idx).unwrap_or("")),
            lot_size: future_meta::fee::parse_optional_f64(record.get(lot_size_idx).unwrap_or("")).unwrap_or(0.0),
            tick_size: future_meta::fee::parse_optional_f64(record.get(tick_size_idx).unwrap_or("")).unwrap_or(0.0),
            source_updated_at: non_empty(record.get(source_updated_idx)),
            is_main_contract: record.get(remark_idx).unwrap_or("").contains("主力"),
        });
    }
    Ok(rows)
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

fn parse_trading_status(text: &str) -> TradingStatus {
    match text.trim() {
        "交易中" => TradingStatus::Trading,
        "否" => TradingStatus::NotTrading,
        _ => TradingStatus::Unknown,
    }
}
```

Implement `row_rule_hash`:

```rust
pub fn row_rule_hash(row: &AllowedRow) -> String {
    let canonical = serde_json::json!({
        "symbol": row.symbol,
        "listing_date": row.listing_date,
        "expiry_date": row.expiry_date,
        "trading_status": row.trading_status,
        "buy_margin_rate": row.buy_margin_rate,
        "sell_margin_rate": row.sell_margin_rate,
        "open_fee": row.open_fee,
        "close_yesterday_fee": row.close_yesterday_fee,
        "close_today_fee": row.close_today_fee,
        "lot_size": row.lot_size,
        "tick_size": row.tick_size,
        "is_main_contract": row.is_main_contract,
    });
    digest_text(&canonical.to_string())
}
```

- [ ] **Step 5: Run parser/hash tests**

Run:

```bash
cargo test -p future-meta-daemon parse::tests
```

Expected: PASS.

- [ ] **Step 6: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta-daemon/src/parse.rs crates/future-meta-daemon/src/hash.rs
git commit -m "feat: parse allowed fee fields"
```

If not a git repo, skip commit and record changed files.

### Task 8: SQLite Schema and Version Maintenance

**Files:**
- Modify: `crates/future-meta-daemon/src/db.rs`
- Create: `crates/future-meta-daemon/tests/daemon_pipeline.rs`

- [ ] **Step 1: Write database versioning integration test**

Create `crates/future-meta-daemon/tests/daemon_pipeline.rs`:

```rust
use future_meta_daemon::db::{connect, ensure_schema, upsert_allowed_rows};
use future_meta_daemon::parse::parse_csv;

const CSV_V1: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,12,64122,0.1元,0.1元,0.1元,5,10,50,0.2,49.8,2026-03-27 22:56:54,主力合约\n";
const CSV_V2: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,12,64122,0.2元,0.1元,0.1元,5,10,50,0.2,49.8,2026-03-28 22:56:54,主力合约\n";

#[test]
fn upsert_creates_new_fee_version_only_for_rule_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let rows_v1 = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&conn, &rows_v1, "2026-06-04T12:00:00+08:00").unwrap();
    upsert_allowed_rows(&conn, &rows_v1, "2026-06-04T13:00:00+08:00").unwrap();

    let count: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let rows_v2 = parse_csv(CSV_V2).unwrap();
    upsert_allowed_rows(&conn, &rows_v2, "2026-06-04T14:00:00+08:00").unwrap();

    let count: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}
```

- [ ] **Step 2: Run integration test to verify failure**

Run:

```bash
cargo test -p future-meta-daemon --test daemon_pipeline upsert_creates_new_fee_version_only_for_rule_changes
```

Expected: FAIL because DB functions are missing.

- [ ] **Step 3: Implement DB schema**

Replace `db.rs` with functions:

```rust
use crate::hash::row_rule_hash;
use crate::parse::AllowedRow;
use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;

pub fn connect(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(Connection::open(path)?)
}

pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        create table if not exists contracts(
          id integer primary key,
          symbol text not null unique,
          listing_date text,
          expiry_date text,
          lot_size real not null,
          tick_size real not null,
          first_seen_at text not null,
          last_seen_at text not null,
          active integer not null
        );
        create table if not exists fee_versions(
          id integer primary key,
          contract_id integer not null,
          rule_hash text not null,
          buy_margin_rate real,
          sell_margin_rate real,
          open_fee_json text not null,
          close_yesterday_fee_json text not null,
          close_today_fee_json text not null,
          trading_status text not null,
          is_main_contract integer not null,
          source_updated_at text,
          valid_from text not null,
          valid_to text,
          first_seen_at text not null,
          last_seen_at text not null
        );
        create index if not exists idx_fee_versions_contract on fee_versions(contract_id, valid_from);
        ",
    )?;
    Ok(())
}
```

- [ ] **Step 4: Implement upsert logic**

Append:

```rust
pub fn upsert_allowed_rows(conn: &Connection, rows: &[AllowedRow], observed_at: &str) -> Result<()> {
    ensure_schema(conn)?;
    for row in rows {
        let contract_id = upsert_contract(conn, row, observed_at)?;
        let rule_hash = row_rule_hash(row);
        let current: Option<(i64, String)> = conn
            .query_row(
                "select id, rule_hash from fee_versions where contract_id = ?1 and valid_to is null order by id desc limit 1",
                params![contract_id],
                |record| Ok((record.get(0)?, record.get(1)?)),
            )
            .optional()?;

        if let Some((version_id, current_hash)) = current {
            if current_hash == rule_hash {
                conn.execute(
                    "update fee_versions set last_seen_at = ?1 where id = ?2",
                    params![observed_at, version_id],
                )?;
                continue;
            }
            conn.execute(
                "update fee_versions set valid_to = ?1, last_seen_at = ?1 where id = ?2",
                params![observed_at, version_id],
            )?;
        }

        conn.execute(
            "insert into fee_versions(
              contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
              open_fee_json, close_yesterday_fee_json, close_today_fee_json,
              trading_status, is_main_contract, source_updated_at,
              valid_from, valid_to, first_seen_at, last_seen_at
            ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, null, ?11, ?11)",
            params![
                contract_id,
                rule_hash,
                row.buy_margin_rate,
                row.sell_margin_rate,
                serde_json::to_string(&row.open_fee)?,
                serde_json::to_string(&row.close_yesterday_fee)?,
                serde_json::to_string(&row.close_today_fee)?,
                format!("{:?}", row.trading_status),
                i64::from(row.is_main_contract),
                row.source_updated_at,
                observed_at,
            ],
        )?;
    }
    Ok(())
}

fn upsert_contract(conn: &Connection, row: &AllowedRow, observed_at: &str) -> Result<i64> {
    conn.execute(
        "insert into contracts(symbol, listing_date, expiry_date, lot_size, tick_size, first_seen_at, last_seen_at, active)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1)
         on conflict(symbol) do update set
           listing_date = excluded.listing_date,
           expiry_date = excluded.expiry_date,
           lot_size = excluded.lot_size,
           tick_size = excluded.tick_size,
           last_seen_at = excluded.last_seen_at,
           active = 1",
        params![
            row.symbol,
            row.listing_date,
            row.expiry_date,
            row.lot_size,
            row.tick_size,
            observed_at,
        ],
    )?;
    Ok(conn.query_row(
        "select id from contracts where symbol = ?1",
        params![row.symbol],
        |record| record.get(0),
    )?)
}

pub fn inspect(db: &Path) -> Result<()> {
    let conn = connect(db)?;
    ensure_schema(&conn)?;
    let contracts: i64 = conn.query_row("select count(*) from contracts", [], |row| row.get(0))?;
    let versions: i64 = conn.query_row("select count(*) from fee_versions", [], |row| row.get(0))?;
    println!("contracts={contracts} fee_versions={versions}");
    Ok(())
}

use rusqlite::OptionalExtension;
```

- [ ] **Step 5: Run DB test**

Run:

```bash
cargo test -p future-meta-daemon --test daemon_pipeline upsert_creates_new_fee_version_only_for_rule_changes
```

Expected: PASS.

- [ ] **Step 6: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta-daemon/src/db.rs crates/future-meta-daemon/tests/daemon_pipeline.rs
git commit -m "feat: maintain sqlite fee versions"
```

If not a git repo, skip commit and record changed files.

### Task 9: HTML Discovery and Refresh Orchestration

**Files:**
- Modify: `crates/future-meta-daemon/src/source.rs`
- Modify: `crates/future-meta-daemon/src/refresh.rs`
- Modify: `crates/future-meta-daemon/tests/daemon_pipeline.rs`

- [ ] **Step 1: Add discovery test**

Append to `daemon_pipeline.rs`:

```rust
use future_meta_daemon::source::discover_sources_from_html;

#[test]
fn discovers_single_variety_sources_from_total_page_html() {
    let html = r#"
      <a href="/qihuoshouxufeisingle?heyue=cu">沪铜</a>
      <a href="https://www.9qihuo.com/qihuoshouxufeisingle?heyue=IF">沪深300</a>
      <a href="/qihuoshouxufeisingle?heyue=cu">duplicate</a>
    "#;
    let sources = discover_sources_from_html(html).unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].heyue, "IF");
    assert_eq!(sources[0].csv_url, "https://www.9qihuo.com/shouxufeixz?heyue=IF");
    assert_eq!(sources[1].heyue, "cu");
}
```

- [ ] **Step 2: Implement discovery**

Replace `source.rs`:

```rust
use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    pub heyue: String,
    pub detail_url: String,
    pub csv_url: String,
}

pub fn discover_sources_from_html(html: &str) -> Result<Vec<SourceEntry>> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a").map_err(|err| anyhow::anyhow!("{err}"))?;
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
                detail_url: format!("https://www.9qihuo.com/qihuoshouxufeisingle?heyue={heyue}"),
                csv_url: format!("https://www.9qihuo.com/shouxufeixz?heyue={heyue}"),
                heyue,
            },
        );
    }
    Ok(entries.into_values().collect())
}

pub fn discover_to_file(out: &Path) -> Result<()> {
    let html = reqwest::blocking::get("https://www.9qihuo.com/qihuoshouxufei")?.text()?;
    let sources = discover_sources_from_html(&html)?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, serde_json::to_vec_pretty(&sources)?)?;
    Ok(())
}

fn extract_heyue(href: &str) -> Option<String> {
    let (_, query) = href.split_once("qihuoshouxufeisingle?")?;
    for part in query.split('&') {
        if let Some(value) = part.strip_prefix("heyue=") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}
```

- [ ] **Step 3: Run discovery test**

Run:

```bash
cargo test -p future-meta-daemon --test daemon_pipeline discovers_single_variety_sources_from_total_page_html
```

Expected: PASS.

- [ ] **Step 4: Implement basic refresh orchestration**

Replace `refresh.rs`:

```rust
use crate::db::{connect, ensure_schema, upsert_allowed_rows};
use crate::parse::parse_csv;
use crate::source::discover_sources_from_html;
use anyhow::Result;
use std::path::Path;

pub fn refresh(db: &Path, force_full: bool) -> Result<()> {
    let conn = connect(db)?;
    ensure_schema(&conn)?;
    let html = reqwest::blocking::get("https://www.9qihuo.com/qihuoshouxufei")?.text()?;
    let sources = discover_sources_from_html(&html)?;
    let observed_at = now_string();
    for source in sources {
        let csv = reqwest::blocking::get(&source.csv_url)?.text()?;
        let rows = parse_csv(&csv)?;
        if rows.is_empty() && !force_full {
            continue;
        }
        upsert_allowed_rows(&conn, &rows, &observed_at)?;
    }
    Ok(())
}

fn now_string() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
```

This is intentionally simple. Source-state probe optimization comes after export proves the pipeline.

- [ ] **Step 5: Run daemon tests**

Run:

```bash
cargo test -p future-meta-daemon
```

Expected: PASS.

- [ ] **Step 6: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta-daemon/src/source.rs crates/future-meta-daemon/src/refresh.rs crates/future-meta-daemon/tests/daemon_pipeline.rs
git commit -m "feat: discover and refresh fee sources"
```

If not a git repo, skip commit and record changed files.

### Task 10: Export Manifest and Archive

**Files:**
- Modify: `crates/future-meta-daemon/src/export.rs`
- Modify: `crates/future-meta-daemon/tests/daemon_pipeline.rs`

- [ ] **Step 1: Add export integration test**

Append to `daemon_pipeline.rs`:

```rust
use future_meta::query::FutureMeta;
use future_meta_daemon::export::export_archive;

#[test]
fn exports_archive_loadable_by_client() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let out = dir.path().join("public");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    upsert_allowed_rows(
        &conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();

    export_archive(&db_path, &out).unwrap();
    let manifest_text = std::fs::read_to_string(out.join("manifest.json")).unwrap();
    assert!(manifest_text.contains("latest.fmeta.zst"));

    let bytes = std::fs::read(out.join("latest.fmeta.zst")).unwrap();
    let archive = future_meta::archive::decode_archive_bytes(&bytes).unwrap();
    let meta = FutureMeta::from_archive(archive).unwrap();
    assert!(meta.contract_fee_asof("SHFE.cu2607", "2026-06-04T12:00:00+08:00").is_ok());
}
```

- [ ] **Step 2: Run export test to verify failure**

Run:

```bash
cargo test -p future-meta-daemon --test daemon_pipeline exports_archive_loadable_by_client
```

Expected: FAIL because exporter is still stubbed.

- [ ] **Step 3: Implement exporter**

Replace `export.rs`:

```rust
use anyhow::Result;
use future_meta::archive::{encode_archive_bytes, sha256_hex};
use future_meta::model::{
    Contract, ContractFee, FeeArchiveV1, FeeSpec, Manifest, SCHEMA_VERSION, TradingStatus,
};
use rusqlite::Connection;
use std::path::Path;

pub fn export_archive(db: &Path, out: &Path) -> Result<()> {
    std::fs::create_dir_all(out.join("artifacts"))?;
    let conn = Connection::open(db)?;
    let archive = load_archive(&conn)?;
    let bytes = encode_archive_bytes(&archive)?;
    let sha = sha256_hex(&bytes);
    let data_version = archive.generated_at.replace([':', '+'], "").replace('-', "");
    let artifact_name = format!("artifacts/future-meta-fees-v1-{data_version}.fmeta.zst");
    std::fs::write(out.join("latest.fmeta.zst"), &bytes)?;
    std::fs::write(out.join(&artifact_name), &bytes)?;
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        data_version: archive.generated_at.clone(),
        generated_at: archive.generated_at.clone(),
        history_start: archive.history_start.clone(),
        history_end: archive.history_end.clone(),
        artifact: "latest.fmeta.zst".to_owned(),
        sha256: sha,
        size: bytes.len() as u64,
        mirrors: vec!["https://future-meta.pages.dev/latest.fmeta.zst".to_owned()],
    };
    std::fs::write(out.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

fn load_archive(conn: &Connection) -> Result<FeeArchiveV1> {
    let mut contracts_stmt = conn.prepare(
        "select id, symbol, listing_date, expiry_date, lot_size, tick_size, active from contracts order by id",
    )?;
    let contracts = contracts_stmt
        .query_map([], |row| {
            Ok(Contract {
                id: row.get::<_, i64>(0)? as u32,
                symbol: row.get(1)?,
                listing_date: row.get(2)?,
                expiry_date: row.get(3)?,
                lot_size: row.get(4)?,
                tick_size: row.get(5)?,
                active: row.get::<_, i64>(6)? != 0,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut fee_stmt = conn.prepare(
        "select contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
                open_fee_json, close_yesterday_fee_json, close_today_fee_json,
                trading_status, is_main_contract, source_updated_at,
                valid_from, valid_to
         from fee_versions order by contract_id, valid_from",
    )?;
    let fee_versions = fee_stmt
        .query_map([], |row| {
            let trading_status_text: String = row.get(7)?;
            Ok(ContractFee {
                contract_id: row.get::<_, i64>(0)? as u32,
                rule_hash: row.get(1)?,
                buy_margin_rate: row.get(2)?,
                sell_margin_rate: row.get(3)?,
                open_fee: serde_json::from_str::<FeeSpec>(&row.get::<_, String>(4)?)
                    .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?,
                close_yesterday_fee: serde_json::from_str::<FeeSpec>(&row.get::<_, String>(5)?)
                    .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?,
                close_today_fee: serde_json::from_str::<FeeSpec>(&row.get::<_, String>(6)?)
                    .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?,
                trading_status: parse_status(&trading_status_text),
                is_main_contract: row.get::<_, i64>(8)? != 0,
                source_updated_at: row.get(9)?,
                valid_from: row.get(10)?,
                valid_to: row.get(11)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let generated_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)?;
    let history_start = fee_versions
        .iter()
        .map(|version| version.valid_from.clone())
        .min()
        .unwrap_or_else(|| generated_at.clone());
    let history_end = generated_at.clone();
    Ok(FeeArchiveV1 {
        schema_version: SCHEMA_VERSION,
        generated_at,
        history_start,
        history_end,
        contracts,
        fee_versions,
    })
}

fn parse_status(text: &str) -> TradingStatus {
    match text {
        "Trading" => TradingStatus::Trading,
        "NotTrading" => TradingStatus::NotTrading,
        _ => TradingStatus::Unknown,
    }
}
```

- [ ] **Step 4: Run export test**

Run:

```bash
cargo test -p future-meta-daemon --test daemon_pipeline exports_archive_loadable_by_client
```

Expected: PASS.

- [ ] **Step 5: Run workspace tests**

Run:

```bash
cargo test --workspace --features future-meta/download
```

If feature syntax fails, run:

```bash
cargo test --workspace
cargo test -p future-meta --features download
```

Expected: PASS.

- [ ] **Step 6: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta-daemon/src/export.rs crates/future-meta-daemon/tests/daemon_pipeline.rs
git commit -m "feat: export cloudflare pages archive"
```

If not a git repo, skip commit and record changed files.

### Task 11: Source-State Probe Optimization

**Files:**
- Modify: `crates/future-meta-daemon/src/db.rs`
- Modify: `crates/future-meta-daemon/src/refresh.rs`
- Modify: `crates/future-meta-daemon/src/hash.rs`

- [ ] **Step 1: Add source state schema**

Update `ensure_schema` SQL with:

```sql
create table if not exists source_state(
  source_url text primary key,
  last_probe_hash text,
  last_rule_set_hash text,
  last_success_at text,
  last_error_at text,
  last_error_message text
);
```

Add DB functions:

```rust
pub fn source_probe_hash(conn: &Connection, source_url: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "select last_probe_hash from source_state where source_url = ?1",
            params![source_url],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn update_source_success(
    conn: &Connection,
    source_url: &str,
    probe_hash: &str,
    rule_set_hash: &str,
    observed_at: &str,
) -> Result<()> {
    conn.execute(
        "insert into source_state(source_url, last_probe_hash, last_rule_set_hash, last_success_at)
         values (?1, ?2, ?3, ?4)
         on conflict(source_url) do update set
           last_probe_hash = excluded.last_probe_hash,
           last_rule_set_hash = excluded.last_rule_set_hash,
           last_success_at = excluded.last_success_at,
           last_error_at = null,
           last_error_message = null",
        params![source_url, probe_hash, rule_set_hash, observed_at],
    )?;
    Ok(())
}
```

- [ ] **Step 2: Add hash helper**

Add to `hash.rs`:

```rust
pub fn source_probe_hash(csv_url: &str, html_fragment_or_url: &str) -> String {
    digest_text(&format!("{csv_url}\n{html_fragment_or_url}"))
}
```

This first version uses source URL plus discovered detail URL as a stable probe. It still performs a daily full CSV check via `--force-full`. Later implementation may parse visible allowed fields from total page for a sharper probe.

- [ ] **Step 3: Update refresh skip logic**

In `refresh.rs`, before downloading CSV:

```rust
let probe_hash = crate::hash::source_probe_hash(&source.csv_url, &source.detail_url);
if !force_full && db::source_probe_hash(&conn, &source.csv_url)?.as_deref() == Some(&probe_hash) {
    continue;
}
```

After successful upsert:

```rust
let rows_hash = crate::hash::rule_set_hash(&rows);
db::update_source_success(&conn, &source.csv_url, &probe_hash, &rows_hash, &observed_at)?;
```

- [ ] **Step 4: Run daemon tests**

Run:

```bash
cargo test -p future-meta-daemon
```

Expected: PASS.

- [ ] **Step 5: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add crates/future-meta-daemon/src/db.rs crates/future-meta-daemon/src/refresh.rs crates/future-meta-daemon/src/hash.rs
git commit -m "feat: skip unchanged fee sources"
```

If not a git repo, skip commit and record changed files.

### Task 12: GitHub Actions and Cloudflare Pages Deployment

**Files:**
- Create: `.github/workflows/update-fee-data.yml`
- Create: `docs/deployment.md`

- [ ] **Step 1: Add deployment workflow**

Create `.github/workflows/update-fee-data.yml`:

```yaml
name: update-fee-data

on:
  schedule:
    - cron: "15 * * * *"
  workflow_dispatch:
    inputs:
      force_full:
        description: "Force full CSV refresh"
        required: false
        default: "false"

jobs:
  update:
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2

      - name: Restore daemon database cache
        uses: actions/cache@v4
        with:
          path: data/future-meta.sqlite
          key: future-meta-db-v1-${{ github.run_id }}
          restore-keys: |
            future-meta-db-v1-

      - name: Test
        run: cargo test --workspace && cargo test -p future-meta --features download

      - name: Refresh data
        run: |
          if [ "${{ github.event.inputs.force_full }}" = "true" ]; then
            cargo run -p future-meta-daemon -- refresh --db data/future-meta.sqlite --force-full
          else
            cargo run -p future-meta-daemon -- refresh --db data/future-meta.sqlite
          fi

      - name: Export Pages artifact
        run: cargo run -p future-meta-daemon -- export --db data/future-meta.sqlite --out public

      - name: Compute artifact hash
        id: artifact
        run: echo "sha=$(sha256sum public/latest.fmeta.zst | cut -d ' ' -f 1)" >> "$GITHUB_OUTPUT"

      - name: Deploy to Cloudflare Pages
        uses: cloudflare/wrangler-action@v3
        with:
          apiToken: ${{ secrets.CLOUDFLARE_API_TOKEN }}
          accountId: ${{ secrets.CLOUDFLARE_ACCOUNT_ID }}
          command: pages deploy public --project-name=future-meta --branch=main
```

- [ ] **Step 2: Add deployment docs**

Create `docs/deployment.md`:

```markdown
# future-meta Deployment

First version uses GitHub Actions for scheduled daemon execution and Cloudflare Pages free tier for static distribution.

## Required GitHub Secrets

- `CLOUDFLARE_API_TOKEN`: token allowed to deploy Cloudflare Pages.
- `CLOUDFLARE_ACCOUNT_ID`: Cloudflare account id.

## Cloudflare Pages Project

Project name: `future-meta`

The workflow deploys:

- `public/manifest.json`
- `public/latest.fmeta.zst`
- `public/artifacts/*.fmeta.zst`

## Manual Run

Use GitHub Actions `workflow_dispatch`.

Set `force_full=true` to bypass source probe skip logic and refresh every CSV source.

## Client URL

Default manifest URL:

`https://future-meta.pages.dev/manifest.json`
```

- [ ] **Step 3: Validate workflow YAML presence**

Run:

```bash
test -f .github/workflows/update-fee-data.yml
test -f docs/deployment.md
```

Expected: both commands exit 0.

- [ ] **Step 4: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add .github/workflows/update-fee-data.yml docs/deployment.md
git commit -m "ci: publish fee archive to cloudflare pages"
```

If not a git repo, skip commit and record changed files.

### Task 13: Final Verification and Dry Run

**Files:**
- No planned source edits unless verification finds a bug.

- [ ] **Step 1: Run formatting**

Run:

```bash
cargo fmt --all
```

Expected: exits 0.

- [ ] **Step 2: Run all tests**

Run:

```bash
cargo test --workspace
cargo test -p future-meta --features download
```

Expected: PASS.

- [ ] **Step 3: Run daemon local fixture pipeline through tests**

Run:

```bash
cargo test -p future-meta-daemon --test daemon_pipeline
```

Expected: PASS.

- [ ] **Step 4: Run CLI smoke checks**

Run:

```bash
cargo run -p future-meta-daemon -- inspect --db data/dev.sqlite
cargo run -p future-meta-daemon -- export --db data/dev.sqlite --out public
```

Expected:

- Inspect prints `contracts=0 fee_versions=0` for a new DB.
- Export creates `public/manifest.json` and `public/latest.fmeta.zst`, even if empty.

- [ ] **Step 5: Validate no derived fields are persisted**

Run:

```bash
rg "现价|涨/跌停板|保证金/每手|每跳毛利|手续费\\(开\\+平\\)|每跳净利" crates
```

Expected: matches only in test CSV fixture strings or comments explaining prohibited fields, not in SQLite schema, archive models, or exported model fields.

- [ ] **Step 6: Commit if git is available**

```bash
git rev-parse --is-inside-work-tree
git add .
git commit -m "test: verify future-meta fee pipeline"
```

If not a git repo, skip commit and record changed files.

## Self-Review Checklist

- Spec coverage:
  - No derived fields persisted: Tasks 7 and 13.
  - TqSdk symbol identity: Task 2.
  - Client archive and as-of queries: Tasks 4 and 5.
  - Optional download/cache: Task 6.
  - Daemon SQLite history: Task 8.
  - Incremental source skip: Task 11.
  - Cloudflare free deployment: Task 12.
- Completion scan:
  - No unresolved vague edge-case instruction should remain.
- Type consistency:
  - Use `symbol` for unique futures contract ids.
  - Use `underlying_symbol` for product-family query indexes.
  - Use `KQ.m@...` only as a main-continuous query alias.
  - Use `UnsupportedSymbolKind` for index, option, and spread inputs.
