"""Runtime request deadline shared by official HTTP and browser collectors."""

import base64
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


def validate_url(url):
    domains = (
        "shfe.com.cn",
        "shfe.cn",
        "ine.cn",
        "dce.com.cn",
        "czce.com.cn",
        "gfex.com.cn",
        "cffex.com.cn",
    )
    parsed = urlparse(url)
    if parsed.username or parsed.password or parsed.scheme not in ("http", "https") or not any(
        parsed.hostname == d or (parsed.hostname or "").endswith("." + d)
        for d in domains
    ):
        raise ValueError("Unapproved official transport URL")


def request(method, url, *, client="requests", **kwargs):
    if client == "requests":
        send = requests.request
    elif client == "curl_cffi":
        from curl_cffi import requests as curl_requests

        def send(method, url, **options):
            # Keep curl's User-Agent aligned with its selected Chrome TLS profile.
            # The historical collector default describes an older, fixed browser.
            if "headers" in options:
                options["headers"] = {key: value for key, value in options["headers"].items()
                                      if key.lower() != "user-agent"}
            try:
                return curl_requests.request(method, url, impersonate="chrome", **options)
            except curl_requests.RequestsError as error:
                raise requests.ConnectionError(str(error)) from error
    else:
        raise ValueError("Unknown official HTTP client")
    for _ in range(6):
        validate_url(url)
        seconds = remaining()
        options = dict(kwargs, allow_redirects=False)
        if seconds is not None:
            options["timeout"] = min(float(kwargs.get("timeout", 15)), seconds)
        response = send(method, url, **options)
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


def guard_browser(page, *, document_url=None, request_url=None, request_method="GET", request_body=None):
    """Check every hop; restrict static navigation or one fetch from local HTML."""
    if document_url is not None and request_url is not None:
        raise ValueError("Choose one browser request mode")
    if os.environ.get("FUTURE_META_ACCESS_DEADLINE") is None and document_url is None and request_url is None:
        from collection_transport import current
        if current() is None:
            return
    if request_url is not None:
        validate_url(request_url)
        if request_method not in ("GET", "POST") or (request_method == "GET" and request_body is not None):
            raise ValueError("Unsupported guarded browser request")
    from playwright.sync_api import Error

    # CDP pauses each network request, including redirects; a Playwright route
    # handler alone does not intercept every redirect hop.
    session = page.context.new_cdp_session(page)
    admitted = False
    blank_admitted = False
    parsed_request = urlparse(request_url) if request_url is not None else None
    blank_url = None if parsed_request is None else (
        f"{parsed_request.scheme}://{parsed_request.netloc}/.future-meta-transport-blank")

    def paused(event):
        nonlocal admitted, blank_admitted
        try:
            remaining()
        except AccessWindowClosed:
            allowed = False
        else:
            request = event["request"]
            synthetic = (request_url is not None and not blank_admitted
                         and request["url"] == blank_url and request["method"] == "GET"
                         and event.get("resourceType") == "Document")
            if request_url is not None:
                allowed = synthetic or (
                    not admitted and blank_admitted and request["url"] == request_url
                    and request["method"] == request_method and request.get("postData") == request_body
                    and event.get("resourceType") in ("Fetch", "XHR"))
                if allowed and not synthetic:
                    try:
                        validate_url(request["url"])
                    except ValueError:
                        allowed = False
            elif document_url is not None:
                allowed = (not admitted and request["url"] == document_url
                           and request["method"] == "GET"
                           and event.get("resourceType") == "Document")
            else:
                try:
                    validate_url(request["url"])
                except ValueError:
                    allowed = False
                else:
                    allowed = True
        try:
            if not allowed:
                session.send(
                    "Fetch.failRequest",
                    {"requestId": event["requestId"], "errorReason": "Aborted"},
                )
            elif request_url is not None and synthetic:
                blank_admitted = True
                session.send("Fetch.fulfillRequest", {
                    "requestId": event["requestId"], "responseCode": 200,
                    "responseHeaders": [{"name": "Content-Type", "value": "text/html; charset=utf-8"}],
                    "body": base64.b64encode(b"<!doctype html><title>transport</title>").decode("ascii"),
                })
            else:
                admitted = True
                session.send("Fetch.continueRequest", {"requestId": event["requestId"]})
        except Error as error:
            # Navigation or closing Chrome can cancel a paused request before
            # its acknowledgement. Other live-page protocol errors still fail.
            cancelled = any(str(error).endswith(message) for message in (
                "Protocol error (Fetch.continueRequest): Invalid InterceptionId.",
                "Protocol error (Fetch.failRequest): Invalid InterceptionId.",
            ))
            if not page.is_closed() and not cancelled:
                raise

    session.on("Fetch.requestPaused", paused)
    session.send(
        "Fetch.enable", {"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}
    )
