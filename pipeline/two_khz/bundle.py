"""Scrape the Qobuz web player bundle for the app id and candidate secrets.

Qobuz does not issue API credentials to individuals, so the web player's own
app_id and signing secret are what every third-party client uses. They rotate
with player releases, which is why this is a command rather than a constant.

The secrets are obfuscated: each is split into a `seed` (attached to a timezone
in the player config) and an `info`/`extras` pair (attached to the matching
timezone entry). Concatenating the three and base64-decoding yields the secret.

Note the format has changed over time, older clients trimmed 44 trailing
characters after decoding, which is no longer correct; the current bundle
decodes to exactly the 32-character secret.
"""

from __future__ import annotations

import base64
import re
import sys

import requests

LOGIN_PAGE = "https://play.qobuz.com/login"
PLAYER_BASE = "https://play.qobuz.com"
USER_AGENT = (
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
)

BUNDLE_RE = re.compile(r'<script src="(?P<path>/resources/[^"]+/bundle\.js)"')
PRIVATE_KEY_RE = re.compile(r'privateKey:"(?P<key>[\w=]+)"')
APP_ID_RE = re.compile(r'production:\{api:\{appId:"(?P<app_id>\d+)",appSecret:"(?P<secret>\w*)"')
SEED_RE = re.compile(r'\.initialSeed\("(?P<seed>[\w=]+)",window\.utimezone\.(?P<timezone>[a-z]+)\)')
TIMEZONE_RE = re.compile(
    r'name:"(?P<name>[\w/_+-]+)",info:"(?P<info>[\w=]+)",extras:"(?P<extras>[\w=]+)"'
)


def fetch_bundle(session: requests.Session | None = None) -> str:
    session = session or requests.Session()
    session.headers.update({"User-Agent": USER_AGENT})

    page = session.get(LOGIN_PAGE, timeout=30)
    page.raise_for_status()

    match = BUNDLE_RE.search(page.text)
    if not match:
        raise RuntimeError("could not find the bundle.js URL on the login page")

    bundle = session.get(PLAYER_BASE + match.group("path"), timeout=120)
    bundle.raise_for_status()
    return bundle.text


def extract(bundle: str) -> tuple[str, list[str]]:
    """Return (app_id, candidate secrets) from a bundle's source."""
    app_match = APP_ID_RE.search(bundle)
    if not app_match:
        raise RuntimeError("could not find appId in the bundle")
    app_id = app_match.group("app_id")

    secrets: list[str] = []

    # The literal appSecret is sometimes usable, so keep it as a candidate.
    direct = app_match.group("secret")
    if direct:
        secrets.append(direct)

    by_timezone = {
        m.group("name").split("/")[-1].lower(): (m.group("info"), m.group("extras"))
        for m in TIMEZONE_RE.finditer(bundle)
    }

    for seed_match in SEED_RE.finditer(bundle):
        pair = by_timezone.get(seed_match.group("timezone"))
        if not pair:
            continue
        blob = seed_match.group("seed") + pair[0] + pair[1]
        try:
            decoded = base64.b64decode(blob + "=" * (-len(blob) % 4)).decode("utf-8")
        except (ValueError, UnicodeDecodeError):
            continue
        if re.fullmatch(r"[0-9a-f]{32}", decoded):
            secrets.append(decoded)

    # Preserve order, drop duplicates.
    seen: set[str] = set()
    unique = [s for s in secrets if not (s in seen or seen.add(s))]

    if not unique:
        raise RuntimeError("found no usable secrets in the bundle")
    return app_id, unique


def extract_private_key(bundle: str) -> str | None:
    """The OAuth exchange key. Needed to trade an auth code for a token."""
    match = PRIVATE_KEY_RE.search(bundle)
    return match.group("key") if match else None


def refresh(verbose: bool = True) -> tuple[str, list[str]]:
    if verbose:
        print("fetching the Qobuz web player bundle ...", file=sys.stderr)
    app_id, secrets = extract(fetch_bundle())
    if verbose:
        print(f"app_id  : {app_id}", file=sys.stderr)
        print(f"secrets : {len(secrets)} candidates", file=sys.stderr)
    return app_id, secrets
