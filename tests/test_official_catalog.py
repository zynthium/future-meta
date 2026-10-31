import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import catalog_guard as catalogs


class CatalogTests(unittest.TestCase):
    def test_replacement_link_fails_even_if_old_document_remains_unchanged(self):
        monitor = {
            "exchange": "DCE",
            "url": "https://www.dce.com.cn/dce/channel/list/2242.html",
        }
        rules = {
            "catalogs": [monitor],
            "sources": [{"url": "https://www.dce.com.cn/old.doc", "sha256": "a" * 64}],
        }
        body = b'<main><a href="/old.doc">Old</a><a href="/new.doc">futures contract</a></main>'
        response = type("Response", (), {"status_code": 200, "content": body})()
        with patch.object(catalogs, "fetch_catalog", return_value=response):
            with self.assertRaisesRegex(ValueError, "unapproved"):
                catalogs.check_catalogs(rules)

    def test_current_approved_attachment_is_accepted_and_navigation_ignored(self):
        url = "https://www.czce.com.cn/cn/sspz/bt/bzhy/qhhy/index.htm"
        other = "https://www.czce.com.cn/cn/sspz/mh/bzhy/qhhy/index.htm"
        rules = {
            "catalogs": [
                {"exchange": "CZCE", "url": url},
                {"exchange": "CZCE", "url": other},
            ],
            "sources": [
                {"url": "https://www.czce.com.cn/approved.pdf", "sha256": "a" * 64}
            ],
        }
        body = (
            '<nav><a href="'
            + other
            + '">期货合约</a></nav><main><a href="/approved.pdf">郑州商品交易所白糖期货合约</a></main>'
        ).encode()
        response = type("Response", (), {"status_code": 200, "content": body})()
        with patch.object(catalogs, "fetch_catalog", return_value=response):
            result = catalogs.check_catalogs(rules)
        self.assertEqual(set(result), {url, other})

    def test_empty_or_dynamic_catalog_cannot_mean_unchanged(self):
        rules = {
            "catalogs": [
                {"exchange": "DCE", "url": "https://www.dce.com.cn/catalog.html"}
            ],
            "sources": [],
        }
        response = type(
            "Response",
            (),
            {"status_code": 200, "content": b"<html>javascript app</html>"},
        )()
        with patch.object(catalogs, "fetch_catalog", return_value=response):
            with self.assertRaisesRegex(ValueError, "no current"):
                catalogs.check_catalogs(rules)


if __name__ == "__main__":
    unittest.main()


class CatalogRegressionTests(unittest.TestCase):
    def test_revision_notice_with_rules_in_title_is_relevant(self):
        url = "https://www.dce.com.cn/catalog.html"
        rules = {
            "catalogs": [{"exchange": "DCE", "url": url}],
            "sources": [{"url": "https://www.dce.com.cn/old.doc", "sha256": "a" * 64}],
        }
        body = '<a href="/old.doc">豆粕期货合约</a><a href="/new.html">关于修订《豆粕期货合约》及其细则的通知</a>'.encode()
        response = type("Response", (), {"status_code": 200, "content": body})()
        with patch.object(catalogs, "fetch_catalog", return_value=response):
            with self.assertRaisesRegex(ValueError, "unapproved"):
                catalogs.check_catalogs(rules)

    def test_unapproved_html_is_not_approved_by_its_link_to_old_document(self):
        url = "https://www.dce.com.cn/catalog.html"
        rules = {
            "catalogs": [{"exchange": "DCE", "url": url}],
            "sources": [{"url": "https://www.dce.com.cn/old.doc", "sha256": "a" * 64}],
        }
        bodies = {
            url: '<a href="/new.html">豆粕期货合约</a>',
            "https://www.dce.com.cn/new.html": '每手20吨<a href="/old.doc">旧版合约每手10吨</a>',
        }

        def fetch(url):
            return type(
                "Response", (), {"status_code": 200, "content": bodies[url].encode()}
            )()

        with patch.object(catalogs, "fetch_catalog", side_effect=fetch):
            with self.assertRaisesRegex(ValueError, "unapproved"):
                catalogs.check_catalogs(rules)

    def test_monitor_root_cannot_shortcut_just_because_it_is_approved_evidence(self):
        url = "http://www.cffex.com.cn/cn/2ts.html"
        rules = {
            "catalogs": [{"exchange": "CFFEX", "url": url, "mode": "document"}],
            "sources": [{"url": url, "sha256": "a" * 64}],
        }
        response = type(
            "Response", (), {"status_code": 200, "content": b"changed current document"}
        )()
        with patch.object(catalogs, "fetch_catalog", return_value=response) as fetch:
            with self.assertRaisesRegex(ValueError, "changed"):
                catalogs.check_catalogs(rules)
            fetch.assert_called_once()


class TreasuryCurrentDocumentTests(unittest.TestCase):
    def test_current_ts_table_is_compared_to_approved_business_rules(self):
        url = "http://www.cffex.com.cn/cn/2ts.html"
        body = """<table><tr><td>交易代码</td><td>TS</td></tr>
        <tr><td>合约标的</td><td>面值为200万元人民币的2年期国债</td></tr>
        <tr><td>报价方式</td><td>百元净价报价</td></tr>
        <tr><td>最小变动价位</td><td>0.002元</td></tr>
        <tr><td>合约月份</td><td>最近的三个季月</td></tr>
        <tr><td>最后交易日</td><td>合约到期月份的第二个星期五，遇法定节假日顺延</td></tr></table>""".encode()
        rules = {
            "catalogs": [
                {
                    "exchange": "CFFEX",
                    "url": url,
                    "mode": "treasury",
                    "product": "TS",
                    "lot_size": 20000.0,
                    "tick_size": 0.002,
                }
            ],
            "sources": [],
        }
        response = type("Response", (), {"status_code": 200, "content": body})()
        with patch.object(catalogs, "fetch_catalog", return_value=response):
            self.assertIn(url, catalogs.check_catalogs(rules))
        response.content = body.replace(b"0.002", b"0.005")
        with patch.object(catalogs, "fetch_catalog", return_value=response):
            with self.assertRaisesRegex(ValueError, "changed"):
                catalogs.check_catalogs(rules)
        for changed in [
            body.replace("第二个星期五".encode(), "第二个星期五前一个交易日".encode()),
            body.replace(
                "百元净价报价".encode(),
                "千元净价报价（原百元净价报价不再适用）".encode(),
            ),
        ]:
            response.content = changed
            with patch.object(catalogs, "fetch_catalog", return_value=response):
                with self.assertRaises(ValueError):
                    catalogs.check_catalogs(rules)


class CatalogBindingTests(unittest.TestCase):
    def test_other_products_approved_document_cannot_replace_current_selection(self):
        url = "https://www.dce.com.cn/catalog.html"
        rules = {
            "catalogs": [
                {
                    "exchange": "DCE",
                    "url": url,
                    "product": "m",
                    "approved_documents": ["https://www.dce.com.cn/old.doc"],
                }
            ],
            "sources": [
                {"url": "https://www.dce.com.cn/old.doc", "sha256": "a" * 64},
                {"url": "https://www.dce.com.cn/other.doc", "sha256": "b" * 64},
            ],
        }
        response = type(
            "Response",
            (),
            {
                "status_code": 200,
                "content": '<a href="/other.doc">豆粕期货合约</a>'.encode(),
            },
        )()
        with patch.object(catalogs, "fetch_catalog", return_value=response):
            with self.assertRaisesRegex(ValueError, "selection"):
                catalogs.check_catalogs(rules)


class CatalogSelectionHistoryTests(unittest.TestCase):
    def test_selection_change_requires_explicit_local_approval(self):
        url = "https://www.dce.com.cn/catalog.html"
        rules = {
            "catalogs": [
                {
                    "exchange": "DCE",
                    "url": url,
                    "product": "m",
                    "approved_documents": [
                        "https://www.dce.com.cn/old.doc",
                        "https://www.dce.com.cn/new.doc",
                    ],
                }
            ],
            "sources": [
                {"url": "https://www.dce.com.cn/old.doc", "sha256": "a" * 64},
                {"url": "https://www.dce.com.cn/new.doc", "sha256": "b" * 64},
            ],
        }
        response = type(
            "Response",
            (),
            {
                "status_code": 200,
                "content": '<a href="/old.doc">豆粕期货合约</a>'.encode(),
            },
        )()
        with patch.object(catalogs, "fetch_catalog", return_value=response):
            previous = catalogs.check_catalogs(rules)
        response.content = '<a href="/new.doc">豆粕期货合约</a>'.encode()
        with patch.object(catalogs, "fetch_catalog", return_value=response):
            with self.assertRaisesRegex(ValueError, "selection changed"):
                catalogs.check_catalogs(rules, previous=previous)
            rules["catalogs"][0]["approved_selection"] = [
                "https://www.dce.com.cn/new.doc"
            ]
            self.assertEqual(
                catalogs.check_catalogs(rules, previous=previous)[url]["documents"],
                ["https://www.dce.com.cn/new.doc"],
            )
