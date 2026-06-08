use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use future_meta::archive::decode_archive_bytes;
use future_meta::model::ContractFee;
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
    let at = archive.history_end.clone();
    let at_time = OffsetDateTime::parse(&at, &Rfc3339)?;
    let trading_date = exchange_date(at_time);
    let contract_symbols = current_contract_symbols(&archive);
    let contract_handles = current_contract_handles(&meta, &contract_symbols)?;
    let underlying_symbols = current_underlying_symbols(&contract_symbols);
    let main_symbols = current_main_symbols(&meta, &underlying_symbols, &at);

    println!("current_contract_samples={}", contract_symbols.len());
    println!("current_contract_handle_samples={}", contract_handles.len());
    println!("current_underlying_samples={}", underlying_symbols.len());
    println!("current_main_samples={}", main_symbols.len());

    if contract_symbols.is_empty() {
        return Err("archive has no open-ended current contract fee records".into());
    }

    let contract_result =
        measure_contract_asof_queries(&meta, &contract_symbols, &at, config.query_iters);
    print_measurement("contract_fee_asof", config.query_iters, contract_result);

    let contract_at_result =
        measure_contract_at_queries(&meta, &contract_symbols, at_time, config.query_iters);
    print_measurement("contract_fee_at", config.query_iters, contract_at_result);

    let contract_on_result =
        measure_contract_on_queries(&meta, &contract_symbols, trading_date, config.query_iters);
    print_measurement("contract_fee_on", config.query_iters, contract_on_result);

    let handle_at_result =
        measure_handle_at_queries(&meta, &contract_handles, at_time, config.query_iters);
    print_measurement(
        "contract_fee_for_handle_at",
        config.query_iters,
        handle_at_result,
    );

    let handle_on_result =
        measure_handle_on_queries(&meta, &contract_handles, trading_date, config.query_iters);
    print_measurement(
        "contract_fee_for_handle_on",
        config.query_iters,
        handle_on_result,
    );

    if !underlying_symbols.is_empty() {
        let underlying_result =
            measure_underlying_queries(&meta, &underlying_symbols, &at, config.query_iters);
        print_measurement(
            "underlying_fees_asof",
            config.query_iters,
            underlying_result,
        );
    }

    if !main_symbols.is_empty() {
        let main_result = measure_main_queries(&meta, &main_symbols, &at, config.query_iters);
        print_measurement("main_contract_fee_asof", config.query_iters, main_result);
    }

    Ok(())
}

#[derive(Debug)]
struct Config {
    archive_path: PathBuf,
    query_iters: usize,
    load_iters: usize,
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

fn current_contract_symbols(archive: &future_meta::FeeArchiveV1) -> Vec<String> {
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

fn current_main_symbols(meta: &FutureMeta, underlying_symbols: &[String], at: &str) -> Vec<String> {
    underlying_symbols
        .iter()
        .map(|underlying| format!("KQ.m@{underlying}"))
        .filter(|symbol| meta.main_contract_fee_asof(symbol, at).is_ok())
        .collect()
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
    archive: &future_meta::FeeArchiveV1,
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

fn measure_main_queries(
    meta: &FutureMeta,
    symbols: &[String],
    at: &str,
    iterations: usize,
) -> Duration {
    let start = Instant::now();
    for index in 0..iterations {
        let symbol = &symbols[index % symbols.len()];
        let fee = meta
            .main_contract_fee_asof(symbol, at)
            .unwrap_or_else(|err| {
                panic!("main_contract_fee_asof failed for {symbol} at {at}: {err}")
            });
        black_box(contract_fee_rule_hash(fee));
    }
    start.elapsed()
}

fn contract_fee_rule_hash(fee: &ContractFee) -> &str {
    fee.rule_hash.as_str()
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
