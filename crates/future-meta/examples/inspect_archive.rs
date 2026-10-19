//! Read a local archive, including legacy payloads, and optionally inspect one rule.
use future_meta::{FutureMeta, archive::decode_archive_bytes};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let path = args
        .first()
        .ok_or("usage: inspect_archive FILE [SYMBOL TIMESTAMP]")?;
    let bytes = std::fs::read(path)?;
    let archive = decode_archive_bytes(&bytes)?;
    let raw = zstd::stream::decode_all(bytes.as_slice())?;
    let (wire_version, _): (u32, usize) =
        bincode::serde::decode_from_slice(&raw, bincode::config::standard())?;
    println!(
        "schema={} model_schema={} contracts={} fee_versions={}",
        wire_version,
        archive.schema_version,
        archive.contracts.len(),
        archive.fee_versions.len()
    );
    if let (Some(symbol), Some(at)) = (args.get(1), args.get(2)) {
        let meta = FutureMeta::from_archive(archive)?;
        println!(
            "{}",
            serde_json::to_string_pretty(meta.contract_fee_asof(symbol, at)?)?
        );
    }
    Ok(())
}
