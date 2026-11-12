"""Small approved rules plus fresh official rows; no historical maintenance store."""

import copy
import csv
import hashlib
import json
import math
import os
import re
from datetime import date, datetime, timedelta
from pathlib import Path

from update_schedule import CALENDAR


def trading_dates(start, end, calendar_path=CALENDAR):
    calendar = json.loads(Path(calendar_path).read_text())
    if calendar["schema"] != 1:
        raise ValueError("Unsupported official calendar")
    current, last = date.fromisoformat(start), date.fromisoformat(end)
    days = []
    while current <= last:
        year = calendar["years"].get(str(current.year))
        if year is None:
            raise ValueError(f"Official schedule calendar missing for {current.year}")
        if current.weekday() < 5 and current.isoformat() not in year["closed_weekdays"]:
            days.append(current.isoformat())
        current += timedelta(days=1)
    return days


def iso_date(value):
    if value is None:
        return None
    return (
        datetime.strptime(value, "%Y%m%d").date().isoformat()
        if len(value) == 8
        else date.fromisoformat(value).isoformat()
    )


def specification(rules, symbol, observed, *, auto_discover=None):
    from collect_contract_specs import applies_to_contract, instant, symbol_parts

    exchange, contract = symbol.split(".")
    _, _, product = symbol_parts(exchange, contract)
    matching = []
    for rule in rules.get("specifications", []):
        if rule["exchange"] != exchange or rule["product"] != product:
            continue
        if rule.get("full_symbol") and rule["full_symbol"] != symbol:
            continue
        scoped = dict(
            rule,
            delivery_month_from=rule.get("delivery_month_from") or None,
            delivery_month_to=rule.get("delivery_month_to") or None,
        )
        if not applies_to_contract(scoped, contract):
            continue
        start = rule.get("valid_from_trade_date") or rule["valid_from"]
        end = rule.get("valid_to_trade_date") or rule.get("valid_to")
        if instant(start) <= instant(observed) and (
            not end or instant(observed) < instant(end)
        ):
            matching.append(rule)
    if not matching or len({(r["lot_size"], r["tick_size"]) for r in matching}) != 1:
        should_discover = (
            auto_discover
            if auto_discover is not None
            else (
                rules.get("auto_discover_specs", False)
                or os.environ.get("FUTURE_META_AUTO_DISCOVER_SPECS") == "1"
            )
        )
        if should_discover:
            try:
                import spec_discovery

                discovered = spec_discovery.auto_register_specification(
                    rules, symbol, observed
                )
                if discovered:
                    return discovered
            except Exception:
                pass
        raise ValueError(
            f"Missing/conflicting approved specification: {symbol} {observed}"
        )
    return max(matching, key=lambda r: r["valid_from"])


def cffex_daily(baseline, snapshots, days, *, catalog_verified):
    if not catalog_verified:
        raise ValueError("CFFEX carry requires a verified official catalog")
    current = copy.deepcopy(baseline)
    pending = sorted(snapshots, key=lambda pair: pair[0])
    output = []
    for day in days:
        while pending and pending[0][0] <= day:
            _, current = pending.pop(0)
        if not current:
            raise ValueError(f"CFFEX has no approved baseline for {day}")
        for row in current:
            output.append(dict(row, date=day))
    return output


def month_days(rules, year, month):
    calendar = rules["lifecycle_calendars"].get(str(year))
    if calendar is None:
        raise ValueError(f"Missing lifecycle calendar for {year}")
    first = date(year, month, 1)
    days = []
    while first.month == month:
        if first.weekday() < 5 and first.isoformat() not in calendar["closed_weekdays"]:
            days.append(first.isoformat())
        first += timedelta(days=1)
    if not days:
        raise ValueError("Empty lifecycle month")
    return days


def next_day(rules, value):
    current = date.fromisoformat(value) + timedelta(days=1)
    for _ in range(32):
        if current.isoformat() in month_days(rules, current.year, current.month):
            return current.isoformat()
        current += timedelta(days=1)
    raise ValueError("No next official trading day")


def expiry(rules, exchange, product, year, month):
    days = month_days(rules, year, month)
    if exchange == "DCE" and product in ("eb", "eg", "pg", "lg", "bz", "jd", "lh"):
        return days[-4]
    if exchange == "CFFEX":
        is_bond = product in ("T", "TF", "TS", "TL")
        fridays = [
            date(year, month, n)
            for n in range(1, 32)
            if n
            <= int(
                (date(year + (month == 12), month % 12 + 1, 1) - timedelta(days=1)).day
            )
            and date(year, month, n).weekday() == 4
        ]
        target = fridays[1 if is_bond else 2].isoformat()
        return next((d for d in days if d >= target), None) or next_day(rules, days[-1])
    return days[(5 if exchange == "CZCE" and product == "ZC" else 10) - 1]


def new_lifecycle(rules, symbol, observed):
    exchange, contract = symbol.split(".")
    if exchange == "DCE" and contract.endswith("F"):
        from collect_contract_specs import symbol_parts

        _, _, product = symbol_parts(exchange, contract)
        approved = rules.get("monthly_average_lifecycles", {}).get(product)
        if not approved:
            raise ValueError(f"Missing approved monthly-average lifecycle: {symbol}")
        digits = re.fullmatch(r"[a-z]+(\d{4})F", contract)[1]
        year, month = 2000 + int(digits[:2]), int(digits[2:])
        if f"{year:04d}-{month:02d}" < approved["first_delivery_month"]:
            raise ValueError(f"Monthly-average contract predates launch: {symbol}")
        expiry_month = year * 12 + month - 2
        listing_month = year * 12 + month - 1 - approved["listing_months_before"]
        expires = month_days(rules, expiry_month // 12, expiry_month % 12 + 1)[-1]
        listing = max(approved["launch_trade_date"], month_days(rules, listing_month // 12, listing_month % 12 + 1)[0])
        if not listing <= observed <= expires:
            raise ValueError(f"Official monthly-average lifecycle conflicts with observation: {symbol} {observed}")
        return listing, expires, rules["lifecycle_calendars"][expires[:4]].get("assumption")
    product, digits = re.fullmatch(r"([A-Za-z]+)(\d+)", contract).groups()
    year = (
        2000 + int(digits[:2])
        if exchange != "CZCE"
        else next(
            y
            for y in range(int(observed[:4]) - 1, int(observed[:4]) + 3)
            if y % 10 == int(digits[0])
        )
    )
    month = int(digits[-2:])
    expires = expiry(rules, exchange, product, year, month)
    prior_y, prior_m = year - 1, month
    if exchange == "CFFEX":
        offset = (
            9
            if product in ("T", "TF", "TS", "TL")
            else (8 if month in (3, 6, 9, 12) else 2)
        )
        shifted = year * 12 + month - 1 - offset
        prior_y, prior_m = shifted // 12, shifted % 12 + 1
    listing = next_day(rules, expiry(rules, exchange, product, prior_y, prior_m))
    launch_dates = [
        (s.get("valid_from_trade_date") or s["valid_from"])[:10]
        for s in rules.get("specifications", [])
        if s.get("exchange") == exchange
        and s.get("product") == product
        and not s.get("full_symbol")
    ]
    launch = min(launch_dates) if launch_dates else None
    if launch:
        if listing < launch or (launch <= observed < listing):
            listing = launch
    # Contradiction is an error; never clamp an inferred date to the observation.
    if not listing <= observed <= expires:
        raise ValueError(
            f"Official lifecycle rule conflicts with observation: {symbol} {observed}"
        )
    assumption = rules["lifecycle_calendars"][str(year)].get("assumption")
    return listing, expires, assumption


def fee(kind, value, *, optional=False):
    if value in (None, ""):
        if optional:
            return None
        raise ValueError("Missing official fee")
    number = float(value)
    if not math.isfinite(number) or number < 0:
        raise ValueError("Invalid official fee")
    return {
        "kind": {
            "cny_per_lot": "CnyPerLot",
            "rate_per_10k": "TurnoverRatePerTenThousand",
        }[kind],
        "value": number,
        "raw_text": None,
    }


def margin(value):
    if value in (None, ""):
        raise ValueError("Missing official margin")
    value = float(value)
    if not math.isfinite(value) or not 0 <= value <= 100:
        raise ValueError("Invalid official margin")
    return value


def normalized_rows(records, archive, rules, lifecycles, exceptions):
    known = {row["symbol"]: row for row in archive["contracts"]}
    output = []
    for record in records:
        symbol = f"{record['exchange']}.{record['symbol']}"
        observed = record["date"]
        spec = specification(rules, symbol, observed)
        previous = known.get(symbol)
        if symbol in lifecycles:
            listing, expires = lifecycles[symbol]
            exceptions.get("assumed_expiry", {}).pop(symbol, None)
        elif record["exchange"] in ("SHFE", "INE"):
            raise ValueError(f"Missing dated official CBI: {symbol}")
        elif previous:
            listing, expires = (
                iso_date(previous["listing_date"]),
                iso_date(previous["expiry_date"]),
            )
        else:
            listing, expires, assumption = new_lifecycle(rules, symbol, observed)
            if assumption:
                key = (
                    "calendar-"
                    + hashlib.sha256(
                        json.dumps(assumption, sort_keys=True).encode()
                    ).hexdigest()[:12]
                )
                exceptions.setdefault("assumptions", {})[key] = assumption
                exceptions.setdefault("assumed_expiry", {})[symbol] = key
        if not listing or not expires:
            raise ValueError(f"Incomplete lifecycle: {symbol}")
        if observed > expires and record["exchange"] == "CFFEX":
            continue  # Explicit expiry filters an unchanged product-wide snapshot.
        if not listing <= observed <= expires:
            raise ValueError(f"Observation contradicts lifecycle: {symbol} {observed}")
        start = spec["valid_from"]
        if (
            spec.get("valid_from_phase") == "settlement"
            and previous
            and (previous["lot_size"], previous["tick_size"])
            != (spec["lot_size"], spec["tick_size"])
        ):
            raise ValueError(f"Unresolved settlement specification boundary: {symbol}")
        spec_start = start if "T" in start else start + "T00:00:00+08:00"
        if previous is None and listing > start[:10]:
            spec_start = listing + "T00:00:00+08:00"
        output.append(
            {
                "symbol": symbol,
                "listing_date": listing,
                "expiry_date": expires,
                "lot_size": spec["lot_size"],
                "tick_size": spec["tick_size"],
                "spec_effective_at": spec_start,
                "spec_trade_date": spec.get("valid_from_trade_date") or None,
                "buy_margin_rate": margin(record["long_margin_ratio"]),
                "sell_margin_rate": margin(record["short_margin_ratio"]),
                "open_fee": fee(record["fee_type"], record["open_fee"]),
                "close_yesterday_fee": fee(record["fee_type"], record["offset_fee"]),
                "close_today_fee": fee(record["fee_type"], record["close_today_fee"]),
                "intraday_open_fee": fee(
                    record["fee_type"], record.get("intraday_open_fee"), optional=True
                ),
                "trading_status": "Unknown",
                "is_main_contract": False,
                "source_updated_at": None,
            }
        )
    return output


def cffex_baseline(archive, previous):
    latest = {}
    for row in archive["fee_versions"]:
        latest[row["contract_id"]] = row
    rows = []
    for contract in archive["contracts"]:
        if not contract["symbol"].startswith("CFFEX.") or not (
            iso_date(contract["listing_date"])
            <= previous
            <= iso_date(contract["expiry_date"])
        ):
            continue
        record = latest.get(contract["id"])
        if record is None:
            raise ValueError(
                f"CFFEX baseline has no parameter state: {contract['symbol']}"
            )
        kind = {
            "CnyPerLot": "cny_per_lot",
            "TurnoverRatePerTenThousand": "rate_per_10k",
            "Zero": "cny_per_lot",
        }[record["open_fee"]["kind"]]
        row = {
            "date": previous,
            "exchange": "CFFEX",
            "symbol": contract["symbol"].split(".")[1],
            "fee_type": kind,
            "long_margin_ratio": record["buy_margin_rate"],
            "short_margin_ratio": record["sell_margin_rate"],
        }
        for source, target in [
            ("open_fee", "open_fee"),
            ("close_yesterday_fee", "offset_fee"),
            ("close_today_fee", "close_today_fee"),
            ("intraday_open_fee", "intraday_open_fee"),
        ]:
            value = record.get(source)
            row[target] = None if value is None else value["value"]
            if (
                value
                and value["value"] != 0
                and value["kind"] != record["open_fee"]["kind"]
            ):
                raise ValueError("CFFEX snapshot has mixed nonzero fee units")
        rows.append(row)
    return rows


def collect_interval(archive, state, rules, days, work):
    """Collect every missing daily snapshot, sharing one cache/browser session."""
    import collect_settle_params as collector

    work = Path(work)
    work.mkdir(parents=True, exist_ok=True)
    store = collector.RawStore(work / "raw", None, False)
    previous = next(iter(state["checked_through"].values()))
    source_versions = copy.deepcopy(state["source_versions"])
    # Daily parameter checks cannot verify fields absent from those responses.
    # Full rule/document discovery is an explicit, separate maintenance command.
    source_versions.pop("rules", None)
    source_versions.pop("catalogs", None)
    source_versions["specifications"] = {
        "mode": "approved_rules",
        "approved_through": rules["approved_through"],
        "rules_sha256": hashlib.sha256(
            json.dumps(rules, sort_keys=True).encode()
        ).hexdigest(),
        "live_check_performed": False,
    }
    records = []
    lifecycle_by_day = {}
    for day in days:
        lifecycle_by_day[day] = {}
        for exchange in ("SHFE", "INE", "CZCE", "GFEX"):
            fetch = {
                "SHFE": lambda: collector.fetch_shfe_ine(day, "SHFE", store),
                "INE": lambda: collector.fetch_shfe_ine(day, "INE", store),
                "CZCE": lambda: collector.fetch_czce(day, store),
                "GFEX": lambda: collector.fetch_gfex_day(day, store),
            }[exchange]
            records.extend(collector.validate_records(fetch(), exchange, day, True))
        required_cbi = {
            f"{row['exchange']}.{row['symbol']}" for row in records
            if row['date'] == day and row['exchange'] in ('SHFE', 'INE')
        }
        cbi_sources = []
        for exchange, host in [("SHFE", "www.shfe.com.cn"), ("INE", "www.ine.cn")]:
            url = f"https://{host}/data/busiparamdata/future/ContractBaseInfo{day.replace('-', '')}.dat"
            raw_body = cached_cbi(store, exchange, day, url)
            body = json.loads(raw_body)
            cbi_sources.append({'url': url, 'sha256': hashlib.sha256(raw_body).hexdigest()})
            for symbol, values in cbi_lifecycles(body, exchange, day).items():
                old = lifecycle_by_day[day].get(symbol)
                if old is not None and old != values:
                    raise ValueError(f"Conflicting official CBI sources: {symbol}")
                lifecycle_by_day[day][symbol] = values
            if required_cbi and all(
                all(lifecycle_by_day[day].get(symbol, ())) and symbol in lifecycle_by_day[day]
                for symbol in required_cbi
            ):
                break
        source_versions['contract_base_info'] = {'report_date': day, 'sources': cbi_sources}
    dce_csv = work / "dce.csv"
    collector.prepare_csv(dce_csv)
    collector.collect_dce(days, str(dce_csv), store, True)
    with dce_csv.open() as handle:
        records.extend(dict(row) for row in csv.DictReader(handle))
    catalog = collector.get_cffex_catalog(store, min_date=previous)
    if not catalog:
        raise ValueError("Empty official CFFEX catalog")
    verify_cffex_anchor(store, source_versions["cffex_snapshot"], catalog, previous)
    fresh = []
    for observed, url in catalog:
        if previous < observed <= days[-1]:
            rows = collector.validate_records(
                collector.fetch_cffex_file(observed, url, store),
                "CFFEX",
                observed,
                True,
            )
            fresh.append((observed, rows))
            cached = json.loads((store.root / "cffex" / f"{observed}.json").read_text())
            source_versions["cffex_snapshot"] = {
                "date": observed,
                "url": url,
                "sha256": cached["sha256"],
            }
    baseline = cffex_baseline(archive, previous)
    records.extend(cffex_daily(baseline, fresh, days, catalog_verified=True))
    exceptions = copy.deepcopy(state["exceptions"])
    snapshots = []
    for day in days:
        for exchange in ("CFFEX", "CZCE", "DCE", "GFEX", "INE", "SHFE"):
            selected = [
                r for r in records if r["date"] == day and r["exchange"] == exchange
            ]
            normalized = normalized_rows(
                selected, archive, rules, lifecycle_by_day[day], exceptions
            )
            snapshots.append(
                {"exchange": exchange, "trade_date": day, "rows": normalized}
            )
    observed_symbols = {r["symbol"] for s in snapshots for r in s["rows"]}
    exceptions["without_observations"] = [
        s
        for s in exceptions.get("without_observations", [])
        if s not in observed_symbols
    ]
    return snapshots, source_versions, exceptions


def cached_cbi(store, exchange, day, url):
    """Keep this round's dated metadata when another runner resumes collection."""
    from collect_contract_specs import fetch_cbi_body

    key = f"cbi-{day}"
    body = store.cached(exchange, day, url, key=key)
    if body is None:
        body = fetch_cbi_body(url)
        store.save(exchange, day, url, body, key=key)
    return body


def verify_cffex_anchor(store, anchor, catalog, through):
    eligible = [(day, url) for day, url in catalog if day <= through]
    if not eligible or max(eligible) != (anchor["date"], anchor["url"]):
        raise ValueError(
            "Official historical CFFEX catalog revised; local review required"
        )
    body = store.get("CFFEX", anchor["date"], anchor["url"])
    if hashlib.sha256(body).hexdigest() != anchor["sha256"]:
        raise ValueError(
            "Official historical CFFEX baseline revised; local review required"
        )


def cbi_lifecycles(body, exchange, day):
    """Validate the dated metadata response before merging any lifecycle fields."""
    from collect_contract_specs import symbol_parts

    if body.get("report_date") and iso_date(body["report_date"]) != day:
        raise ValueError("CBI report date mismatch")
    items = body.get("ContractBaseInfo")
    if not isinstance(items, list) or not items:
        raise ValueError("Empty official CBI")
    output = {}
    for item in items:
        if iso_date(item.get("TRADINGDAY")) != day:
            raise ValueError("CBI record date mismatch")
        parts = symbol_parts(exchange, item.get("INSTRUMENTID", ""))
        if parts:
            symbol = f"{parts[0]}.{parts[1]}"
            values = (iso_date(item.get("OPENDATE")), iso_date(item.get("EXPIREDATE")))
            if symbol in output and output[symbol] != values:
                raise ValueError(f"Conflicting official CBI rows: {symbol}")
            output[symbol] = values
    return output
