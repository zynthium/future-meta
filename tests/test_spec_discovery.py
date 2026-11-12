import os
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import incremental_inputs as inputs
import spec_discovery as discovery


SAMPLE_HTML = """
<table id="table-futures-basic-data">
  <tr> <th>交易品种 </th><td>葵花籽油</td><th>交易单位 </th><td>20吨/手</td><th>报价单位 </th><td>元（人民币）/吨</td> </tr>
  <tr> <th>最小变动价位 </th><td>1元/吨</td><th>涨跌停板幅度 </th><td>±4%</td><th>合约交割月份 </th><td>1、3、5、7、9、11月</td> </tr>
  <tr> <th>交易代码 </th><td>SO</td><th>上市交易所 </th><td>郑州商品交易所</td> </tr>
</table>
"""

SAMPLE_FB_HTML = """
<table id="table-futures-basic-data">
  <tr> <th>交易品种 </th><td>中密度纤维板</td><th>交易单位 </th><td>10张/手</td><th>报价单位 </th><td>元(人民币)/张</td> </tr>
  <tr> <th>最小变动价位 </th><td>0.5元/张</td><th>涨跌停板幅度 </th><td>±5%</td><th>合约交割月份 </th><td>1---12月</td> </tr>
  <tr> <th>交易代码 </th><td>FB</td><th>上市交易所 </th><td>大连商品交易所</td> </tr>
</table>
"""


class SpecDiscoveryTests(unittest.TestCase):
    def test_parser_extracts_structured_basic_data(self):
        parser = discovery.FuturesBasicDataParser()
        parser.feed(SAMPLE_HTML)
        self.assertEqual(parser.data["交易品种"], "葵花籽油")
        self.assertEqual(parser.data["交易单位"], "20吨/手")
        self.assertEqual(parser.data["最小变动价位"], "1元/吨")
        self.assertEqual(parser.data["交易代码"], "SO")
        self.assertEqual(parser.data["上市交易所"], "郑州商品交易所")

    def test_numeric_parsing(self):
        self.assertEqual(discovery.parse_numeric("20吨/手"), 20.0)
        self.assertEqual(discovery.parse_numeric("1元/吨"), 1.0)
        self.assertEqual(discovery.parse_numeric("0.05元/张"), 0.05)
        self.assertEqual(discovery.parse_numeric("0.2点"), 0.2)
        self.assertIsNone(discovery.parse_numeric(""))

    def test_symbol_normalization_for_sina(self):
        self.assertEqual(discovery.normalize_sina_contract("CZCE", "SO705"), "SO2705")
        self.assertEqual(discovery.normalize_sina_contract("CZCE", "SR701"), "SR2701")
        self.assertEqual(discovery.normalize_sina_contract("SHFE", "cu2701"), "CU2701")
        self.assertEqual(discovery.normalize_sina_contract("DCE", "m2701"), "M2701")
        self.assertEqual(discovery.normalize_sina_contract("CFFEX", "IF2612"), "IF2612")

    def test_build_specification_entry_matches_schema(self):
        entry = discovery.build_specification_entry(
            exchange="CZCE",
            product="SO",
            lot_size=20.0,
            tick_size=1.0,
            valid_from="2026-11-12",
        )
        self.assertEqual(entry["exchange"], "CZCE")
        self.assertEqual(entry["product"], "SO")
        self.assertEqual(entry["lot_size"], 20.0)
        self.assertEqual(entry["tick_size"], 1.0)
        self.assertEqual(entry["valid_from"], "2026-11-12")
        self.assertEqual(entry["valid_from_phase"], "date")
        self.assertEqual(entry["valid_to"], "")

    @patch.object(discovery, "fetch_public_contract_spec")
    def test_auto_register_specification_adds_to_rules(self, mock_fetch):
        mock_fetch.return_value = {
            "exchange": "CZCE",
            "product": "SO",
            "contract": "SO705",
            "lot_size": 20.0,
            "tick_size": 1.0,
            "variety_name": "葵花籽油",
            "quote_unit": "元/吨",
            "source_url": "https://finance.sina.com.cn/test.shtml",
        }
        rules = {"specifications": []}
        entry = discovery.auto_register_specification(rules, "CZCE.SO705", "2026-11-12")
        self.assertIsNotNone(entry)
        self.assertEqual(entry["lot_size"], 20.0)
        self.assertEqual(len(rules["specifications"]), 1)
        self.assertEqual(rules["specifications"][0]["product"], "SO")

    @patch.object(discovery, "fetch_public_contract_spec")
    def test_incremental_inputs_specification_auto_discover(self, mock_fetch):
        mock_fetch.return_value = {
            "exchange": "CZCE",
            "product": "SO",
            "contract": "SO705",
            "lot_size": 20.0,
            "tick_size": 1.0,
            "variety_name": "葵花籽油",
            "quote_unit": "元/吨",
            "source_url": "https://finance.sina.com.cn/test.shtml",
        }
        rules = {"specifications": []}
        # With auto_discover=False, fails with ValueError
        with self.assertRaisesRegex(ValueError, "Missing/conflicting approved specification"):
            inputs.specification(rules, "CZCE.SO705", "2026-11-12", auto_discover=False)

        # With auto_discover=True, discovers and returns specification
        spec = inputs.specification(rules, "CZCE.SO705", "2026-11-12", auto_discover=True)
        self.assertEqual(spec["lot_size"], 20.0)
        self.assertEqual(spec["tick_size"], 1.0)
        self.assertEqual(len(rules["specifications"]), 1)

    @patch.object(discovery, "fetch_public_contract_spec")
    def test_midway_change_detection(self, mock_fetch):
        # Simulate an existing specification for DCE.fb with lot_size 500
        # and a midway contract fb2001 with new lot_size 10
        mock_fetch.return_value = {
            "exchange": "DCE",
            "product": "fb",
            "contract": "fb2001",
            "lot_size": 10.0,
            "tick_size": 0.5,
            "variety_name": "中密度纤维板",
            "quote_unit": "元/张",
            "source_url": "https://finance.sina.com.cn/fb2001.shtml",
        }
        rules = {
            "specifications": [
                {
                    "exchange": "DCE",
                    "product": "fb",
                    "full_symbol": "",
                    "valid_from": "2015-01-01",
                    "valid_to": "",
                    "lot_size": 500.0,
                    "tick_size": 0.05,
                    "valid_from_phase": "date",
                    "delivery_month_to": "2019-12",
                }
            ]
        }
        # For old contract fb1912, existing spec applies
        old_spec = inputs.specification(rules, "DCE.fb1912", "2019-11-01", auto_discover=False)
        self.assertEqual(old_spec["lot_size"], 500.0)

        # For new contract fb2001, old spec does not apply, auto_discover fetches new 10.0 spec
        new_spec = inputs.specification(rules, "DCE.fb2001", "2019-12-02", auto_discover=True)
        self.assertEqual(new_spec["lot_size"], 10.0)
        self.assertEqual(new_spec["tick_size"], 0.5)


if __name__ == "__main__":
    unittest.main()
