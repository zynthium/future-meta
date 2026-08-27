#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Fail-closed HTFC announcement crawler."""

import argparse
import json
import os
import re
import sys
import tempfile
import time
from html.parser import HTMLParser
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Tuple
from urllib.parse import urljoin

import requests

BASE_URL = "https://htfc.com"
API_ENDPOINT = "https://htfc.com/servlet/json"
DEFAULT_PAGE_SIZE = 20
DEFAULT_TIMEOUT_SECONDS = 20.0
DEFAULT_RETRIES = 3
DEFAULT_RETRY_DELAY_SECONDS = 1.0

HEADERS = {
    "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
    "X-Requested-With": "XMLHttpRequest",
    "Referer": "https://htfc.com/main/index/ggdt/index.shtml",
    "Content-Type": "application/x-www-form-urlencoded; charset=UTF-8",
}


class CrawlError(RuntimeError):
    """The source could not be crawled completely and safely."""


class ArticleBodyParser(HTMLParser):
    """Extract text exclusively from the site's ``wz_content`` container."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self._depth: Optional[int] = None
        self._parts: List[str] = []
        self.found = False

    def handle_starttag(self, tag: str, attrs: List[Tuple[str, Optional[str]]]) -> None:
        if self._depth is not None:
            self._depth += 1
            return
        classes = dict(attrs).get("class", "") or ""
        if "wz_content" in classes.split():
            self._depth = 1
            self.found = True

    def handle_startendtag(
        self, tag: str, attrs: List[Tuple[str, Optional[str]]]
    ) -> None:
        if self._depth is not None and tag == "br":
            self._parts.append(" ")

    def handle_endtag(self, _tag: str) -> None:
        if self._depth is None:
            return
        self._depth -= 1
        if self._depth == 0:
            self._depth = None

    def handle_data(self, data: str) -> None:
        if self._depth is not None:
            self._parts.append(data)

    def text(self) -> str:
        return re.sub(r"\s+", " ", " ".join(self._parts)).strip()


def parse_announcement_page(payload: object, page: int) -> Tuple[List[Dict[str, Any]], int, int]:
    """Validate one list API response before using its pagination metadata."""
    if not isinstance(payload, dict):
        raise CrawlError(f"列表接口第 {page} 页返回的 JSON 不是对象")
    if payload.get("error_no") != "0":
        raise CrawlError(
            f"列表接口第 {page} 页返回错误: {payload.get('error_info', '未知错误')}"
        )
    results = payload.get("results")
    if not isinstance(results, list) or not results or not isinstance(results[0], dict):
        raise CrawlError(f"列表接口第 {page} 页缺少 results[0]")
    info = results[0]
    items = info.get("data")
    if not isinstance(items, list) or not all(isinstance(item, dict) for item in items):
        raise CrawlError(f"列表接口第 {page} 页 data 不是公告数组")
    try:
        total_pages = int(info["totalPages"])
        total_rows = int(info["totalRows"])
    except (KeyError, TypeError, ValueError) as exc:
        raise CrawlError(f"列表接口第 {page} 页分页元数据无效") from exc
    if total_pages <= 0:
        raise CrawlError(f"列表接口第 {page} 页 totalPages 必须大于零")
    if total_rows < 0:
        raise CrawlError(f"列表接口第 {page} 页 totalRows 不能为负数")
    if page > total_pages:
        raise CrawlError(f"列表接口第 {page} 页超过 totalPages={total_pages}")
    return items, total_pages, total_rows


def fetch_announcement_page(
    page: int = 1,
    page_size: int = DEFAULT_PAGE_SIZE,
    catalog_id: str = "10320",
    search_word: str = "",
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
    retries: int = DEFAULT_RETRIES,
    session: Optional[requests.Session] = None,
) -> Tuple[List[Dict[str, Any]], int, int]:
    """Fetch one list page, retrying transient transport and JSON failures."""
    if page <= 0 or page_size <= 0 or timeout <= 0 or retries <= 0:
        raise CrawlError("page、page_size、timeout 和 retries 必须为正数")
    payload = {
        "funcNo": "2000065",
        "catalogId": str(catalog_id),
        "pageNum": str(page),
        "pageSize": str(page_size),
        "searchWord": search_word,
    }
    client = session or requests
    last_error: Optional[BaseException] = None
    for attempt in range(1, retries + 1):
        try:
            response = client.post(
                API_ENDPOINT,
                data=payload,
                headers=HEADERS,
                timeout=timeout,
            )
            response.raise_for_status()
            return parse_announcement_page(response.json(), page)
        except (requests.RequestException, ValueError) as exc:
            last_error = exc
            if attempt < retries:
                time.sleep(DEFAULT_RETRY_DELAY_SECONDS * attempt)
    raise CrawlError(f"列表接口第 {page} 页请求失败，已重试 {retries} 次: {last_error}")


def article_url(relative_url: object) -> str:
    """Turn the source's relative article URL into an absolute URL."""
    if not isinstance(relative_url, str) or not relative_url.strip():
        raise CrawlError("公告缺少正文 URL")
    return urljoin(f"{BASE_URL}/", relative_url)


def extract_article_body(html: str) -> str:
    """Extract the actual article body and reject selector changes explicitly."""
    parser = ArticleBodyParser()
    parser.feed(html)
    parser.close()
    if not parser.found:
        raise CrawlError("正文页面缺少 wz_content 容器")
    body = parser.text()
    if not body:
        raise CrawlError("正文页面的 wz_content 容器为空")
    return body


def fetch_article_detail(
    relative_url: object,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
    retries: int = DEFAULT_RETRIES,
    session: Optional[requests.Session] = None,
) -> str:
    """Fetch and structurally extract one announcement body."""
    if timeout <= 0 or retries <= 0:
        raise CrawlError("timeout 和 retries 必须为正数")
    full_url = article_url(relative_url)
    client = session or requests
    last_error: Optional[BaseException] = None
    for attempt in range(1, retries + 1):
        try:
            response = client.get(full_url, headers=HEADERS, timeout=timeout)
            response.raise_for_status()
            response.encoding = "utf-8"
            return extract_article_body(response.text)
        except requests.RequestException as exc:
            last_error = exc
            if attempt < retries:
                time.sleep(DEFAULT_RETRY_DELAY_SECONDS * attempt)
    raise CrawlError(f"正文请求失败 {full_url}，已重试 {retries} 次: {last_error}")


def crawl_announcements(
    max_pages: Optional[int] = None,
    page_size: int = DEFAULT_PAGE_SIZE,
    delay: float = 0.3,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
    retries: int = DEFAULT_RETRIES,
    session: Optional[requests.Session] = None,
    fetch_page: Optional[Callable[[int, int], Tuple[List[Dict[str, Any]], int, int]]] = None,
    fetch_detail: Optional[Callable[[object], str]] = None,
) -> List[Dict[str, Any]]:
    """Crawl all requested pages or fail without returning a partial snapshot."""
    if max_pages is not None and max_pages <= 0:
        raise CrawlError("max_pages 必须为正数或省略")
    if page_size <= 0 or delay < 0:
        raise CrawlError("page_size 必须为正数，delay 不能为负数")

    if fetch_page is None:
        def fetch_page(page: int, configured_page_size: int) -> Tuple[List[Dict[str, Any]], int, int]:
            return fetch_announcement_page(
                page=page,
                page_size=configured_page_size,
                timeout=timeout,
                retries=retries,
                session=session,
            )
    if fetch_detail is None:
        def fetch_detail(relative_url: object) -> str:
            return fetch_article_detail(
                relative_url,
                timeout=timeout,
                retries=retries,
                session=session,
            )

    first_items, total_pages, total_rows = fetch_page(1, page_size)
    target_pages = min(total_pages, max_pages) if max_pages is not None else total_pages
    expected_rows = min(total_rows, target_pages * page_size)
    if not first_items and expected_rows > 0:
        raise CrawlError("第 1 页为空，拒绝写入不完整公告数据")

    results: List[Dict[str, Any]] = []
    article_ids = set()
    for page in range(1, target_pages + 1):
        items = first_items if page == 1 else fetch_page(page, page_size)[0]
        if not items:
            raise CrawlError(f"第 {page} 页为空，拒绝写入不完整公告数据")
        if len(items) > page_size:
            raise CrawlError(f"第 {page} 页返回 {len(items)} 条，超过 page_size={page_size}")

        for item in items:
            article_id = item.get("article_id")
            if not isinstance(article_id, (str, int)) or not str(article_id).strip():
                raise CrawlError(f"第 {page} 页有公告缺少 article_id")
            article_id = str(article_id)
            if article_id in article_ids:
                raise CrawlError(f"公告 article_id 重复: {article_id}")
            article_ids.add(article_id)
            relative_url = item.get("url")
            full_url = article_url(relative_url)
            content = fetch_detail(relative_url)
            if not content.strip():
                raise CrawlError(f"公告正文为空: {article_id}")
            results.append(
                {
                    "article_id": article_id,
                    "title": item.get("title"),
                    "author": item.get("author"),
                    "publish_date": item.get("publish_date"),
                    "create_date": item.get("create_date"),
                    "url": full_url,
                    "content": content,
                }
            )
            if delay > 0:
                time.sleep(delay)

    if len(results) != expected_rows:
        raise CrawlError(
            f"公告记录数不足: 预期 {expected_rows} 条，实际 {len(results)} 条"
        )
    return results


def write_announcements_atomically(output: Path, announcements: List[Dict[str, Any]]) -> None:
    """Serialize first, then atomically replace the previous snapshot."""
    serialized = json.dumps(announcements, ensure_ascii=False, indent=2)
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary_path: Optional[Path] = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=output.parent,
            prefix=f".{output.name}.",
            suffix=".tmp",
            delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
            temporary.write(serialized)
            temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_path, output)
        temporary_path = None
    except OSError as exc:
        raise CrawlError(f"原子写入公告 JSON 失败: {output}: {exc}") from exc
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def main(argv: Optional[List[str]] = None) -> int:
    """Run the crawler and return a non-zero status for any incomplete crawl."""
    parser = argparse.ArgumentParser(description="抓取华泰期货公告动态")
    parser.add_argument("--output", type=Path, default=Path("htfc_announcements.json"))
    parser.add_argument("--max-pages", type=int, default=None, help="默认抓取 API 报告的全部页")
    parser.add_argument("--page-size", type=int, default=DEFAULT_PAGE_SIZE)
    parser.add_argument("--delay", type=float, default=0.2)
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--retries", type=int, default=DEFAULT_RETRIES)
    args = parser.parse_args(argv)

    try:
        announcements = crawl_announcements(
            max_pages=args.max_pages,
            page_size=args.page_size,
            delay=args.delay,
            timeout=args.timeout,
            retries=args.retries,
        )
        write_announcements_atomically(args.output, announcements)
    except CrawlError as exc:
        print(f"[失败] {exc}", file=sys.stderr)
        return 1

    print(f"抓取完成：{len(announcements)} 条公告，已原子写入 {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
