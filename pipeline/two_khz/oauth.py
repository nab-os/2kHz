"""Browser-based OAuth login.

Qobuz moved `user/login` behind an OAuth flow protected by reCAPTCHA around
April 2026, so email/password authentication over the API no longer works at
all, it returns 401 whether or not the password is right. The sign-in has to
happen in a real browser.

The flow:
  1. bind a local port and start a one-shot HTTP server
  2. open https://www.qobuz.com/signin/oauth?ext_app_id=..&redirect_url=..
  3. the user signs in; Qobuz redirects back with an authorisation code
  4. trade the code for a user_auth_token via oauth/callback

The alternative is copying `localuser.token` out of localStorage by hand, which
still works if the browser round-trip is inconvenient (headless box, etc).
"""

from __future__ import annotations

import socket
import sys
import threading
import webbrowser
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

import requests

from . import bundle as bundle_mod
from .qobuz import API_BASE

SIGNIN_URL = "https://www.qobuz.com/signin/oauth"
DEFAULT_TIMEOUT = 300

SUCCESS_PAGE = b"""<!doctype html><meta charset="utf-8">
<title>two_khz</title>
<body style="font:15px system-ui;background:#12131a;color:#e5e7ef;padding:3rem">
<h2>Signed in.</h2><p>You can close this tab and return to the terminal.</p>
"""

FAILURE_PAGE = b"""<!doctype html><meta charset="utf-8">
<title>two_khz</title>
<body style="font:15px system-ui;background:#12131a;color:#f7768e;padding:3rem">
<h2>No authorisation code in the redirect.</h2>
<p>Check the terminal for details.</p>
"""


class _CodeCatcher(BaseHTTPRequestHandler):
    code: str | None = None

    def do_GET(self):  # noqa: N802 (http.server's required name)
        params = parse_qs(urlparse(self.path).query)
        # Qobuz uses code_autorisation; accept plain `code` as a fallback.
        found = params.get("code_autorisation", params.get("code", [""]))[0]
        if found:
            _CodeCatcher.code = found
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(SUCCESS_PAGE)
        else:
            self.send_response(400)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(FAILURE_PAGE)

    def log_message(self, *args):
        """Silence the default per-request logging."""


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def exchange_code(code: str, private_key: str, app_id: str) -> str:
    """Trade an authorisation code for a user_auth_token."""
    response = requests.get(
        f"{API_BASE}/oauth/callback",
        params={"code": code, "private_key": private_key},
        headers={"X-App-Id": app_id, "User-Agent": bundle_mod.USER_AGENT},
        timeout=30,
    )
    if response.status_code != 200:
        raise RuntimeError(
            f"oauth/callback -> HTTP {response.status_code}: {response.text[:200]}"
        )

    data = response.json()
    token = data.get("token") or data.get("user_auth_token")
    if not token:
        raise RuntimeError(f"oauth/callback returned no token: {data}")
    return token


def login(
    app_id: str | None = None,
    private_key: str | None = None,
    timeout: int = DEFAULT_TIMEOUT,
    open_browser: bool = True,
) -> str:
    """Run the browser flow and return a user_auth_token."""
    if app_id is None or private_key is None:
        source = bundle_mod.fetch_bundle()
        if app_id is None:
            app_id, _ = bundle_mod.extract(source)
        if private_key is None:
            private_key = bundle_mod.extract_private_key(source)

    if not private_key:
        raise RuntimeError(
            "could not find the OAuth private key in the web player bundle; "
            "fall back to copying localuser.token from localStorage"
        )

    port = _free_port()
    redirect = f"http://localhost:{port}"
    url = f"{SIGNIN_URL}?ext_app_id={app_id}&redirect_url={redirect}"

    _CodeCatcher.code = None
    server = HTTPServer(("127.0.0.1", port), _CodeCatcher)
    server.timeout = timeout

    print("\nOpening your browser to sign in to Qobuz.", file=sys.stderr)
    print(f"If it does not open, visit this URL manually:\n\n  {url}\n", file=sys.stderr)
    print(f"Waiting up to {timeout}s for the redirect ...", file=sys.stderr)

    if open_browser:
        threading.Thread(target=webbrowser.open, args=(url,), daemon=True).start()

    try:
        # One request is all we need; handle_request honours server.timeout.
        server.handle_request()
    finally:
        server.server_close()

    if not _CodeCatcher.code:
        raise RuntimeError(
            "timed out waiting for the OAuth redirect. "
            "You can instead copy localuser.token from localStorage on "
            "https://play.qobuz.com/ and set QOBUZ_USER_AUTH_TOKEN."
        )

    return exchange_code(_CodeCatcher.code, private_key, app_id)
