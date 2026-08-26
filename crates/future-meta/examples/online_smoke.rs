use future_meta::{DownloadConfig, load_or_fetch};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let symbol = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "SHFE.cu2607".to_owned());
    let at = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "2026-06-08T08:26:12Z".to_owned());

    let cache_dir = std::env::temp_dir().join("future-meta-online-smoke");
    let meta = load_or_fetch(DownloadConfig {
        cache_dir,
        ..DownloadConfig::default()
    })
    .await?;

    let fee = meta.contract_fee_asof(&symbol, &at)?;
    println!(
        "contracts={} symbol={} at={} open_fee={:?} close_today_fee={:?} buy_margin={:?} source_updated_at={:?}",
        meta.contracts().len(),
        symbol,
        at,
        fee.open_fee,
        fee.close_today_fee,
        fee.buy_margin_rate,
        fee.source_updated_at
    );

    let concrete = fee;
    println!(
        "concrete={} at={} contract_id={} open_fee={:?}",
        symbol, at, concrete.contract_id, concrete.open_fee
    );

    Ok(())
}
