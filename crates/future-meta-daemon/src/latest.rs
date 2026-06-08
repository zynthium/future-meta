//! Latest total-page HTML parsing.

use anyhow::{Result, anyhow};
use future_meta::fee::{parse_fee_spec, parse_optional_f64};
use future_meta::model::{FeeSpec, TradingStatus};
use future_meta::symbol::normalize_futures_symbol;
use scraper::{ElementRef, Html, Selector};

/// Stable source probe component for the latest table on the total page.
pub const LATEST_TABLE_PROBE_KEY: &str = "table#heyuetbl";

/// Latest row normalized down to allowed dynamic fields.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestRow {
    /// TqSdk-style futures contract symbol.
    pub symbol: String,
    /// Listing date from the source when present.
    pub listing_date: Option<String>,
    /// Expiry date from the source when present.
    pub expiry_date: Option<String>,
    /// Trading status reported or implied by the total page.
    pub trading_status: TradingStatus,
    /// Long-side margin percentage points when known, e.g. `12.0` means 12%.
    pub buy_margin_rate: Option<f64>,
    /// Short-side margin percentage points when known, e.g. `12.0` means 12%.
    pub sell_margin_rate: Option<f64>,
    /// Open position fee rule.
    pub open_fee: FeeSpec,
    /// Close-yesterday position fee rule.
    pub close_yesterday_fee: FeeSpec,
    /// Close-today position fee rule.
    pub close_today_fee: FeeSpec,
    /// Number of units per lot when the latest page exposes it directly.
    pub lot_size: Option<f64>,
    /// Minimum price tick when the latest page exposes it directly.
    pub tick_size: Option<f64>,
    /// Source fee update timestamp when present.
    pub source_updated_at: Option<String>,
    /// Whether the source remark marks this as a main contract.
    pub is_main_contract: bool,
}

/// Parsed latest snapshot from the total page.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestSnapshot {
    /// Global source fee update timestamp printed near the Excel button.
    pub source_updated_at: Option<String>,
    /// Latest allowed rows.
    pub rows: Vec<LatestRow>,
    /// Rows skipped because the local code is not a supported futures symbol.
    pub skipped_invalid_symbols: usize,
}

/// Parse the 9qihuo total page latest table into allowed latest rows.
///
/// # Errors
///
/// Returns an error when `table#heyuetbl` is missing, selectors cannot be
/// parsed, a data row has no exchange section, or a futures symbol is invalid.
pub fn parse_latest_html(html: &str) -> Result<LatestSnapshot> {
    let document = Html::parse_document(html);
    let table_selector = selector("table#heyuetbl")?;
    let tr_selector = selector("tr")?;
    let td_selector = selector("td")?;
    let b_selector = selector("b")?;
    let table = document
        .select(&table_selector)
        .next()
        .ok_or_else(|| anyhow!("missing latest table #{LATEST_TABLE_PROBE_KEY}"))?;
    let snapshot_source_updated_at = extract_source_updated_at(html);
    let mut current_exchange = None::<String>;
    let mut skipped_invalid_symbols = 0usize;
    let mut rows = Vec::new();

    for tr in table.select(&tr_selector) {
        let cells = tr.select(&td_selector).collect::<Vec<_>>();
        if cells.is_empty() {
            continue;
        }

        if let Some(section) = cells.iter().find(|cell| has_class(cell, "jysname")) {
            current_exchange = Some(exchange_code(&all_text(section))?.to_owned());
            continue;
        }

        let Some(contract_cell) = cells.first().filter(|cell| has_class(cell, "heyuealink")) else {
            continue;
        };
        if cells.len() < 13 {
            return Err(anyhow!(
                "latest table data row has {} cells; expected at least 13",
                cells.len()
            ));
        }

        let exchange = current_exchange
            .as_deref()
            .ok_or_else(|| anyhow!("latest table data row appears before exchange section"))?;
        let local = contract_local(contract_cell, &b_selector)?;
        let symbol = match normalize_futures_symbol(exchange, &local) {
            Ok(symbol) => symbol,
            Err(_) => {
                skipped_invalid_symbols += 1;
                continue;
            }
        };
        let row_source_updated_at = contract_cell
            .value()
            .attr("title")
            .and_then(extract_source_updated_at)
            .or_else(|| snapshot_source_updated_at.clone());

        rows.push(LatestRow {
            symbol,
            listing_date: None,
            expiry_date: None,
            trading_status: TradingStatus::Trading,
            buy_margin_rate: parse_margin_cell(primary_text(&cells[3]).as_deref())?,
            sell_margin_rate: parse_margin_cell(primary_text(&cells[4]).as_deref())?,
            open_fee: parse_fee_spec(primary_text(&cells[6]).as_deref().unwrap_or("")),
            close_yesterday_fee: parse_fee_spec(primary_text(&cells[7]).as_deref().unwrap_or("")),
            close_today_fee: parse_fee_spec(primary_text(&cells[8]).as_deref().unwrap_or("")),
            lot_size: None,
            tick_size: None,
            source_updated_at: row_source_updated_at,
            is_main_contract: parse_main_contract(&all_text(&cells[12])),
        });
    }

    Ok(LatestSnapshot {
        source_updated_at: snapshot_source_updated_at,
        rows,
        skipped_invalid_symbols,
    })
}

fn selector(text: &str) -> Result<Selector> {
    Selector::parse(text).map_err(|err| anyhow!("invalid selector {text}: {err}"))
}

fn exchange_code(name: &str) -> Result<&'static str> {
    match name.trim() {
        "上海期货交易所" => Ok("SHFE"),
        "大连商品交易所" => Ok("DCE"),
        "郑州商品交易所" => Ok("CZCE"),
        "中国金融期货交易所" => Ok("CFFEX"),
        "上海国际能源交易中心" => Ok("INE"),
        "广州期货交易所" => Ok("GFEX"),
        other => Err(anyhow!("unknown latest table exchange section: {other}")),
    }
}

fn contract_local(cell: &ElementRef<'_>, b_selector: &Selector) -> Result<String> {
    if let Some(text) = cell.select(b_selector).find_map(|node| primary_text(&node)) {
        return Ok(text);
    }

    let text = all_text(cell);
    if let Some((_, tail)) = text.rsplit_once('(')
        && let Some((inside, _)) = tail.split_once(')')
    {
        let inside = inside.trim();
        if !inside.is_empty() {
            return Ok(inside.to_owned());
        }
    }

    Err(anyhow!(
        "missing contract code in latest table cell: {text}"
    ))
}

fn parse_margin_cell(text: Option<&str>) -> Result<Option<f64>> {
    let Some(text) = text else {
        return Ok(None);
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return Ok(None);
    }

    let numeric = trimmed.strip_suffix('%').unwrap_or(trimmed);
    parse_optional_f64(numeric)
        .map(Some)
        .ok_or_else(|| anyhow!("invalid margin cell: {text}"))
}

fn parse_main_contract(text: &str) -> bool {
    matches!(text.trim(), "主力" | "主力合约")
}

fn has_class(cell: &ElementRef<'_>, class: &str) -> bool {
    cell.value()
        .attr("class")
        .is_some_and(|classes| classes.split_ascii_whitespace().any(|value| value == class))
}

fn primary_text(cell: &ElementRef<'_>) -> Option<String> {
    cell.text()
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

fn all_text(cell: &ElementRef<'_>) -> String {
    cell.text()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("")
}

fn extract_source_updated_at(text: &str) -> Option<String> {
    let (_, tail) = text.split_once("手续费更新时间：")?;
    let timestamp = tail
        .chars()
        .take_while(|ch| {
            !matches!(
                ch,
                '，' | ',' | '。' | ')' | '）' | '<' | '\'' | '"' | '\n' | '\r'
            )
        })
        .collect::<String>();
    let timestamp = timestamp.trim();
    (!timestamp.is_empty()).then(|| timestamp.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use future_meta::model::FeeKind;

    const LATEST_HTML: &str = r#"
      <div>
        <input class="fee_hide_obj" type="button" onclick="tableToExcel('heyuetbl', 'name', '期货手续费和保证金一览表2026年06月更新(九期网).xls')" value="下载手续费Excel表格">
        （手续费更新时间：2026-06-05 23:40:54.805，价格更新时间：2026-06-08 15:26:53.471。）
      </div>
      <table id="heyuetbl">
        <tr><td colspan="15" class="jysname"><a>上海期货交易所</a></td></tr>
        <tr><td rowspan="2"><strong>合约品种</strong></td></tr>
        <tr><td>买开%</td><td>卖开%</td><td>保证金/每手</td><td>开仓</td><td>平昨</td><td>平今</td></tr>
        <tr>
          <td class="heyuealink" title="手续费更新时间：2026-06-05 21:26:53.200"><a>白银2606 (<b>ag2606</b>)</a></td>
          <td class="fee_hide_obj">17715</td>
          <td class="fee_hide_obj"><span>21258</span>/<span>14172</span></td>
          <td>22%</td>
          <td class="fee_hide_obj">22%</td>
          <td>58459.5元</td>
          <td>0.5/万分之<br><nobr class="js_single_fee">(13.3元)</nobr></td>
          <td>0.5/万分之<br><nobr class="js_single_fee">(13.3元)</nobr></td>
          <td>0.5/万分之<br><nobr class="js_single_fee">(13.3元)</nobr></td>
          <td class="fee_hide_obj">15</td>
          <td class="fee_hide_obj">26.6元</td>
          <td>-11.6</td>
          <td class="fee_hide_obj"></td>
        </tr>
        <tr>
          <td class="heyuealink"><a>聚乙烯月均价2607 (<b>l2607F</b>)</a></td>
          <td></td><td></td>
          <td>10%</td><td>10%</td><td></td>
          <td>1元</td><td>1元</td><td>1元</td>
          <td></td><td></td><td></td><td></td>
        </tr>
        <tr><td colspan="15" class="jysname">中国金融期货交易所</td></tr>
        <tr>
          <td class="heyuealink"><a>沪深300指数2606 (<b>IF2606</b>)</a></td>
          <td></td><td></td>
          <td>12%</td>
          <td>12%</td>
          <td>168890.4元</td>
          <td><span>0.23/万分之</span><br><nobr class="js_single_fee">(33元)</nobr></td>
          <td><span>0.23/万分之</span><br><nobr class="js_single_fee">(33元)</nobr></td>
          <td><span>2.3/万分之</span><br><nobr class="js_single_fee">(330.2元)</nobr></td>
          <td></td><td></td><td></td>
          <td>主力合约</td>
        </tr>
      </table>
    "#;

    #[test]
    fn parses_latest_total_page_table() {
        let snapshot = parse_latest_html(LATEST_HTML).unwrap();

        assert_eq!(
            snapshot.source_updated_at.as_deref(),
            Some("2026-06-05 23:40:54.805")
        );
        assert_eq!(snapshot.rows.len(), 2);
        assert_eq!(snapshot.skipped_invalid_symbols, 1);

        let first = &snapshot.rows[0];
        assert_eq!(first.symbol, "SHFE.ag2606");
        assert_eq!(first.buy_margin_rate, Some(22.0));
        assert_eq!(first.sell_margin_rate, Some(22.0));
        assert_eq!(first.open_fee.kind, FeeKind::TurnoverRatePerTenThousand);
        assert_eq!(first.open_fee.value, Some(0.5));
        assert_eq!(
            first.source_updated_at.as_deref(),
            Some("2026-06-05 21:26:53.200")
        );
        assert!(!first.is_main_contract);
        assert_eq!(first.lot_size, None);
        assert_eq!(first.tick_size, None);

        let second = &snapshot.rows[1];
        assert_eq!(second.symbol, "CFFEX.IF2606");
        assert_eq!(second.close_today_fee.value, Some(2.3));
        assert_eq!(
            second.source_updated_at.as_deref(),
            Some("2026-06-05 23:40:54.805")
        );
        assert!(second.is_main_contract);
    }
}
