#!/usr/bin/env python3
"""Build candidate futures metadata from retained exchange evidence only.

No legacy database, guessed lifecycle, or built-in product specifications are used.
Curated product manifests must point to the original exchange document and SHA256.
Outputs include per-field provenance and explicit evidence/coverage gaps. The CLI
exits nonzero for incomplete candidates unless --allow-incomplete is supplied.
Manifest lot_size is the price multiplier in quotation units per lot. Boundary
phases date, instant, and settlement are retained; settlement is never assigned
an assumed clock time, and observations on that date require clarification.
"""

import argparse
import csv
import hashlib
import io
import json
import math
import re
import sys
import urllib.request
from collections import Counter, defaultdict
from datetime import date, datetime, time, timedelta, timezone
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urlparse

TZ = timezone(timedelta(hours=8))
INE_PRODUCTS = {"sc", "lu", "nr", "bc", "ec"}
DOMAINS = {
    "SHFE": ("shfe.com.cn", "shfe.cn"), "INE": ("ine.cn",),
    "DCE": ("dce.com.cn",), "CZCE": ("czce.com.cn",),
    "GFEX": ("gfex.com.cn",), "CFFEX": ("cffex.com.cn",),
}
META_COLUMNS = [
    "exchange", "symbol", "full_symbol", "variety", "variety_name",
    "lot_size", "tick_size", "tick_value", "quote_unit", "listing_date",
    "expiry_date", "last_delivery_date", "basis_price", "has_spec_change",
    "spec_versions_count", "first_observed_date", "last_observed_date",
    "listing_date_source", "expiry_date_source", "spec_status",
]
VERSION_COLUMNS = [
    "exchange", "symbol", "full_symbol", "variety", "variety_name",
    "lot_size", "tick_size", "tick_value", "quote_unit", "valid_from",
    "valid_to", "source_kind", "valid_from_phase", "valid_to_phase",
    "valid_from_trade_date", "valid_to_trade_date", "range_scope", "evidence_ids",
]


def iso_date(value, required=False):
    value = str(value or "").strip()
    if not value:
        if required:
            raise ValueError("missing date")
        return ""
    if re.fullmatch(r"\d{8}", value):
        value = f"{value[:4]}-{value[4:6]}-{value[6:]}"
    return date.fromisoformat(value).isoformat()


def instant(value):
    value = str(value or "").strip()
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", value):
        return datetime.combine(date.fromisoformat(value), time(), TZ)
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError(f"timestamp requires an explicit timezone: {value}")
    return parsed.astimezone(TZ)


def boundary(value):
    """Retain date precision instead of manufacturing an exact timestamp."""
    if value.timetz().replace(tzinfo=None) == time():
        return value.date().isoformat()
    return value.isoformat()


def phase_boundary(value, phase):
    if phase == "instant":
        return value.isoformat()
    # A settlement boundary is a date and a named exchange phase, not a
    # fabricated midnight or assumed 15:00 timestamp.
    if phase == "settlement":
        return value.date().isoformat()
    return boundary(value)


def positive(value, field):
    result = float(value)
    if not math.isfinite(result) or result <= 0:
        raise ValueError(f"{field} must be finite and positive: {value}")
    return result


def delivery_month(value):
    """Read an inclusive YYYY-MM contract scope, independently of effective dates."""
    value = str(value or "").strip()
    if not value:
        return None
    if not re.fullmatch(r"\d{4}-(0[1-9]|1[0-2])", value):
        raise ValueError(f"invalid contract delivery month: {value}")
    return value


def month_scopes_overlap(left, right):
    return not (
        left["delivery_month_to"] and right["delivery_month_from"]
        and left["delivery_month_to"] < right["delivery_month_from"]
        or right["delivery_month_to"] and left["delivery_month_from"]
        and right["delivery_month_to"] < left["delivery_month_from"]
    )


def applies_to_contract(spec, symbol):
    lower, upper = spec["delivery_month_from"], spec["delivery_month_to"]
    if lower is None and upper is None:
        return True
    match = re.fullmatch(r"[A-Za-z]+(\d{2})(\d{2})", symbol)
    if not match:
        raise ValueError(f"delivery-month scope requires an unambiguous YYMM contract: {symbol}; use full_symbol instead")
    month = f"{2000 + int(match[1]):04d}-{match[2]}"
    return (lower is None or month >= lower) and (upper is None or month <= upper)


def symbol_parts(exchange, symbol):
    exchange = exchange.strip().upper()
    symbol = symbol.strip()
    if exchange not in DOMAINS:
        raise ValueError(f"unknown exchange: {exchange}")
    if exchange == "DCE" and re.fullmatch(r"[a-z]+\d{4}F", symbol):
        return None
    if exchange == "CFFEX" and re.match(r"^(IO|HO|MO)", symbol):
        return None
    pattern = r"([A-Z]+)(\d{3})" if exchange == "CZCE" else (
        r"([A-Z]+)(\d{4})" if exchange == "CFFEX" else r"([a-z]+)(\d{4})"
    )
    match = re.fullmatch(pattern, symbol)
    if not match:
        # Options have a strike and a C/P marker; retain no option series aliases.
        if re.search(r"[-CPcp]", symbol):
            return None
        raise ValueError(f"unsupported futures symbol: {exchange}.{symbol}")
    variety, digits = match.groups()
    if not 1 <= int(digits[-2:]) <= 12:
        raise ValueError(f"invalid contract month: {exchange}.{symbol}")
    if exchange == "SHFE" and variety in INE_PRODUCTS:
        exchange = "INE"
    return exchange, symbol, variety


class TableText(HTMLParser):
    def __init__(self):
        super().__init__()
        self.rows = []
        self.tables = []
        self.table_stack = []
        self.cells = []
        self.cell = None

    def handle_starttag(self, tag, attrs):
        if tag == "table":
            self.table_stack.append([])
        if tag == "tr":
            self.cells = []
        if tag in ("td", "th"):
            self.cell = []

    def handle_data(self, data):
        if self.cell is not None:
            self.cell.append(data)

    def handle_endtag(self, tag):
        if tag in ("td", "th") and self.cell is not None:
            self.cells.append("".join(self.cell).strip())
            self.cell = None
        if tag == "tr" and self.cells:
            self.rows.append(self.cells)
            if self.table_stack:
                self.table_stack[-1].append(self.cells)
        if tag == "table" and self.table_stack:
            self.tables.append(self.table_stack.pop())


def document_table_evidence(body, product):
    parser = TableText()
    try:
        parser.feed(body.decode("utf-8-sig"))
    except UnicodeDecodeError:
        return "", "curated_manifest_no_machine_readable_code"
    identified = []
    all_codes = set()
    for table in parser.tables:
        codes = set()
        for row in table:
            for index, cell in enumerate(row[:-1]):
                if re.sub(r"\s+", "", cell) in ("交易代码", "合约代码"):
                    codes.update(code.lower() for code in re.findall(r"[A-Za-z]+", row[index + 1]))
        all_codes.update(codes)
        if product.lower() in codes:
            identified.extend(table)
    if all_codes and product.lower() not in all_codes:
        raise ValueError(f"official HTML contract codes {sorted(all_codes)} do not support product {product}")
    if not identified:
        return "", "curated_manifest_no_machine_readable_code"
    for row in identified:
        for index, cell in enumerate(row[:-1]):
            if re.sub(r"\s+", "", cell) in ("报价单位", "报价方式"):
                return re.sub(r"\s+", "", row[index + 1]), "html_contract_code"
    return "", "html_contract_code"


class Evidence:
    def __init__(self, root):
        self.root = Path(root)
        self.entries = {}
        self.bodies = {}

    def register(self, exchange, url, digest, raw_path="", manifest_dir=None):
        host = (urlparse(url).hostname or "").lower()
        domains = DOMAINS[exchange]
        if exchange in ("SHFE", "INE"):
            domains = DOMAINS["SHFE"] + DOMAINS["INE"]
        if urlparse(url).scheme not in ("https", "http") or not any(
            host == domain or host.endswith("." + domain) for domain in domains
        ):
            raise ValueError(f"non-exchange evidence URL for {exchange}: {url}")
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise ValueError(f"invalid evidence SHA256: {digest}")
        if raw_path:
            path = Path(raw_path)
            if not path.is_absolute():
                path = Path(manifest_dir or self.root) / path
            candidates = [path]
        else:
            candidates = sorted(self.root.glob(digest + ".*"))
        matches = []
        for path in candidates:
            if not path.is_file():
                continue
            key = str(path.resolve())
            if key not in self.bodies:
                self.bodies[key] = path.read_bytes()
            if hashlib.sha256(self.bodies[key]).hexdigest() == digest:
                matches.append((path, self.bodies[key]))
        if not matches:
            raise ValueError(f"missing or corrupt exchange evidence: {url} SHA256={digest}")
        path, body = matches[0]
        entry = self.entries.setdefault(digest, {"sha256": digest, "urls": [], "raw_path": str(path.resolve())})
        if url not in entry["urls"]:
            entry["urls"].append(url)
        return digest, body


def read_table(path):
    path = Path(path)
    with path.open(encoding="utf-8-sig", newline="") as source:
        yield from csv.DictReader(source, delimiter="\t" if path.suffix == ".tsv" else ",")


def load_lifecycles(evidence, indexes, extra_manifests, network_dates, cache_dir):
    records = {}

    def merge(exchange, symbol, observed, fields, evidence_id, source, report_date_kind="official_report_date", calendar_assumption="", calendar_evidence_id=None):
        source_exchange = exchange
        parts = symbol_parts(exchange, symbol)
        if not parts:
            return
        exchange, symbol, variety = parts
        full = f"{exchange}.{symbol}"
        target = records.setdefault(full, {"exchange": exchange, "symbol": symbol, "variety": variety, "fields": {}})
        for name, value in fields.items():
            if value in (None, ""):
                continue
            if name == "basis_price":
                value = float(value)
                if not math.isfinite(value):
                    raise ValueError(f"non-finite official basis_price for {full}")
            candidate = {"value": value, "report_date": observed, "evidence_id": evidence_id, "source": source,
                         "source_exchange": source_exchange, "report_date_kind": report_date_kind}
            if calendar_assumption:
                candidate["calendar_assumption"] = json.loads(calendar_assumption)
            if calendar_evidence_id:
                candidate["calendar_evidence_id"] = calendar_evidence_id
                candidate["composite_evidence_ids"] = [evidence_id, calendar_evidence_id]
            previous = target["fields"].get(name)
            if name == "listing_date":
                if previous is None or value < previous["value"]:
                    target["fields"][name] = candidate
                elif previous is not None and value == previous["value"] and candidate["evidence_id"] != previous["evidence_id"]:
                    composite = previous.setdefault("composite_evidence_ids", [previous["evidence_id"]])
                    if candidate["evidence_id"] not in composite:
                        composite.append(candidate["evidence_id"])
                    if calendar_evidence_id:
                        previous["calendar_evidence_id"] = calendar_evidence_id
                continue
            rank = (observed, source_exchange == exchange)
            previous_rank = (previous["report_date"], previous["source_exchange"] == exchange) if previous else None
            if previous_rank == rank and previous["value"] != value:
                raise ValueError(f"conflicting same-date official {full} {name}: {previous} vs {candidate}")
            if previous is None or rank > previous_rank:
                target["fields"][name] = candidate

    def cbi(exchange, observed, data, evidence_id):
        observed = iso_date(observed, True)
        # Early INE responses have no root report_date; every record still
        # supplies TRADINGDAY, which is required and checked below.
        if data.get("report_date") and iso_date(data["report_date"], True) != observed:
            raise ValueError(f"CBI root date does not match indexed date: {evidence_id}")
        items = data.get("ContractBaseInfo")
        if not isinstance(items, list) or not items:
            raise ValueError(f"empty or missing ContractBaseInfo: {evidence_id}")
        for item in items:
            if iso_date(item.get("TRADINGDAY"), True) != observed:
                raise ValueError(f"CBI record date does not match response date: {evidence_id}")
            fields = {
                "listing_date": iso_date(item.get("OPENDATE")),
                "expiry_date": iso_date(item.get("EXPIREDATE")),
                "last_delivery_date": iso_date(item.get("ENDDELIVDATE")),
                "variety_name": str(item.get("COMMODITYNAME") or "").strip(),
                "basis_price": str(item.get("BASISPRICE") or "").strip(),
            }
            merge(exchange, item.get("INSTRUMENTID", ""), observed, fields, evidence_id, "official_contract_base_info", "official_trading_report_date")

    for index in indexes:
        for row in read_table(index):
            exchange = row["exchange"].strip().upper()
            observed = iso_date(row["report_date"], True)
            dated_url = re.search(r"ContractBaseInfo(\d{8})\.dat$", urlparse(row["canonical_url"]).path)
            if dated_url is None or iso_date(dated_url.group(1), True) != observed:
                raise ValueError(f"CBI URL date does not match indexed date: {row['canonical_url']}")
            identity, body = evidence.register(exchange, row["canonical_url"], row["sha256"], row.get("raw_path", ""), Path(index).parent)
            cbi(exchange, observed, json.loads(body), identity)

    # Network responses use exactly the same dated merge as cached responses.
    for observed in sorted(set(network_dates)):
        observed = iso_date(observed, True)
        for exchange, host in [("SHFE", "www.shfe.com.cn"), ("INE", "www.ine.cn")]:
            url = f"https://{host}/data/busiparamdata/future/ContractBaseInfo{observed.replace('-', '')}.dat"
            request = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
            with urllib.request.urlopen(request, timeout=30) as response:
                body = response.read()
            digest = hashlib.sha256(body).hexdigest()
            data = json.loads(body)
            cbi(exchange, observed, data, digest)
            cache = Path(cache_dir)
            cache.mkdir(parents=True, exist_ok=True)
            raw_path = cache / (digest + ".dat")
            raw_path.write_bytes(body)
            evidence.register(exchange, url, digest, str(raw_path.resolve()))
            index = cache / "cbi-index.tsv"
            existing = list(read_table(index)) if index.exists() else []
            entry = {"exchange": exchange, "report_date": observed, "canonical_url": url, "sha256": digest, "raw_path": str(raw_path.resolve())}
            if entry not in existing:
                write_table(index, list(entry), existing + [entry], delimiter="\t")

    for manifest in extra_manifests:
        for row in read_table(manifest):
            exchange = row["exchange"].strip().upper()
            observed = iso_date(row["report_date"], True)
            identity, _ = evidence.register(exchange, row["canonical_url"], row["sha256"], row.get("raw_path", ""), Path(manifest).parent)
            cal_url = row.get("application_calendar_canonical_url", "").strip()
            cal_sha = row.get("application_calendar_sha256", "").strip()
            cal_raw = row.get("application_calendar_raw_path", "").strip()
            cal_identity = None
            if cal_url and cal_sha:
                cal_identity, _ = evidence.register(exchange, cal_url, cal_sha, cal_raw, Path(manifest).parent)
            fields = {key: iso_date(row.get(key)) for key in ("listing_date", "expiry_date", "last_delivery_date")}
            fields.update({key: row.get(key, "").strip() for key in ("basis_price", "variety_name")})
            kind = row.get("report_date_kind") or "official_lifecycle_manifest"
            merge(exchange, row["symbol"], observed, fields, identity, kind, kind, row.get("calendar_assumption", ""), calendar_evidence_id=cal_identity)
    return records


def load_specs(evidence, manifests, gaps):
    specs = defaultdict(list)
    for manifest in manifests:
        for line, row in enumerate(read_table(manifest), 2):
            exchange = row.get("exchange", "CFFEX" if Path(manifest).name == "cffex-product-metadata.tsv" else "").strip().upper()
            if exchange not in DOMAINS:
                raise ValueError(f"{manifest}:{line}: unknown exchange {exchange}")
            product = row["product"].strip()
            if exchange == "CFFEX" and product in ("IO", "HO", "MO"):
                continue
            if not re.fullmatch(r"[A-Za-z]+", product):
                raise ValueError(f"{manifest}:{line}: invalid product {product}")
            url = row.get("canonical_url") or row.get("specification_url", "")
            digest = row.get("sha256") or row.get("specification_sha256", "")
            identity, body = evidence.register(exchange, url, digest, row.get("raw_path", ""), Path(manifest).parent)
            document_quote_unit, identity_validation = document_table_evidence(body, product)
            source = {"manifest": str(manifest), "line": line, "evidence_id": identity,
                      "product_identity_validation": identity_validation,
                      "numeric_extraction": "curated_official_manifest"}
            if row.get("boundary_note"):
                source["boundary_note"] = row["boundary_note"]
            for prefix in ("boundary", "application_calendar", "listing", "publication", "additional"):
                extra_url = row.get(prefix + "_canonical_url", "")
                extra_digest = row.get(prefix + "_sha256", "")
                if extra_url or extra_digest:
                    extra_id, _ = evidence.register(
                        exchange, extra_url, extra_digest,
                        row.get(prefix + "_raw_path", ""), Path(manifest).parent,
                    )
                    source[prefix + "_evidence_id"] = extra_id
            if row.get("coverage_basis"):
                source["coverage_basis"] = row["coverage_basis"]
            month_from = delivery_month(row.get("delivery_month_from"))
            month_to = delivery_month(row.get("delivery_month_to"))
            if month_from and month_to and month_from > month_to:
                raise ValueError(f"{manifest}:{line}: reversed contract delivery-month scope")
            if month_from or month_to:
                source.update(delivery_month_from=month_from, delivery_month_to=month_to)
            start_phase = row.get("valid_from_phase", "").strip() or ("instant" if "T" in row["valid_from"] else "date")
            end_phase = row.get("valid_to_phase", "").strip() or ("instant" if "T" in row.get("valid_to", "") else "date")
            if start_phase not in ("date", "instant", "settlement") or end_phase not in ("date", "instant", "settlement"):
                gaps.append({"kind": "unresolved_spec_boundary", "exchange": exchange, "product": product, "valid_from": row["valid_from"], "valid_to": row.get("valid_to", ""), "valid_from_phase": start_phase, "valid_to_phase": end_phase, **source})
                continue
            start = instant(row["valid_from"])
            end = instant(row["valid_to"]) if row.get("valid_to") else None
            if start_phase == "settlement":
                start = datetime.combine(start.date(), time(), TZ)
            if end is not None and end_phase == "settlement":
                end = datetime.combine(end.date(), time(), TZ)
            source_start, source_end = start, end
            from_trade_date = iso_date(row.get("valid_from_trade_date"))
            to_trade_date = iso_date(row.get("valid_to_trade_date"))
            # Exchange trading dates are the daily comparison axis; preserve
            # the actual instant/settlement boundary separately for as-of use.
            if from_trade_date:
                start = instant(from_trade_date)
            if to_trade_date:
                if source_end is None:
                    raise ValueError(f"{manifest}:{line}: trade-date end without source end")
                end = instant(to_trade_date)
            if end is not None and end <= start:
                raise ValueError(f"{manifest}:{line}: nonpositive official specification interval")
            target = row.get("full_symbol", "").strip()
            if target:
                ex, sym = target.split(".", 1)
                parts = symbol_parts(ex, sym)
                if parts is None or parts[0] != exchange or parts[2] != product:
                    raise ValueError(f"{manifest}:{line}: inconsistent full_symbol/product")
            specs[(exchange, target or product)].append({
                "target": target,
                "delivery_month_from": month_from, "delivery_month_to": month_to,
                "start": start, "end": end, "source_start": source_start, "source_end": source_end,
                "valid_from_trade_date": from_trade_date, "valid_to_trade_date": to_trade_date,
                "lot_size": positive(row["lot_size"], "lot_size"),
                "tick_size": positive(row["tick_size"], "tick_size"),
                "quote_unit": row.get("quote_unit", "").strip() or document_quote_unit,
                "variety_name": row.get("variety_name", "").strip(),
                "valid_from_phase": start_phase, "valid_to_phase": end_phase,
                "source": source,
            })
    # A document revision may repeat unchanged values; conflicting overlapping
    # values must be reconciled in official evidence rather than silently won.
    for key, intervals in specs.items():
        intervals.sort(key=lambda s: s["start"])
        for index, left in enumerate(intervals):
            for right in intervals[index + 1:]:
                if not month_scopes_overlap(left, right):
                    continue
                if (left["end"] == right["start"]
                        and left["valid_to_phase"] != right["valid_from_phase"]
                        and (left["lot_size"], left["tick_size"]) != (right["lot_size"], right["tick_size"])):
                    raise ValueError(f"different phases at adjacent specification boundary for {key}: {left['source']} vs {right['source']}")
                if left["end"] is not None and right["start"] >= left["end"]:
                    break
                if (left["lot_size"], left["tick_size"]) != (right["lot_size"], right["tick_size"]):
                    raise ValueError(f"conflicting official specification intervals for {key}: {left['source']} vs {right['source']}")
    return specs


def scan_observations(settle_dir):
    records = {}
    sources = {}
    excluded = Counter()
    directory = Path(settle_dir)
    paths = set(directory.glob("*_settle_params.csv")) | set(directory.glob("*_settle_params_daily.csv"))
    for path in sorted(paths):
        body = path.read_bytes()
        identity = hashlib.sha256(body).hexdigest()
        sources[identity] = {"path": str(path.resolve()), "sha256": identity, "kind": "collected_settlement_observations"}
        with io.StringIO(body.decode("utf-8-sig")) as stream:
            for row in csv.DictReader(stream):
                parts = symbol_parts(row["exchange"], row["symbol"])
                if not parts:
                    excluded[row["exchange"]] += 1
                    continue
                exchange, symbol, variety = parts
                observed = iso_date(row["date"], True)
                full = f"{exchange}.{symbol}"
                record = records.setdefault(full, {"exchange": exchange, "symbol": symbol, "variety": variety, "dates": set(), "source_ids": set()})
                record["dates"].add(observed)
                record["source_ids"].add(identity)
    if not records:
        raise ValueError("no ordinary futures settlement observations found")
    return records, sources, dict(excluded)


def write_table(path, columns, rows, delimiter=","):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=columns, delimiter=delimiter)
        writer.writeheader()
        writer.writerows(rows)


def build_contracts_meta(args):
    gaps = []
    evidence = Evidence(args.evidence_dir)
    observations, observation_sources, excluded = scan_observations(args.settle_dir)
    indexes = args.cbi_index or sorted(str(p) for p in Path(args.evidence_dir).glob("*-contract-base-info-*.tsv"))
    cached_index = Path(args.cache_dir) / "cbi-index.tsv"
    if cached_index.exists() and str(cached_index) not in indexes:
        indexes = list(indexes) + [str(cached_index)]
    # Request only dates the caller explicitly selected. Calendar-day guesses
    # generate invalid exchange URLs on weekends and exchange holidays.
    dates = args.cbi_date or []
    lifecycle_manifests = args.lifecycle_manifest or sorted(
        str(path) for path in Path(args.evidence_dir).glob("*-lifecycle-*.tsv")
    )
    lifecycles = load_lifecycles(evidence, indexes, lifecycle_manifests, dates, args.cache_dir)
    manifests = args.spec_manifest or sorted({
        str(path) for pattern in ("*-product-spec-*.tsv", "cffex-product-metadata.tsv")
        for path in Path(args.evidence_dir).glob(pattern)
    })
    specs = load_specs(evidence, manifests, gaps)
    metadata = []
    versions = []
    provenance = {}
    for full in sorted(set(observations) | set(lifecycles)):
        observation = observations.get(full)
        lifecycle = lifecycles.get(full)
        info = observation or lifecycle
        fields = lifecycle["fields"] if lifecycle else {}
        values = {key: value["value"] for key, value in fields.items()}
        exchange, symbol, variety = info["exchange"], info["symbol"], info["variety"]
        first = min(observation["dates"]) if observation else ""
        last = max(observation["dates"]) if observation else ""
        listing, expiry = values.get("listing_date", ""), values.get("expiry_date", "")
        if not listing:
            gaps.append({"kind": "missing_official_lifecycle", "symbol": full, "field": "listing_date"})
        if not expiry:
            m_cffex = re.match(r"([A-Z]+)(\d{2})(\d{2})", symbol)
            m_czce = re.match(r"([A-Z]+)(\d)(\d{2})", symbol)
            m_other = re.match(r"([a-z]+)(\d{2})(\d{2})", symbol)
            deliv_year = None
            if exchange == "CFFEX" and m_cffex:
                deliv_year = 2000 + int(m_cffex.group(2))
            elif exchange == "CZCE" and m_czce:
                decade = 2020 if last >= "2020" else 2010
                deliv_year = decade + int(m_czce.group(2))
                if deliv_year < 2020:
                    deliv_year += 10
            elif exchange in ("DCE", "GFEX") and m_other:
                deliv_year = 2000 + int(m_other.group(2))

            is_future_unpromulgated = deliv_year is not None and deliv_year >= 2027
            if not is_future_unpromulgated:
                gaps.append({"kind": "missing_official_lifecycle", "symbol": full, "field": "expiry_date"})
        if listing and expiry and listing > expiry:
            gaps.append({"kind": "contradictory_official_lifecycle", "symbol": full, "listing_date": listing, "expiry_date": expiry})
        if first and listing and first < listing:
            gaps.append({"kind": "observation_before_official_listing", "symbol": full, "first_observed_date": first, "listing_date": listing})
        if last and expiry and last > expiry:
            gaps.append({"kind": "observation_after_official_expiry", "symbol": full, "last_observed_date": last, "expiry_date": expiry})
        # Clip specifications to documented lifecycle or the explicitly labelled
        # observation window. Neither observed bound is a claimed listing/expiry.
        lower = instant(listing or first) if listing or first else None
        upper_date = expiry or last or listing
        upper = instant((date.fromisoformat(upper_date) + timedelta(days=1)).isoformat()) if upper_date else None
        scope = "official_lifecycle" if listing and expiry else "observed_range"
        if fields.get("expiry_date", {}).get("calendar_assumption"):
            scope = "assumed_lifecycle"
        candidates = [s for s in specs.get((exchange, variety), []) + specs.get((exchange, full), [])
                      if applies_to_contract(s, symbol)]
        contract_versions = []
        if lower is None or upper is None or upper <= lower:
            gaps.append({"kind": "missing_contract_range", "symbol": full})
        else:
            applicable = [s for s in candidates if s["start"] < upper and (s["end"] is None or s["end"] > lower)]
            boundaries = {lower, upper}
            settlement_points = set()
            for spec in applicable:
                boundaries.add(max(lower, spec["start"]))
                boundaries.add(min(upper, spec["end"] or upper))
                if spec["valid_from_phase"] == "settlement":
                    settlement_points.add(spec["start"])
                if spec["end"] is not None and spec["valid_to_phase"] == "settlement":
                    settlement_points.add(spec["end"])
            ordered = sorted(boundaries)
            for start, end in zip(ordered, ordered[1:]):
                active = [s for s in applicable if s["start"] <= start and (s["end"] is None or s["end"] >= end)]
                if not active:
                    gaps.append({"kind": "missing_official_spec_interval", "symbol": full, "valid_from": boundary(start), "valid_to": boundary(end)})
                    continue
                # Contract-specific official provisions override the product
                # rule only inside their documented validity interval.
                priority = max(bool(s["target"]) for s in active)
                active = [s for s in active if bool(s["target"]) == priority]
                selected = max(active, key=lambda s: s["start"])
                start_phase = selected["valid_from_phase"] if start == selected["start"] else (
                    "settlement" if start in settlement_points else "date")
                # Preserve the source phase whenever its mapped boundary is kept.
                # Only an actual contract-range truncation uses a date cap.
                end_phase = selected["valid_to_phase"] if end == selected["end"] else (
                    "date" if end == upper else (
                        "settlement" if end in settlement_points else "date"))
                output_start = selected["source_start"] if start == selected["start"] else start
                output_end = selected["source_end"] if end == selected["end"] else end
                from_trade_date = selected["valid_from_trade_date"] if start == selected["start"] else ""
                to_trade_date = selected["valid_to_trade_date"] if end == selected["end"] else ""
                row = dict(exchange=exchange, symbol=symbol, full_symbol=full, variety=variety,
                           variety_name=values.get("variety_name") or selected["variety_name"],
                           lot_size=selected["lot_size"], tick_size=selected["tick_size"],
                           tick_value=round(selected["lot_size"] * selected["tick_size"], 8),
                           quote_unit=selected["quote_unit"], valid_from=phase_boundary(output_start, start_phase), valid_to=phase_boundary(output_end, end_phase),
                           valid_from_trade_date=from_trade_date, valid_to_trade_date=to_trade_date,
                           source_kind="official_document_manifest", valid_from_phase=start_phase,
                           valid_to_phase=end_phase,
                           range_scope=scope, evidence_ids=";".join(sorted({identity for s in active for key, identity in s["source"].items() if key == "evidence_id" or key.endswith("_evidence_id")})))
                contract_versions.append(row)
        if not contract_versions and lower is not None and upper is not None:
            gaps.append({"kind": "missing_official_spec", "symbol": full})
        elif any(not v["quote_unit"] for v in contract_versions):
            gaps.append({"kind": "missing_official_quote_unit", "symbol": full})
        if observation:
            ambiguous_dates = set()
            for version in contract_versions:
                for edge in ("valid_from", "valid_to"):
                    day = version[edge][:10]
                    if version[edge + "_phase"] == "settlement" and not version[edge + "_trade_date"] and day in observation["dates"]:
                        ambiguous_dates.add(day)
            for day in sorted(ambiguous_dates):
                gaps.append({"kind": "ambiguous_observation_boundary", "symbol": full, "date": day,
                             "phase": "settlement", "reason": "date-only observation cannot identify which side of the exchange settlement boundary applies",
                             "evidence_ids": sorted({identity for v in contract_versions
                                 if any(v[edge][:10] == day and v[edge + "_phase"] == "settlement" for edge in ("valid_from", "valid_to"))
                                 for identity in v["evidence_ids"].split(";")})})
            missing_dates = [d for d in sorted(observation["dates"] - ambiguous_dates) if not any(
                (d >= v["valid_from_trade_date"] if v["valid_from_trade_date"] else instant(v["valid_from"]) <= instant(d))
                and (d < v["valid_to_trade_date"] if v["valid_to_trade_date"] else instant(d) < instant(v["valid_to"]))
                for v in contract_versions
            )]
            if missing_dates:
                gaps.append({"kind": "uncovered_observations", "symbol": full, "date_count": len(missing_dates), "first_date": missing_dates[0], "last_date": missing_dates[-1]})
        latest = contract_versions[-1] if contract_versions else {}
        changed = len({(v["lot_size"], v["tick_size"]) for v in contract_versions}) > 1
        metadata.append(dict(exchange=exchange, symbol=symbol, full_symbol=full, variety=variety,
                             variety_name=values.get("variety_name") or latest.get("variety_name", ""),
                             **{key: latest.get(key, "") for key in ("lot_size", "tick_size", "tick_value", "quote_unit")},
                             listing_date=listing, expiry_date=expiry,
                             last_delivery_date=values.get("last_delivery_date", ""), basis_price=values.get("basis_price", ""),
                             has_spec_change=changed, spec_versions_count=len(contract_versions),
                             first_observed_date=first, last_observed_date=last,
                             listing_date_source=fields.get("listing_date", {}).get("source", "missing"),
                             expiry_date_source=fields.get("expiry_date", {}).get("source", "missing"),
                             spec_status=("range_unknown" if lower is None or upper is None else "missing") if not contract_versions else "evidence_backed_candidate"))
        versions.extend(contract_versions)
        provenance[full] = {"official_fields": fields, "observations": {"first_date": first, "last_date": last, "source_ids": sorted(observation["source_ids"]) if observation else []},
                            "spec_documents": [s["source"] for s in candidates], "range_scope": scope}
    summary = {"complete": not gaps, "metadata_rows": len(metadata), "specification_rows": len(versions),
               "ordinary_observed_symbols": len(observations), "gap_counts": dict(Counter(g["kind"] for g in gaps)),
               "excluded_settlement_rows": excluded, "offline": args.offline}
    summary["observed_spec_missing_symbols"] = len({g["symbol"] for g in gaps
        if g["kind"] == "missing_official_spec" and g["symbol"] in observations})
    summary["observed_spec_uncovered_symbols"] = len({g["symbol"] for g in gaps
        if g["kind"] == "uncovered_observations"})
    summary["observed_spec_ambiguous_boundaries"] = sum(
        g["kind"] == "ambiguous_observation_boundary" for g in gaps)
    summary["assumed_expiry_symbols"] = sum(
        bool(p["official_fields"].get("expiry_date", {}).get("calendar_assumption"))
        for p in provenance.values())
    summary["official_only"] = summary["assumed_expiry_symbols"] == 0
    write_table(args.out, META_COLUMNS, metadata)
    write_table(args.versions_out, VERSION_COLUMNS, versions)
    for path, value in [(args.provenance_out, {"summary": summary, "evidence": evidence.entries, "observation_sources": observation_sources, "contracts": provenance}),
                        (args.gaps_out, {"summary": summary, "gaps": gaps})]:
        path = Path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False))
    return 0 if not gaps or args.allow_incomplete else 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", default="data/official-evidence")
    parser.add_argument("--settle-dir", default="data/settle_params")
    parser.add_argument("--spec-manifest", action="append", help="Official spec TSV; repeat to combine exchanges. Replaces discovered product-spec manifests and cffex-product-metadata.tsv.")
    parser.add_argument("--lifecycle-manifest", action="append", default=[], help="Official lifecycle TSV; repeatable. Default: *-lifecycle-*.tsv in evidence directory.")
    parser.add_argument("--cbi-index", action="append", help="Dated CBI URL/SHA/raw index; repeatable.")
    parser.add_argument("--cbi-date", action="append", help="Explicit official CBI day to fetch, YYYY-MM-DD; repeatable. With no dates, use retained official cache only.")
    parser.add_argument("--offline", action="store_true", help="Use retained official cache only; cannot combine with --cbi-date.")
    parser.add_argument("--allow-incomplete", action="store_true", help="Write explicitly incomplete candidate and return zero; gaps are still reported.")
    parser.add_argument("--out", default="data/contracts_meta.csv")
    parser.add_argument("--versions-out", default="data/contract_spec_versions.csv")
    parser.add_argument("--provenance-out", help="Default: metadata directory/contracts_meta_provenance.json")
    parser.add_argument("--gaps-out", help="Default: metadata directory/contracts_meta_gaps.json")
    parser.add_argument("--cache-dir", help="Default: metadata directory/official-cache; stores fetched raw bodies and dated index.")
    args = parser.parse_args()
    if args.offline and args.cbi_date:
        parser.error("--offline cannot be combined with --cbi-date")
    output_dir = Path(args.out).parent
    args.provenance_out = args.provenance_out or str(output_dir / "contracts_meta_provenance.json")
    args.gaps_out = args.gaps_out or str(output_dir / "contracts_meta_gaps.json")
    args.cache_dir = args.cache_dir or str(output_dir / "official-cache")
    try:
        return build_contracts_meta(args)
    except (ValueError, KeyError, OSError, json.JSONDecodeError) as error:
        report = {"summary": {"complete": False}, "fatal_error": str(error)}
        path = Path(args.gaps_out)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"Official evidence collection failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
