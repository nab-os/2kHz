"""Paths and credentials.

Credentials are read from the repo's own .env, falling back to the qobuz_stream
repo for app_id/app_secret so they only live in one place.
"""

from __future__ import annotations

import os
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

# Overridable so tests and experiments cannot clobber a real corpus.
DATA_DIR = Path(os.environ.get("TWO_KHZ_DATA_DIR") or REPO_ROOT / "data")
CACHE_DIR = Path(os.environ.get("TWO_KHZ_CACHE_DIR") or REPO_ROOT / "cache" / "audio")
DB_PATH = DATA_DIR / "two_khz.db"
SPACE_BIN = DATA_DIR / "space.bin"
SPACE_JSON = DATA_DIR / "space.json"
TOKEN_CACHE = DATA_DIR / ".token.json"

# Where app_id/app_secret already live, from the existing qobuz_stream project.
FALLBACK_ENV = REPO_ROOT.parent / "qobuz_stream" / ".env"


def _parse_env(path: Path) -> dict[str, str]:
    """Minimal .env reader: KEY=VALUE per line, # comments, optional quotes."""
    out: dict[str, str] = {}
    if not path.is_file():
        return out
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        out[key.strip()] = value.strip().strip("'\"")
    return out


def load_env() -> dict[str, str]:
    """Merge, in increasing priority: fallback .env, repo .env, real environment."""
    merged = _parse_env(FALLBACK_ENV)
    merged.update(_parse_env(REPO_ROOT / ".env"))
    for key in (
        "QOBUZ_APP_ID",
        "QOBUZ_APP_SECRET",
        "QOBUZ_APP_SECRETS",
        "QOBUZ_EMAIL",
        "QOBUZ_PASSWORD",
        "QOBUZ_USER_AUTH_TOKEN",
    ):
        if os.environ.get(key):
            merged[key] = os.environ[key]
    return merged


def ensure_dirs() -> None:
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    CACHE_DIR.mkdir(parents=True, exist_ok=True)


class ConfigError(RuntimeError):
    """Raised when required credentials are missing, with a fix-it message."""
