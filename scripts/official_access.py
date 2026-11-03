"""Runtime request deadline shared by official HTTP and browser collectors."""

import os
from datetime import datetime, timezone
import requests
from urllib.parse import urljoin, urlparse


class AccessWindowClosed(RuntimeError):
    pass


def now():
    return datetime.now(timezone.utc)


def remaining():
    value = os.environ.get("FUTURE_META_ACCESS_DEADLINE")
    if value is None:
        return None  # Explicit local historical collection retains its existing CLI.
    deadline = datetime.fromisoformat(value)
    if deadline.tzinfo is None:
        raise ValueError("Access deadline requires a timezone")
    seconds = (deadline - now()).total_seconds()
    if seconds <= 0:
        raise AccessWindowClosed("Official exchange access window closed")
    return seconds


def request(method, url, **kwargs):
    domains = (
        "shfe.com.cn",
        "shfe.cn",
        "ine.cn",
        "dce.com.cn",
        "czce.com.cn",
        "gfex.com.cn",
        "cffex.com.cn",
    )
    for _ in range(6):
        parsed = urlparse(url)
        if parsed.scheme not in ("http", "https") or not any(
            parsed.hostname == d or (parsed.hostname or "").endswith("." + d)
            for d in domains
        ):
            raise ValueError("Unapproved official transport URL")
        seconds = remaining()
        options = dict(kwargs, allow_redirects=False)
        if seconds is not None:
            options["timeout"] = min(float(kwargs.get("timeout", 15)), seconds)
        response = requests.request(method, url, **options)
        status = getattr(response, "status_code", None)
        if status not in (301, 302, 303, 307, 308):
            return response
        destination = response.headers.get("Location")
        response.close()
        if not destination:
            raise ValueError("Official redirect has no destination")
        url = urljoin(url, destination)
        if status == 303 or (status in (301, 302) and method.upper() == "POST"):
            method = "GET"
            kwargs.pop("data", None)
            kwargs.pop("json", None)
    raise ValueError("Official redirect limit exceeded")


def guard_browser(page, *, document_url=None):
    """Check every hop; optionally admit only one GET of a static data file."""
    if os.environ.get("FUTURE_META_ACCESS_DEADLINE") is None and document_url is None:
        return
    from playwright.sync_api import Error

    # CDP pauses each network request, including redirects; a Playwright route
    # handler alone does not intercept every redirect hop.
    session = page.context.new_cdp_session(page)
    admitted = False

    def paused(event):
        nonlocal admitted
        try:
            remaining()
        except AccessWindowClosed:
            allowed = False
        else:
            request = event["request"] if document_url is not None else None
            allowed = document_url is None or (
                not admitted
                and request["url"] == document_url
                and request["method"] == "GET"
                and event.get("resourceType") == "Document"
            )
        try:
            if not allowed:
                session.send(
                    "Fetch.failRequest",
                    {"requestId": event["requestId"], "errorReason": "Aborted"},
                )
            else:
                admitted = True
                session.send("Fetch.continueRequest", {"requestId": event["requestId"]})
        except Error:
            # Closing Chrome can cancel a pending favicon interception.
            if not page.is_closed():
                raise

    session.on("Fetch.requestPaused", paused)
    session.send(
        "Fetch.enable", {"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}
    )
