//! Read a local archive, including legacy payloads, and optionally inspect one rule.
use future_meta::{FutureMeta, archive::decode_archive_bytes};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let path = args
        .first()
        .ok_or("usage: inspect_archive FILE [SYMBOL TIMESTAMP]")?;
    let archive = decode_archive_bytes(&std::fs::read(path)?)?;
    println!(
        "schema={} contracts={} fee_versions={}",
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
