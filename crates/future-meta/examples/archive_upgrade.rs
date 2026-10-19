//! Upgrade a local archive without changing its query data or publishing it.

use std::{error::Error, path::PathBuf, time::Instant};

use future_meta::{
    FutureMeta,
    archive::{decode_archive_bytes, encode_archive_bytes, sha256_hex},
    model::SCHEMA_VERSION,
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let input = PathBuf::from(
        args.next()
            .ok_or("usage: archive_upgrade <input> <new-output> [compare-export]")?,
    );
    let output = PathBuf::from(args.next().ok_or("missing output path")?);
    if output.exists() {
        return Err("output already exists".into());
    }
    let original_bytes = std::fs::read(&input)?;
    let raw = zstd::stream::decode_all(original_bytes.as_slice())?;
    let (input_schema_version, _): (u32, usize) =
        bincode::serde::decode_from_slice(&raw, bincode::config::standard())?;
    let original = decode_archive_bytes(&original_bytes)?;
    let bytes = encode_archive_bytes(&original)?;
    let restored = decode_archive_bytes(&bytes)?;
    if restored != original {
        return Err("archive query data changed during upgrade".into());
    }
    if let Some(path) = args.next() {
        let mut exported = decode_archive_bytes(&std::fs::read(path)?)?;
        exported.generated_at.clone_from(&original.generated_at);
        if exported != original {
            return Err("daemon export changed data beyond its generation time".into());
        }
    }
    let client = FutureMeta::from_archive(restored)?;
    let symbols = original
        .contracts
        .iter()
        .map(|c| (c.id, c.symbol.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let step = (original.fee_versions.len() / 1000).max(1);
    let mut queries = 0;
    for v in original.fee_versions.iter().step_by(step).take(1000) {
        if client.contract_fee_asof(symbols[&v.contract_id], &v.valid_from)? != v {
            return Err("as-of query changed".into());
        }
        queries += 1;
    }
    let mut load_times = Vec::new();
    for _ in 0..8 {
        let start = Instant::now();
        let loaded = FutureMeta::from_archive(decode_archive_bytes(&bytes)?)?;
        std::hint::black_box(&loaded);
        load_times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    load_times.remove(0);
    load_times.sort_by(f64::total_cmp);
    std::fs::write(&output, &bytes)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version":SCHEMA_VERSION,"input_schema_version":input_schema_version,"input":input,"output":output,
            "input_bytes":original_bytes.len(),"output_bytes":bytes.len(),"sha256":sha256_hex(&bytes),
            "contracts":original.contracts.len(),"spec_versions":original.contract_spec_versions.len(),
            "fee_versions":original.fee_versions.len(),"query_samples":queries,
            "all_fields_equal":true,"decode_and_index_ms_median":load_times[load_times.len()/2]
        }))?
    );
    Ok(())
}
