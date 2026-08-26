use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use future_meta::archive::decode_archive_bytes;
use future_meta::symbol::derive_underlying_symbol;
use future_meta::{ContractHandle, FutureMeta};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, UtcOffset};

const DEFAULT_ARCHIVE_PATH: &str = "public/latest.fmeta.zst";
const DEFAULT_QUERY_ITERS: usize = 1_000_000;
const DEFAULT_LOAD_ITERS: usize = 100;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let bytes = fs::read(&config.archive_path)?;
    let archive = decode_archive_bytes(&bytes)?;

    println!("archive_path={}", config.archive_path.display());
    println!("compressed_bytes={}", bytes.len());
    println!("contracts={}", archive.contracts.len());
    println!("fee_versions={}", archive.fee_versions.len());
    println!("history_start={}", archive.history_start);
    println!("history_end={}", archive.history_end);
    println!("query_iters={}", config.query_iters);
    println!("load_iters={}", config.load_iters);

    let decode_result = measure_decode(&bytes, config.load_iters)?;
    print_measurement("decode_archive_bytes", config.load_iters, decode_result);

    let build_result = measure_index_build(&archive, config.load_iters)?;
    print_measurement("FutureMeta::from_archive", config.load_iters, build_result);

    let meta = FutureMeta::from_archive(archive.clone())?;
    let clone_result = measure_clone(&meta, config.query_iters);
    print_measurement("FutureMeta::clone", config.query_iters, clone_result);
    let at = archive.history_end.clone();
    let at_time = OffsetDateTime::parse(&at, &Rfc3339)?;
    let trading_date = exchange_date(at_time);
    let start_unix_nanos = i64::try_from(at_time.unix_timestamp_nanos())?;
    let contract_symbols = current_contract_symbols(&archive);
    let contract_handles = current_contract_handles(&meta, &contract_symbols)?;
    let underlying_symbols = current_underlying_symbols(&contract_symbols);

    println!("current_contract_samples={}", contract_symbols.len());
    println!("current_contract_handle_samples={}", contract_handles.len());
    println!("current_underlying_samples={}", underlying_symbols.len());

    if contract_symbols.is_empty() {
        return Err("archive has no open-ended current contract fee records".into());
    }

    let benchmarks = BenchmarkData {
        meta: &meta,
        contract_symbols: &contract_symbols,
        contract_handles: &contract_handles,
        underlying_symbols: &underlying_symbols,
        at: &at,
        at_time,
        trading_date,
        config: &config,
    };
    run_prepared_benchmarks(&benchmarks, start_unix_nanos)?;
    run_query_benchmarks(&benchmarks);

    Ok(())
}

#[derive(Debug)]
struct Config {
    archive_path: PathBuf,
    query_iters: usize,
    load_iters: usize,
}

struct BenchmarkData<'a> {
    meta: &'a FutureMeta,
    contract_symbols: &'a [String],
    contract_handles: &'a [ContractHandle],
    underlying_symbols: &'a [String],
    at: &'a str,
    at_time: OffsetDateTime,
    trading_date: Date,
    config: &'a Config,
}

impl Config {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let mut args = env::args().skip(1);
        let archive_path = args
            .next()
            .map_or_else(|| PathBuf::from(DEFAULT_ARCHIVE_PATH), PathBuf::from);
        let query_iters = args
            .next()
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(DEFAULT_QUERY_ITERS);
        let load_iters = args
            .next()
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(DEFAULT_LOAD_ITERS);

        Ok(Self {
            archive_path,
            query_iters,
            load_iters,
        })
    }
}

fn run_prepared_benchmarks(
    data: &BenchmarkData<'_>,
    start_unix_nanos: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let trading_day_result =
        measure_trading_day(data.meta, data.trading_date, data.config.load_iters)?;
    print_measurement(
        "FutureMeta::for_trading_day",
        data.config.load_iters,
        trading_day_result,
    );

    let day = data.meta.for_trading_day(data.trading_date)?;
    let prepared_handles = data
        .contract_handles
        .iter()
        .copied()
        .filter(|handle| day.prepare_fee(*handle).is_ok())
        .collect::<Vec<_>>();
    let handle = *prepared_handles
        .first()
        .ok_or("archive has no current fees that can be prepared")?;
    println!("prepared_handle_samples={}", prepared_handles.len());

    let cursor_prepare_result = measure_prepare_cursors(
        data.meta,
        std::slice::from_ref(&handle),
        data.trading_date,
        start_unix_nanos,
        data.config.load_iters,
    )?;
    print_measurement(
        "FutureMeta::prepare_fee_cursors",
        data.config.load_iters,
        cursor_prepare_result,
    );

    let prepared_result = measure_prepared_fee_queries(
        data.meta,
        handle,
        data.trading_date,
        start_unix_nanos,
        data.config.query_iters,
    )?;
    print_measurement(
        "PreparedFeeCursors::advance_and_get_unix_nanos",
        data.config.query_iters,
        prepared_result,
    );

    let cursor_advance_result = measure_cursor_day_advances(
        data.meta,
        handle,
        data.trading_date,
        start_unix_nanos,
        data.config.load_iters,
    )?;
    print_measurement(
        "PreparedFeeCursors::advance_to(next_day)",
        data.config.load_iters,
        cursor_advance_result,
    );
    Ok(())
}

fn run_query_benchmarks(data: &BenchmarkData<'_>) {
    let contract_result = measure_contract_asof_queries(
        data.meta,
        data.contract_symbols,
        data.at,
        data.config.query_iters,
    );
    print_measurement(
        "contract_fee_asof",
        data.config.query_iters,
        contract_result,
    );

    let contract_at_result = measure_contract_at_queries(
        data.meta,
        data.contract_symbols,
        data.at_time,
        data.config.query_iters,
    );
    print_measurement(
        "contract_fee_at",
        data.config.query_iters,
        contract_at_result,
    );

    let contract_on_result = measure_contract_on_queries(
        data.meta,
        data.contract_symbols,
        data.trading_date,
        data.config.query_iters,
    );
    print_measurement(
        "contract_fee_on",
        data.config.query_iters,
        contract_on_result,
    );

    let handle_at_result = measure_handle_at_queries(
        data.meta,
        data.contract_handles,
        data.at_time,
        data.config.query_iters,
    );
    print_measurement(
        "contract_fee_for_handle_at",
        data.config.query_iters,
        handle_at_result,
    );

    let handle_on_result = measure_handle_on_queries(
        data.meta,
        data.contract_handles,
        data.trading_date,
        data.config.query_iters,
    );
    print_measurement(
        "contract_fee_for_handle_on",
        data.config.query_iters,
        handle_on_result,
    );

    run_underlying_benchmarks(data);
}

fn run_underlying_benchmarks(data: &BenchmarkData<'_>) {
    if data.underlying_symbols.is_empty() {
        return;
    }
    let underlying_result = measure_underlying_queries(
        data.meta,
        data.underlying_symbols,
        data.at,
        data.config.query_iters,
    );
    print_measurement(
        "underlying_fees_asof",
        data.config.query_iters,
        underlying_result,
    );

    let underlying_on_result = measure_underlying_on_queries(
        data.meta,
        data.underlying_symbols,
        data.trading_date,
        data.config.query_iters,
    );
    print_measurement(
        "underlying_fees_on(iterator)",
        data.config.query_iters,
        underlying_on_result,
    );
}

fn current_contract_symbols(archive: &future_meta::FeeArchiveV2) -> Vec<String> {
    let contract_by_id: HashMap<u32, &str> = archive
        .contracts
        .iter()
        .map(|contract| (contract.id, contract.symbol.as_str()))
        .collect();

    archive
        .fee_versions
        .iter()
        .filter(|fee| fee.valid_to.is_none())
        .filter_map(|fee| contract_by_id.get(&fee.contract_id).copied())
        .map(ToOwned::to_owned)
        .collect()
}

fn current_contract_handles(
    meta: &FutureMeta,
    symbols: &[String],
) -> Result<Vec<ContractHandle>, Box<dyn std::error::Error>> {
    Ok(symbols
        .iter()
        .map(|symbol| meta.resolve_contract(symbol))
        .collect::<Result<Vec<_>, _>>()?)
}

fn current_underlying_symbols(contract_symbols: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    contract_symbols
        .iter()
        .filter_map(|symbol| derive_underlying_symbol(symbol).ok())
        .filter(|underlying| seen.insert(underlying.clone()))
        .collect()
}

fn measure_clone(meta: &FutureMeta, iterations: usize) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(meta.clone());
    }
    start.elapsed()
}

fn measure_decode(bytes: &[u8], iterations: usize) -> Result<Duration, Box<dyn std::error::Error>> {
    let start = Instant::now();
    for _ in 0..iterations {
        let archive = decode_archive_bytes(bytes)?;
        black_box(archive.contracts.len());
        black_box(archive.fee_versions.len());
    }
    Ok(start.elapsed())
}

fn measure_index_build(
    archive: &future_meta::FeeArchiveV2,
    iterations: usize,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let mut elapsed = Duration::ZERO;
    for _ in 0..iterations {
        let archive = archive.clone();
        let start = Instant::now();
        let meta = FutureMeta::from_archive(archive)?;
        elapsed += start.elapsed();
        black_box(meta.contracts().len());
    }
    Ok(elapsed)
}

fn measure_trading_day(
    meta: &FutureMeta,
    trading_date: Date,
    iterations: usize,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let start = Instant::now();
    for _ in 0..iterations {
        let day = meta.for_trading_day(trading_date)?;
        black_box(day.trading_date());
    }
    Ok(start.elapsed())
}

fn measure_prepare_cursors(
    meta: &FutureMeta,
    handles: &[ContractHandle],
    trading_date: Date,
    start_unix_nanos: i64,
    iterations: usize,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let start = Instant::now();
    for _ in 0..iterations {
        let cursors =
            meta.prepare_fee_cursors(handles.iter().copied(), trading_date, start_unix_nanos)?;
        black_box(cursors.len());
    }
    Ok(start.elapsed())
}

fn measure_prepared_fee_queries(
    meta: &FutureMeta,
    handle: ContractHandle,
    trading_date: Date,
    start_unix_nanos: i64,
    iterations: usize,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let mut cursors = meta.prepare_fee_cursors([handle], trading_date, start_unix_nanos)?;
    let start = Instant::now();
    for _ in 0..iterations {
        let fee = cursors.advance_and_get_unix_nanos(0, start_unix_nanos)?;
        black_box(fee.open_amount(70_000.0, 1.0));
    }
    Ok(start.elapsed())
}

fn measure_cursor_day_advances(
    meta: &FutureMeta,
    handle: ContractHandle,
    trading_date: Date,
    start_unix_nanos: i64,
    iterations: usize,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let mut cursors = meta.prepare_fee_cursors([handle], trading_date, start_unix_nanos)?;
    let mut date = trading_date;
    let start = Instant::now();
    for _ in 0..iterations {
        date = date.next_day().ok_or("benchmark date has no next day")?;
        cursors.advance_to(date, exchange_midnight_unix_nanos(date)?)?;
        black_box(cursors.current(0)?);
    }
    Ok(start.elapsed())
}

fn measure_contract_asof_queries(
    meta: &FutureMeta,
    symbols: &[String],
    at: &str,
    iterations: usize,
) -> Duration {
    let start = Instant::now();
    for index in 0..iterations {
        let symbol = &symbols[index % symbols.len()];
        let fee = meta
            .contract_fee_asof(symbol, at)
            .unwrap_or_else(|err| panic!("contract_fee_asof failed for {symbol} at {at}: {err}"));
        black_box(fee.rule_hash.as_str());
    }
    start.elapsed()
}

fn measure_contract_at_queries(
    meta: &FutureMeta,
    symbols: &[String],
    at: OffsetDateTime,
    iterations: usize,
) -> Duration {
    let start = Instant::now();
    for index in 0..iterations {
        let symbol = &symbols[index % symbols.len()];
        let fee = meta
            .contract_fee_at(symbol, at)
            .unwrap_or_else(|err| panic!("contract_fee_at failed for {symbol} at {at}: {err}"));
        black_box(fee.rule_hash.as_str());
    }
    start.elapsed()
}

fn measure_contract_on_queries(
    meta: &FutureMeta,
    symbols: &[String],
    trading_date: Date,
    iterations: usize,
) -> Duration {
    let start = Instant::now();
    for index in 0..iterations {
        let symbol = &symbols[index % symbols.len()];
        let fee = meta
            .contract_fee_on(symbol, trading_date)
            .unwrap_or_else(|err| {
                panic!("contract_fee_on failed for {symbol} on {trading_date}: {err}")
            });
        black_box(fee.rule_hash.as_str());
    }
    start.elapsed()
}

fn measure_handle_at_queries(
    meta: &FutureMeta,
    handles: &[ContractHandle],
    at: OffsetDateTime,
    iterations: usize,
) -> Duration {
    let start = Instant::now();
    for index in 0..iterations {
        let handle = handles[index % handles.len()];
        let fee = meta
            .contract_fee_for_handle_at(handle, at)
            .unwrap_or_else(|err| {
                panic!(
                    "contract_fee_for_handle_at failed for contract id {} at {at}: {err}",
                    handle.contract_id()
                )
            });
        black_box(fee.rule_hash.as_str());
    }
    start.elapsed()
}

fn measure_handle_on_queries(
    meta: &FutureMeta,
    handles: &[ContractHandle],
    trading_date: Date,
    iterations: usize,
) -> Duration {
    let start = Instant::now();
    for index in 0..iterations {
        let handle = handles[index % handles.len()];
        let fee = meta
            .contract_fee_for_handle_on(handle, trading_date)
            .unwrap_or_else(|err| {
                panic!(
                    "contract_fee_for_handle_on failed for contract id {} on {trading_date}: {err}",
                    handle.contract_id()
                )
            });
        black_box(fee.rule_hash.as_str());
    }
    start.elapsed()
}

fn measure_underlying_queries(
    meta: &FutureMeta,
    symbols: &[String],
    at: &str,
    iterations: usize,
) -> Duration {
    let start = Instant::now();
    for index in 0..iterations {
        let symbol = &symbols[index % symbols.len()];
        let fees = meta.underlying_fees_asof(symbol, at).unwrap_or_else(|err| {
            panic!("underlying_fees_asof failed for {symbol} at {at}: {err}")
        });
        black_box(fees.len());
    }
    start.elapsed()
}

fn measure_underlying_on_queries(
    meta: &FutureMeta,
    symbols: &[String],
    trading_date: Date,
    iterations: usize,
) -> Duration {
    let start = Instant::now();
    for index in 0..iterations {
        let symbol = &symbols[index % symbols.len()];
        let count = meta
            .underlying_fees_on(symbol, trading_date)
            .unwrap_or_else(|err| {
                panic!("underlying_fees_on failed for {symbol} on {trading_date}: {err}")
            })
            .count();
        black_box(count);
    }
    start.elapsed()
}

fn print_measurement(name: &str, iterations: usize, elapsed: Duration) {
    let total_ns = elapsed.as_nanos();
    let avg_ns = if iterations == 0 {
        0
    } else {
        total_ns / iterations as u128
    };
    let ops_per_sec = if elapsed.is_zero() {
        0
    } else {
        iterations as u128 * 1_000_000_000 / elapsed.as_nanos()
    };

    println!(
        "{name}: total_ms={:.3} avg_ns={} ops_per_sec={}",
        elapsed.as_secs_f64() * 1_000.0,
        avg_ns,
        ops_per_sec
    );
}

fn exchange_date(at: OffsetDateTime) -> Date {
    at.to_offset(UtcOffset::from_hms(8, 0, 0).expect("valid exchange UTC offset"))
        .date()
}

fn exchange_midnight_unix_nanos(date: Date) -> Result<i64, Box<dyn std::error::Error>> {
    let at = date.midnight().assume_offset(UtcOffset::from_hms(8, 0, 0)?);
    Ok(i64::try_from(at.unix_timestamp_nanos())?)
}
