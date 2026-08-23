# Public API Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Repository instructions require inline execution and disable subagent dispatch.

**Goal:** Remove avoidable full-archive work, deep copies, allocations, and async runtime blocking from the `future-meta` public client API while preserving compatibility.

**Architecture:** Keep archive ownership inside `FutureMeta`, but wrap immutable archive and index collections in `Arc` so cloning remains handle-compatible and constant-time. Separate selected-handle cursor preparation from the all-contract `TradingDayMeta` snapshot. Add allocation-free iterator and parsed-time query tiers, then keep existing string/`Vec` APIs as compatibility wrappers. Move CPU-heavy decode/index construction to Tokio blocking workers.

**Tech Stack:** Rust 2024, `time`, Tokio, existing integration tests, release `perf_smoke` example.

---

### Task 1: Shared client storage and selected-handle cursors

**Files:**
- Modify: `crates/future-meta/src/query.rs`
- Test: `crates/future-meta/tests/client_archive.rs`

- [ ] **Step 1: Write failing clone-sharing test**

Add an integration test which clones `FutureMeta` and asserts `std::ptr::eq(meta.contracts(), cloned.contracts())`. Existing deep clone must fail this assertion.

- [ ] **Step 2: Run test and verify red**

Run:

```bash
cargo test -p future-meta --features download --test client_archive clone_shares_indexed_storage
```

Expected: assertion failure because cloned contract slices use different allocations.

- [ ] **Step 3: Make clone constant-time**

Change immutable archive and index fields to `Arc<...>` while keeping `handle_token` copied unchanged. Build ordinary collections in `from_archive`, then wrap each collection exactly once when constructing `FutureMeta`.

- [ ] **Step 4: Remove all-contract cursor rebuild**

Make `FutureMeta::prepare_fee_cursors` collect only caller handles, compute trading-day bounds directly, and prepare only those handles. Add reusable scratch fee storage to `PreparedFeeCursors`; on date changes, prepare selected handles into scratch storage, swap only after all preparation succeeds, then update date bounds and timestamp. Leave `for_trading_day` as explicit all-contract snapshot API.

- [ ] **Step 5: Run focused cursor and clone tests**

```bash
cargo test -p future-meta --features download --test client_archive clone_shares_indexed_storage
cargo test -p future-meta --features download --test client_archive prepared_fee_cursors
```

Expected: all selected tests pass.

### Task 2: Allocation-free underlying and main-contract queries

**Files:**
- Modify: `crates/future-meta/src/query.rs`
- Modify: `crates/future-meta/src/symbol.rs`
- Test: `crates/future-meta/tests/client_archive.rs`

- [ ] **Step 1: Write failing API tests**

Add tests for `underlying_fees_at`, `underlying_fees_on`, `main_contract_fee_at`, and `main_contract_fee_on`. Collect the iterator only inside tests when comparing results. These tests must initially fail to compile because methods do not exist.

- [ ] **Step 2: Verify red**

```bash
cargo test -p future-meta --features download --test client_archive parsed_time_underlying_and_main_queries
```

Expected: compile errors reporting missing methods.

- [ ] **Step 3: Implement query tiers**

Add iterator-returning `underlying_fees_at` and `underlying_fees_on`. Retain `underlying_fees_asof` by parsing once and collecting that iterator. Add `main_contract_fee_at` and `main_contract_fee_on`; make the string API parse time once and delegate. Add a crate-private borrowed parser for `KQ.m@...` so successful main-contract queries allocate no symbol strings and never construct an intermediate `Vec`.

- [ ] **Step 4: Verify green**

```bash
cargo test -p future-meta --features download --test client_archive parsed_time_underlying_and_main_queries
```

Expected: test passes.

### Task 3: Async CPU isolation and cross-crate inlining

**Files:**
- Modify: `crates/future-meta/src/query.rs`
- Modify: `crates/future-meta/src/download.rs`
- Modify: `crates/future-meta/src/error.rs`
- Test: `crates/future-meta/tests/client_archive.rs`

- [ ] **Step 1: Add blocking decode helper**

Introduce a crate-private async constructor which accepts owned compressed bytes and runs both `decode_archive_bytes` and `FutureMeta::from_archive` inside `tokio::task::spawn_blocking`. Map task cancellation or panic to a specific non-exhaustive `FutureMetaError` variant.

- [ ] **Step 2: Route both async loaders through helper**

Use the helper from `FutureMeta::load_file` and `load_or_fetch`. No archive decode or index construction may execute directly on an async executor thread.

- [ ] **Step 3: Mark hot public methods inline**

Add `#[inline]` to small `PreparedFeeBook` accessors and cursor access/advance methods used per tick. Do not add unsafe unchecked accessors.

- [ ] **Step 4: Verify loader behavior**

```bash
cargo test -p future-meta --features download --test client_archive load_file_decodes_archive
```

Expected: test passes.

### Task 4: Performance regression coverage

**Files:**
- Modify: `crates/future-meta/examples/perf_smoke.rs`

- [ ] **Step 1: Add measurements**

Measure constant-time `FutureMeta::clone`, `for_trading_day`, initial selected-handle cursor construction, cross-day cursor advance, prepared fee lookup plus amount calculation, iterator-based underlying queries, and parsed-date main-contract queries. If published data has no main marker, build a benchmark-only in-memory archive clone with one valid current fee marked as main so the main API is still executed.

- [ ] **Step 2: Run release smoke**

```bash
cargo run --release -q -p future-meta --example perf_smoke -- public/latest.fmeta.zst 1000000 10
```

Expected: command exits successfully and prints every added measurement without silently skipping main-contract coverage.

### Task 5: Full verification

**Files:**
- Verify all modified files.

- [ ] **Step 1: Format**

```bash
cargo fmt --all -- --check
```

- [ ] **Step 2: Test required matrices**

```bash
cargo test --workspace
cargo test -p future-meta --features download
cargo test -p future-meta-daemon --test daemon_pipeline
cargo test -p future-meta --features download --test client_archive
```

- [ ] **Step 3: Check and lint**

```bash
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

- [ ] **Step 4: Inspect diff**

Confirm only plan, client implementation, integration tests, and performance example changed. Do not stage generated `public/`, `data/`, or `target/` content.
