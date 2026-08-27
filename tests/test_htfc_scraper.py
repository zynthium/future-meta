import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import requests

import htfc_scraper


class AlwaysFailingSession:
    def __init__(self):
        self.calls = 0

    def post(self, *_args, **_kwargs):
        self.calls += 1
        raise requests.RequestException("temporary network failure")


class HtfcScraperTests(unittest.TestCase):
    def test_extract_article_body_uses_structured_wz_content_only(self):
        html = """
            <html><body>
              <nav>站点导航</nav>
              <div class="wz_content">手续费调整 <span>自 8 月 27 日起</span><div>适用于期货</div></div>
              <footer>页脚手续费标准链接</footer>
            </body></html>
        """

        self.assertEqual(
            htfc_scraper.extract_article_body(html),
            "手续费调整 自 8 月 27 日起 适用于期货",
        )

    def test_extract_article_body_rejects_missing_selector(self):
        with self.assertRaisesRegex(htfc_scraper.CrawlError, "wz_content"):
            htfc_scraper.extract_article_body("<html><body>无正文容器</body></html>")

    def test_parse_announcement_page_rejects_invalid_pagination(self):
        payload = {
            "error_no": "0",
            "results": [{"data": [{"article_id": "1"}], "totalPages": "0", "totalRows": "1"}],
        }

        with self.assertRaisesRegex(htfc_scraper.CrawlError, "totalPages"):
            htfc_scraper.parse_announcement_page(payload, page=1)

    def test_fetch_page_retries_then_raises(self):
        session = AlwaysFailingSession()
        with patch.object(htfc_scraper.time, "sleep"):
            with self.assertRaisesRegex(htfc_scraper.CrawlError, "列表接口"):
                htfc_scraper.fetch_announcement_page(
                    page=1,
                    retries=2,
                    session=session,
                )
        self.assertEqual(session.calls, 2)

    def test_crawl_rejects_short_page_instead_of_writing_partial_data(self):
        pages = {
            1: ([{"article_id": "1", "url": "/a"}, {"article_id": "2", "url": "/b"}], 2, 4),
            2: ([{"article_id": "3", "url": "/c"}], 2, 4),
        }

        with self.assertRaisesRegex(htfc_scraper.CrawlError, "记录数不足"):
            htfc_scraper.crawl_announcements(
                max_pages=None,
                page_size=2,
                delay=0,
                fetch_page=lambda page, _page_size: pages[page],
                fetch_detail=lambda _url: "正文",
            )

    def test_atomic_write_replaces_old_json_only_after_serialization(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "announcements.json"
            output.write_text("old", encoding="utf-8")

            htfc_scraper.write_announcements_atomically(
                output,
                [{"article_id": "1", "content": "完整正文"}],
            )

            self.assertEqual(
                json.loads(output.read_text(encoding="utf-8")),
                [{"article_id": "1", "content": "完整正文"}],
            )
            self.assertEqual(list(Path(directory).glob("*.tmp")), [])


if __name__ == "__main__":
    unittest.main()
