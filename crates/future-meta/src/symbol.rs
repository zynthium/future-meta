//! Futures symbol parsing and normalization.

use serde::{Deserialize, Serialize};

use crate::error::FutureMetaError;

/// Parsed `TqSdk` symbol category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    /// A concrete futures contract.
    Futures,
    /// Main-continuous query alias such as `KQ.m@SHFE.cu`.
    MainContinuous,
    /// Index query alias such as `KQ.i@SHFE.bu`.
    Index,
    /// Option contract.
    Option,
    /// Spread contract.
    Spread,
    /// Recognized as outside this crate's supported symbol set.
    Unsupported,
}

/// Structured representation of a `TqSdk` symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSymbol {
    /// Original input string.
    pub raw: String,
    /// Parsed symbol category.
    pub kind: SymbolKind,
    /// Exchange or query namespace.
    pub exchange: String,
    /// Exchange-local symbol component.
    pub local: String,
    /// Underlying product symbol when derivable.
    pub underlying_symbol: Option<String>,
}

/// Parse a `TqSdk` symbol into its structured representation.
///
/// # Errors
///
/// Returns an error when the symbol is malformed or uses an unsupported exchange.
pub fn parse_symbol(raw: &str) -> Result<ParsedSymbol, FutureMetaError> {
    if raw.trim() != raw || raw.is_empty() {
        return Err(FutureMetaError::InvalidSymbol(raw.to_string()));
    }

    if let Some(local) = raw.strip_prefix("KQ.") {
        return parse_kq_symbol(raw, local);
    }

    let (exchange, local) = split_exchange_local(raw)?;
    validate_futures_exchange(exchange)?;

    if local.starts_with("SP ") {
        validate_spread_local(exchange, local)?;
        return Ok(ParsedSymbol {
            raw: raw.to_string(),
            kind: SymbolKind::Spread,
            exchange: exchange.to_string(),
            local: local.to_string(),
            underlying_symbol: None,
        });
    }

    let kind = if is_option_symbol(exchange, local)? {
        SymbolKind::Option
    } else {
        SymbolKind::Futures
    };
    let underlying_symbol = match kind {
        SymbolKind::Futures => Some(derive_underlying_from_local(exchange, local)?),
        SymbolKind::Option | SymbolKind::Spread => None,
        SymbolKind::MainContinuous | SymbolKind::Index | SymbolKind::Unsupported => unreachable!(),
    };

    Ok(ParsedSymbol {
        raw: raw.to_string(),
        kind,
        exchange: exchange.to_string(),
        local: local.to_string(),
        underlying_symbol,
    })
}

/// Normalize an exchange-local futures symbol into canonical `EXCHANGE.local` form.
///
/// # Errors
///
/// Returns an error when the exchange is unsupported or the local symbol is malformed.
pub fn normalize_futures_symbol(exchange: &str, local: &str) -> Result<String, FutureMetaError> {
    let exchange = exchange.trim().to_ascii_uppercase();
    let local = local.trim();
    validate_futures_exchange(&exchange)?;
    let product_len = leading_product_len(local);
    if product_len == 0 || product_len == local.len() {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }
    if !is_valid_product(&local[..product_len]) {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }
    if !local[product_len..].chars().all(|ch| ch.is_ascii_digit()) {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }

    let normalized_local = if exchange == "CZCE" {
        normalize_czce_local(local, product_len)?
    } else {
        validate_futures_local(&exchange, local)?;
        local.to_string()
    };

    Ok(format!("{exchange}.{normalized_local}"))
}

/// Return the parsed symbol's underlying product symbol.
///
/// # Errors
///
/// Returns an error when the symbol kind has no supported underlying symbol.
pub fn derive_underlying_symbol(symbol: &str) -> Result<String, FutureMetaError> {
    let parsed = parse_symbol(symbol)?;
    match parsed.kind {
        SymbolKind::Futures | SymbolKind::MainContinuous => parsed
            .underlying_symbol
            .ok_or_else(|| FutureMetaError::UnsupportedSymbolKind(symbol.to_owned())),
        SymbolKind::Index | SymbolKind::Option | SymbolKind::Spread | SymbolKind::Unsupported => {
            Err(FutureMetaError::UnsupportedSymbolKind(symbol.to_owned()))
        }
    }
}

fn parse_kq_symbol(raw: &str, local: &str) -> Result<ParsedSymbol, FutureMetaError> {
    let (kind, underlying) = if let Some(underlying) = local.strip_prefix("m@") {
        (SymbolKind::MainContinuous, underlying)
    } else if let Some(underlying) = local.strip_prefix("i@") {
        (SymbolKind::Index, underlying)
    } else {
        return Err(FutureMetaError::InvalidSymbol(raw.to_string()));
    };

    validate_underlying_symbol(underlying)?;

    Ok(ParsedSymbol {
        raw: raw.to_string(),
        kind,
        exchange: "KQ".to_string(),
        local: local.to_string(),
        underlying_symbol: Some(underlying.to_string()),
    })
}

fn split_exchange_local(raw: &str) -> Result<(&str, &str), FutureMetaError> {
    let (exchange, local) = raw
        .split_once('.')
        .ok_or_else(|| FutureMetaError::InvalidSymbol(raw.to_string()))?;
    if exchange.is_empty() || local.is_empty() {
        return Err(FutureMetaError::InvalidSymbol(raw.to_string()));
    }
    Ok((exchange, local))
}

fn validate_futures_exchange(exchange: &str) -> Result<(), FutureMetaError> {
    if matches!(exchange, "SHFE" | "DCE" | "CZCE" | "CFFEX" | "INE" | "GFEX") {
        Ok(())
    } else {
        Err(FutureMetaError::InvalidSymbol(format!(
            "unsupported exchange {exchange}"
        )))
    }
}

fn validate_underlying_symbol(underlying: &str) -> Result<(), FutureMetaError> {
    let (exchange, product) = split_exchange_local(underlying)?;
    validate_futures_exchange(exchange)?;
    if !is_valid_product(product) {
        return Err(FutureMetaError::UnknownUnderlyingSymbol(
            underlying.to_string(),
        ));
    }
    Ok(())
}

fn derive_underlying_from_local(exchange: &str, local: &str) -> Result<String, FutureMetaError> {
    let product_len = validate_futures_local(exchange, local)?;
    Ok(format!("{exchange}.{}", &local[..product_len]))
}

fn validate_futures_local(exchange: &str, local: &str) -> Result<usize, FutureMetaError> {
    let product_len = leading_product_len(local);
    if product_len == 0 || product_len == local.len() {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }

    if !is_valid_product(&local[..product_len]) {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }

    let suffix = &local[product_len..];
    if !suffix.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }

    let expected_suffix_len = if exchange == "CZCE" { 3 } else { 4 };
    if suffix.len() != expected_suffix_len {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }

    Ok(product_len)
}

fn validate_spread_local(exchange: &str, local: &str) -> Result<(), FutureMetaError> {
    let legs = local
        .strip_prefix("SP ")
        .ok_or_else(|| FutureMetaError::InvalidSymbol(format!("{exchange}.{local}")))?;
    let (first_leg, second_leg) = legs
        .split_once('&')
        .ok_or_else(|| FutureMetaError::InvalidSymbol(format!("{exchange}.{local}")))?;

    if first_leg.is_empty() || second_leg.is_empty() || second_leg.contains('&') {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }

    validate_futures_local(exchange, first_leg)?;
    validate_futures_local(exchange, second_leg)?;
    Ok(())
}

fn is_valid_product(product: &str) -> bool {
    !product.is_empty() && product.chars().all(|ch| ch.is_ascii_alphabetic())
}

fn leading_product_len(local: &str) -> usize {
    local
        .char_indices()
        .find_map(|(index, ch)| ch.is_ascii_digit().then_some(index))
        .unwrap_or(local.len())
}

fn normalize_czce_local(local: &str, product_len: usize) -> Result<String, FutureMetaError> {
    let (product, digits) = local.split_at(product_len);
    if !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(FutureMetaError::InvalidSymbol(format!("CZCE.{local}")));
    }

    match digits.len() {
        3 => Ok(local.to_string()),
        4 => Ok(format!("{product}{}", &digits[1..])),
        _ => Err(FutureMetaError::InvalidSymbol(format!("CZCE.{local}"))),
    }
}

fn is_option_symbol(exchange: &str, local: &str) -> Result<bool, FutureMetaError> {
    match exchange {
        "SHFE" | "INE" if is_shfe_style_option(local) => Ok(true),
        "DCE" | "CZCE" | "GFEX" | "CFFEX" if contains_dash_option_marker(local) => {
            validate_dash_style_option(exchange, local)?;
            Ok(true)
        }
        _ if contains_dash_option_marker(local) => Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        ))),
        _ => Ok(false),
    }
}

fn contains_dash_option_marker(local: &str) -> bool {
    local.contains("-C-") || local.contains("-P-")
}

fn validate_dash_style_option(exchange: &str, local: &str) -> Result<(), FutureMetaError> {
    let (contract, strike) = local
        .split_once("-C-")
        .or_else(|| local.split_once("-P-"))
        .ok_or_else(|| FutureMetaError::InvalidSymbol(format!("{exchange}.{local}")))?;

    if contract.contains("-C-")
        || contract.contains("-P-")
        || strike.contains("-C-")
        || strike.contains("-P-")
    {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }

    let product_len = validate_futures_local(exchange, contract)?;
    let suffix = &contract[product_len..];
    let expected_suffix_len = if exchange == "CZCE" { 3 } else { 4 };

    if suffix.len() != expected_suffix_len
        || strike.is_empty()
        || !strike.chars().all(|ch| ch.is_ascii_digit())
    {
        return Err(FutureMetaError::InvalidSymbol(format!(
            "{exchange}.{local}"
        )));
    }

    Ok(())
}

fn is_shfe_style_option(local: &str) -> bool {
    let Some(first_digit) = local.find(|ch: char| ch.is_ascii_digit()) else {
        return false;
    };

    if !is_valid_product(&local[..first_digit]) {
        return false;
    }

    let after_product = &local[first_digit..];
    let digit_count = after_product
        .chars()
        .take_while(char::is_ascii_digit)
        .count();
    if digit_count != 4 {
        return false;
    }

    let after_contract = &after_product[digit_count..];
    let Some(strike) = after_contract
        .strip_prefix('C')
        .or_else(|| after_contract.strip_prefix('P'))
    else {
        return false;
    };
    !strike.is_empty() && strike.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shfe_futures_contract() {
        let parsed = parse_symbol("SHFE.cu2607").expect("symbol parses");

        assert_eq!(parsed.kind, SymbolKind::Futures);
        assert_eq!(parsed.exchange, "SHFE");
        assert_eq!(parsed.local, "cu2607");
        assert_eq!(parsed.underlying_symbol.as_deref(), Some("SHFE.cu"));
    }

    #[test]
    fn derives_underlying_from_futures_symbol_string() {
        assert_eq!(
            derive_underlying_symbol("SHFE.cu2607").expect("underlying derives"),
            "SHFE.cu"
        );
    }

    #[test]
    fn normalizes_czce_four_digit_year_month_to_three_digits() {
        assert_eq!(
            normalize_futures_symbol("CZCE", "SR2609").expect("symbol normalizes"),
            "CZCE.SR609"
        );
    }

    #[test]
    fn normalizes_futures_symbol_after_trimming_parts() {
        assert_eq!(
            normalize_futures_symbol(" CZCE ", " SR2609 ").expect("symbol normalizes"),
            "CZCE.SR609"
        );
    }

    #[test]
    fn normalizes_non_czce_futures_without_changing_local_symbol() {
        assert_eq!(
            normalize_futures_symbol("SHFE", "cu2607").expect("symbol normalizes"),
            "SHFE.cu2607"
        );
        assert_eq!(
            normalize_futures_symbol("CFFEX", "IF2406").expect("symbol normalizes"),
            "CFFEX.IF2406"
        );
    }

    #[test]
    fn parses_main_continuous_alias() {
        let parsed = parse_symbol("KQ.m@SHFE.cu").expect("symbol parses");

        assert_eq!(parsed.kind, SymbolKind::MainContinuous);
        assert_eq!(parsed.exchange, "KQ");
        assert_eq!(parsed.local, "m@SHFE.cu");
        assert_eq!(parsed.underlying_symbol.as_deref(), Some("SHFE.cu"));
    }

    #[test]
    fn derives_underlying_from_main_continuous_symbol_string() {
        assert_eq!(
            derive_underlying_symbol("KQ.m@SHFE.cu").expect("underlying derives"),
            "SHFE.cu"
        );
    }

    #[test]
    fn parses_index_alias() {
        let parsed = parse_symbol("KQ.i@SHFE.bu").expect("symbol parses");

        assert_eq!(parsed.kind, SymbolKind::Index);
    }

    #[test]
    fn rejects_underlying_derivation_for_index_symbol_string() {
        assert!(matches!(
            derive_underlying_symbol("KQ.i@SHFE.bu"),
            Err(FutureMetaError::UnsupportedSymbolKind(symbol)) if symbol == "KQ.i@SHFE.bu"
        ));
    }

    #[test]
    fn recognizes_dce_option_contracts() {
        let parsed = parse_symbol("DCE.m1807-C-2450").expect("symbol parses");

        assert_eq!(parsed.kind, SymbolKind::Option);
        assert_eq!(parsed.underlying_symbol, None);
    }

    #[test]
    fn recognizes_shfe_option_contracts() {
        let parsed = parse_symbol("SHFE.au2004C308").expect("symbol parses");

        assert_eq!(parsed.kind, SymbolKind::Option);
        assert_eq!(parsed.underlying_symbol, None);
    }

    #[test]
    fn rejects_futures_local_with_extra_suffix_text() {
        assert!(parse_symbol("SHFE.cu2607x").is_err());
    }

    #[test]
    fn rejects_non_czce_futures_with_short_suffix() {
        assert!(parse_symbol("SHFE.cu2").is_err());
    }

    #[test]
    fn rejects_czce_futures_with_four_digit_canonical_suffix() {
        assert!(parse_symbol("CZCE.SR2609").is_err());
    }

    #[test]
    fn parses_czce_three_digit_canonical_futures_contract() {
        let parsed = parse_symbol("CZCE.SR903").expect("symbol parses");

        assert_eq!(parsed.kind, SymbolKind::Futures);
        assert_eq!(parsed.exchange, "CZCE");
        assert_eq!(parsed.local, "SR903");
        assert_eq!(parsed.underlying_symbol.as_deref(), Some("CZCE.SR"));
    }

    #[test]
    fn recognizes_spread_contracts() {
        let parsed = parse_symbol("DCE.SP a1709&a1801").expect("symbol parses");

        assert_eq!(parsed.kind, SymbolKind::Spread);
    }

    #[test]
    fn rejects_ampersand_without_spread_prefix() {
        assert!(parse_symbol("DCE.foo&bar").is_err());
    }

    #[test]
    fn rejects_malformed_spread_missing_second_leg() {
        assert!(parse_symbol("DCE.SP a1709&").is_err());
    }

    #[test]
    fn rejects_futures_local_with_invalid_product() {
        assert!(parse_symbol("SHFE.cu_2607").is_err());
    }

    #[test]
    fn rejects_kq_underlying_with_invalid_product() {
        assert!(parse_symbol("KQ.m@SHFE.cu_foo").is_err());
    }

    #[test]
    fn rejects_option_missing_strike() {
        assert!(parse_symbol("DCE.m1807-C-").is_err());
    }

    #[test]
    fn rejects_option_with_short_contract_suffix() {
        assert!(parse_symbol("DCE.m18-C-2450").is_err());
    }
}
