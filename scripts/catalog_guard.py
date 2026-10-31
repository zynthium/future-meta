"""Validate current official contract catalogs against approved document links."""

import hashlib
import json
import re
from html.parser import HTMLParser
from urllib.parse import urljoin, urlparse, urldefrag, unquote


def decode(body):
    try:
        return body.decode("utf-8-sig")
    except UnicodeDecodeError:
        return body.decode("gb18030", errors="replace")


class Links(HTMLParser):
    def __init__(self):
        super().__init__()
        self.href = None
        self.label = ""
        self.links = []

    def handle_starttag(self, tag, attributes):
        if tag == "a":
            self.href = dict(attributes).get("href")
            self.label = ""

    def handle_data(self, data):
        if self.href:
            self.label += data

    def handle_endtag(self, tag):
        if tag == "a" and self.href:
            self.links.append((self.href, "".join(self.label.split())))
            self.href = None


def fetch_catalog(url):
    from incremental_inputs import fetch_source

    return fetch_source({"url": url}, {})


def canonical(url):
    return urldefrag(url)[0].replace("http://", "https://", 1)


def table_signatures(body, product=None):
    import unicodedata

    class Tables(HTMLParser):
        def __init__(self):
            super().__init__()
            self.stack = []
            self.tables = []

        def handle_starttag(self, tag, attributes):
            if tag == "table":
                self.stack.append([])

        def handle_data(self, data):
            for table in self.stack:
                table.append(data)

        def handle_endtag(self, tag):
            if tag == "table" and self.stack:
                self.tables.append("".join(self.stack.pop()))

    parser = Tables()
    parser.feed(decode(body))
    texts = [
        "".join(unicodedata.normalize("NFKC", text).split()) for text in parser.tables
    ]
    texts = [
        text
        for text in texts
        if "交易单位" in text
        and "最小变动价位" in text
        and (
            product is None
            or re.search(r"(?<![A-Za-z])" + re.escape(product) + r"(?![A-Za-z])", text)
        )
    ]
    # Nested layout tables may contain the real table plus unrelated navigation.
    texts = [t for t in texts if not any(other != t and other in t for other in texts)]
    return sorted({hashlib.sha256(text.encode()).hexdigest() for text in texts})


def treasury_profile(body, monitor):
    from collect_contract_specs import TableText

    parser = TableText()
    parser.feed(decode(body))
    matches = []
    for table in parser.tables:
        fields = {
            "".join(row[i].split()): "".join(row[i + 1].split())
            for row in table
            for i in range(0, len(row) - 1, 2)
        }
        if fields.get("交易代码", fields.get("合约代码")) == monitor["product"]:
            matches.append(fields)
    if len(matches) != 1:
        raise ValueError("Missing/ambiguous current treasury contract table")
    fields = matches[0]
    nominal = re.fullmatch(
        r"面值为?([0-9]+(?:\.[0-9]+)?)万元人民币(?:的2年期国债|[、，,]票面利率为3%的名义中短期国债)",
        fields.get("合约标的", ""),
    )
    tick = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)(?:元)?", fields.get("最小变动价位", ""))
    if not nominal or not tick or fields.get("报价方式") != "百元净价报价":
        raise ValueError("Unrecognized treasury quotation units")
    expiry = fields.get("最后交易日", "")
    months = fields.get("合约月份", "")
    if not re.fullmatch(
        r"合约到期月份的第二个(?:星期|周)五(?:[，,](?:遇|如遇)(?:国家)?法定(?:节假|假)日顺延(?:至下一交易日)?)?",
        expiry,
    ):
        raise ValueError("Official treasury lifecycle rule changed")
    if not re.fullmatch(
        r"最近的?[三3]个季月(?:[（(]3月、6月、9月、12月中的最近三个月循环[）)])?",
        months,
    ):
        raise ValueError("Official treasury listing cycle changed")
    profile = {
        "product": monitor["product"],
        "lot_size": float(nominal[1]) * 100.0,
        "tick_size": float(tick[1]),
        "expiry_rule": "second_friday",
        "quarter_count": 3,
    }
    if (
        profile["lot_size"] != monitor["lot_size"]
        or profile["tick_size"] != monitor["tick_size"]
    ):
        raise ValueError("Official current treasury specification changed")
    return profile


def check(rules, verified_sources=None, previous=None):
    monitors = rules.get("catalogs", [])
    if not monitors or set(m["exchange"] for m in monitors) != {
        "CFFEX",
        "CZCE",
        "DCE",
        "GFEX",
        "INE",
        "SHFE",
    }:
        raise ValueError("Missing authoritative current contract catalogs")
    if any(not m.get("product") or not m.get("approved_documents") for m in monitors):
        raise ValueError("Current catalogs must be bound to approved product rules")
    return check_catalogs(rules, verified_sources, previous)


def check_catalogs(rules, verified_sources=None, previous=None):
    monitors = rules["catalogs"]
    known = {canonical(source["url"]): source["sha256"] for source in rules["sources"]}
    catalog_urls = {canonical(m["url"]) for m in monitors}
    verified_sources = verified_sources or {}
    versions = {}
    for monitor in monitors:
        url = monitor["url"]
        key = canonical(url)
        mode = monitor.get("mode", "catalog")
        if mode == "document" and any(
            canonical(source) == key and status.get("sha256") == known.get(key)
            for source, status in verified_sources.items()
        ):
            # This is an explicitly configured CURRENT document entry, verified
            # in this run. Immutable historical announcements are not such roots.
            versions[url] = {"documents": [key], "sha256": known[key]}
            continue
        response = fetch_catalog(url)
        if response.status_code != 200:
            raise ValueError(f"Current rule catalog unavailable: {url}")
        body = response.content
        digest = hashlib.sha256(body).hexdigest()
        if mode == "document":
            if digest != known.get(key):
                raise ValueError(f"Official current rule document changed: {url}")
            versions[url] = {"documents": [key], "sha256": digest}
            continue
        if mode == "treasury":
            versions[url] = {"profile": treasury_profile(body, monitor)}
            continue
        tables = table_signatures(body, monitor.get("product"))
        if tables or mode == "table":
            if not tables or tables != monitor.get("table_sha256"):
                raise ValueError(
                    f"Unapproved/changed inline current contract table: {url}"
                )
            versions[url] = {"documents": [key], "table_sha256": tables}
            continue
        parser = Links()
        parser.feed(decode(body))
        selected = set()
        for href, label in parser.links:
            target = canonical(urljoin(url, href))
            path = urlparse(target).path.lower()
            if (
                target == key
                or target in catalog_urls
                or urlparse(target).scheme not in ("http", "https")
            ):
                continue
            revised = any(
                word in label
                for word in (
                    "修订",
                    "修正",
                    "修改",
                    "变更",
                    "发布",
                    "交易单位",
                    "最小变动",
                )
            )
            futures = "期货合约" in label or "contract" in label.lower()
            excluded = (
                any(word in label + unquote(path) for word in ("期权", "细则"))
                and not futures
            ) or any(
                word in label
                for word in (
                    "旧版",
                    "废止",
                    "历史合约",
                    "指南",
                    "章程",
                    "办法",
                    "停板",
                    "最值",
                    "分布",
                    "热力",
                )
            )
            if excluded:
                continue
            attachment = (
                bool(re.search(r"\.(pdf|docx?|wps)$", path))
                or "clientDf.do" in urlparse(target).path
            )
            relevant = ("合约" in label or "contract" in label.lower()) and (
                not label.startswith("关于") or revised
            )
            if "standard_" in path and not path.startswith(
                urlparse(key).path.rsplit("/", 1)[0]
            ):
                continue
            if attachment or relevant:
                selected.add(target)
        if not selected:
            raise ValueError(f"Official catalog has no current contract links: {url}")
        allowed = {canonical(url) for url in monitor.get("approved_documents", known)}
        unapproved = selected - allowed
        if unapproved and selected.issubset(known):
            raise ValueError(
                f"Official catalog selection differs from approved product: {url}"
            )
        if unapproved:
            # Newly selected HTML is a terminal rule/article, not an implicit
            # catalog authorized by whichever obsolete attachment it links to.
            raise ValueError(
                f"Official current catalog references unapproved document: {sorted(unapproved)[0]}"
            )
        links = sorted(selected)
        versions[url] = {
            "documents": links,
            "sha256": hashlib.sha256(json.dumps(links).encode()).hexdigest(),
        }
    for monitor in monitors:
        url = monitor["url"]
        related = [
            row
            for row in rules.get("specifications", [])
            if row["exchange"] == monitor["exchange"]
            and row["product"] == monitor.get("product")
        ]
        binding = hashlib.sha256(
            json.dumps({"monitor": monitor, "rules": related}, sort_keys=True).encode()
        ).hexdigest()
        current = versions[url]
        selection = monitor.get("approved_selection")
        if (
            selection is not None
            and sorted(map(canonical, selection)) != current["documents"]
        ):
            raise ValueError(
                f"Official catalog selection differs from explicit approval: {url}"
            )
        old = (previous or {}).get(url)
        if (
            old
            and old.get("binding_sha256") == binding
            and {k: v for k, v in old.items() if k != "binding_sha256"} != current
        ):
            raise ValueError(
                f"Official catalog selection changed; local approval required: {url}"
            )
        current["binding_sha256"] = binding
    return versions


def extract_monitors(evidence, rules):
    """Current entry points discovered from retained official site navigation."""
    from collections import defaultdict
    import csv
    from pathlib import Path

    candidates = defaultdict(dict)
    product_names = {}
    for manifest in Path(evidence).glob("dce-product-spec-*.tsv"):
        for row in csv.DictReader(manifest.open(), delimiter="\t"):
            product_names[row["variety_name"]] = row["product"]
    for path in Path(evidence).glob("*.ht*"):
        text = decode(path.read_bytes())
        parser = Links()
        parser.feed(text)
        for href, label in parser.links:
            if href.startswith(("javascript:", "#")):
                continue
            if re.search(r"/standard_([a-z]+)(?:_f)?/", href):
                product = re.search(r"/standard_([a-z]+)(?:_f)?/", href)[1]
                for exchange, host in [
                    ("SHFE", "www.shfe.com.cn"),
                    ("INE", "www.ine.cn"),
                ]:
                    if any(
                        r["exchange"] == exchange and r["product"] == product
                        for r in rules["specifications"]
                    ):
                        key = href[
                            : re.search(r"/standard_([a-z]+)(?:_f)?/", href).end()
                        ]
                        candidates[exchange][product] = urljoin("https://" + host, key)
            product_root = re.search(r"/products/futures/.+?/([a-z]+)_f/", href)
            if product_root:
                product = product_root[1]
                for exchange, host in [
                    ("SHFE", "www.shfe.com.cn"),
                    ("INE", "www.ine.cn"),
                ]:
                    if any(
                        r["exchange"] == exchange and r["product"] == product
                        for r in rules["specifications"]
                    ):
                        candidates[exchange].setdefault(
                            product,
                            urljoin("https://" + host, href[: product_root.end()]),
                        )
            if label == "期货合约" and "/cn/sspz/" in href and "/bzhy/qhhy/" in href:
                candidates["CZCE"][href] = urljoin("https://www.czce.com.cn", href)
            if href.endswith("sspz.shtml") and "/gfex/" in href:
                candidates["GFEX"][href] = urljoin("https://www.gfex.com.cn", href)
            if (
                "/dce/channel/list/" in href
                and "期货" in label
                and len(label) < 20
                and not any(w in label for w in ("合约", "关于", "征求"))
            ):
                name = (
                    label.replace("期货/期权", "")
                    .replace("期货", "")
                    .replace("期权", "")
                )
                matches = [(n, p) for n, p in product_names.items() if n in name]
                if not matches:
                    continue
                name = max(matches, key=lambda pair: len(pair[0]))[1]
                prior = candidates["DCE"].get(name)
                number = int(re.search(r"/list/(\d+)", href)[1])
                if prior is None or number > int(re.search(r"/list/(\d+)", prior)[1]):
                    candidates["DCE"][name] = urljoin("https://www.dce.com.cn", href)
    for source in rules["sources"]:
        if source["exchange"] == "CFFEX":
            candidates["CFFEX"][source["url"]] = source["url"]
    czce = {
        "bl": "FG",
        "bt": "SR",
        "bxqhqq": "PL",
        "cjqh": "SA",
        "czp": "RM",
        "czy": "OI",
        "dejbqhqq": "PX",
        "dlm": "ZC",
        "dxqh": "PF",
        "gt": "SF",
        "hsqh": "PK",
        "hzqh": "CJ",
        "jc": "MA",
        "jd": "JR",
        "meng": "SM",
        "mh": "CF",
        "miansha": "CY",
        "nsqh": "UR",
        "pg": "AP",
        "pm": "PM",
        "ppqh": "PR",
        "pta": "TA",
        "qm": "WH",
        "sjqhqq": "SH",
        "wxd": "LR",
        "ycz": "RS",
        "zxd": "RI",
    }
    gfex = {"gyeg": "si", "tsl": "lc", "djg": "ps", "sspzp": "pd", "sspzb": "pt"}
    monitors = []
    for exchange, items in sorted(candidates.items()):
        for key, url in sorted(items.items()):
            if exchange == "CZCE":
                product = czce[url.split("/sspz/")[1].split("/")[0]]
            elif exchange == "GFEX":
                product = gfex[url.split("/gfex/")[1].split("/")[0]]
            elif exchange == "CFFEX":
                product = next(
                    r["product"]
                    for r in rules["specifications"]
                    if r["exchange"] == exchange and r["source_url"] == url
                )
            else:
                product = key
            approved = sorted(
                {
                    row["source_url"]
                    for row in rules["specifications"]
                    if row["exchange"] == exchange and row["product"] == product
                }
            )
            if not approved:
                raise ValueError(
                    f"No approved current product rules for {exchange}.{product}"
                )
            monitors.append(
                {
                    "exchange": exchange,
                    "url": url,
                    "product": product,
                    "approved_documents": approved,
                }
            )
    for monitor in monitors:
        if monitor["exchange"] == "CFFEX":
            monitor["mode"] = "document"
            if "/jystz/" in monitor["url"]:
                rule = next(
                    r
                    for r in rules["specifications"]
                    if r["exchange"] == "CFFEX" and r["source_url"] == monitor["url"]
                )
                if rule["product"] != "TS":
                    raise ValueError(
                        "Historical announcement is not a current rule entry"
                    )
                monitor.update(
                    url="http://www.cffex.com.cn/cn/2ts.html",
                    mode="treasury",
                    product="TS",
                    lot_size=rule["lot_size"],
                    tick_size=rule["tick_size"],
                )
    if set(candidates) != {"CFFEX", "CZCE", "DCE", "GFEX", "INE", "SHFE"}:
        raise ValueError("Retained official navigation lacks current catalog families")
    return monitors
