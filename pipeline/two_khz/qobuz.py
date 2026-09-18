"""Qobuz API client (unofficial v0.2 endpoints).

Only what the pipeline needs: login, favourites, catalog metadata, similar
artists, and signed file URLs. The Rust app has its own client for playback and
playlist writes; the two barely overlap.
"""

from __future__ import annotations

import hashlib
import json
import threading
import time
from collections.abc import Iterator
from typing import Any

import requests

from . import config

API_BASE = "https://www.qobuz.com/api.json/0.2"

# Qobuz format ids. 5 is MP3 320, which is what we analyse: ~10x less bandwidth
# than FLAC and acoustically irrelevant once Essentia/CLAP resample to 16-44.1kHz.
FORMAT_MP3_320 = 5
FORMAT_FLAC_CD = 6
FORMAT_FLAC_HIRES = 7


class AudioUnavailable(RuntimeError):
    """A track exists in the catalogue listing but cannot be streamed.

    Usually means it has been delisted: Qobuz reports this as a signature
    error on track/getFileUrl, while track/get returns 404.
    """


class QobuzError(RuntimeError):
    def __init__(self, status: int, endpoint: str, body: str):
        super().__init__(f"{endpoint} -> HTTP {status}: {body[:200]}")
        self.status = status
        self.endpoint = endpoint


class _RateLimiter:
    """Token bucket. Keeps the crawl polite enough to stay out of 429 territory.

    Thread-safe, because the analyse stage fetches excerpts from a pool of
    threads sharing one client: the whole point of the limit is that it is
    global, so it has to survive being called concurrently. The sleep happens
    outside the lock, otherwise waiting threads would serialise behind it and
    the effective rate would collapse to one request per sleep.
    """

    def __init__(self, rate_per_sec: float = 2.0, burst: int = 4):
        self.rate = rate_per_sec
        self.burst = burst
        self._tokens = float(burst)
        self._last = time.monotonic()
        self._lock = threading.Lock()

    def acquire(self) -> None:
        with self._lock:
            now = time.monotonic()
            self._tokens = min(self.burst, self._tokens + (now - self._last) * self.rate)
            self._last = now
            if self._tokens >= 1.0:
                self._tokens -= 1.0
                return
            # Claim the deficit now and pay for it outside the lock, so the
            # next caller queues behind this slot rather than racing for it.
            wait = (1.0 - self._tokens) / self.rate
            self._tokens = 0.0
            self._last = now + wait
        time.sleep(wait)


class QobuzClient:
    def __init__(
        self,
        app_id: str,
        secrets: list[str],
        email: str | None = None,
        password_md5: str | None = None,
        auth_token: str | None = None,
        rate_per_sec: float = 2.0,
    ):
        if not app_id:
            raise config.ConfigError("QOBUZ_APP_ID is missing")
        self.app_id = app_id
        self.secrets = [s for s in secrets if s]
        self.email = email
        self.password_md5 = password_md5
        self.auth_token = auth_token
        self.user: dict[str, Any] | None = None
        self._working_secret: str | None = None
        self._limiter = _RateLimiter(rate_per_sec)
        self._session = requests.Session()
        self._session.headers.update({"X-App-Id": app_id, "User-Agent": "two_khz/0.1"})

    # ---------------------------------------------------------------- factory

    @classmethod
    def from_env(cls, rate_per_sec: float = 2.0) -> QobuzClient:
        env = config.load_env()
        secrets = []
        if env.get("QOBUZ_APP_SECRETS"):
            secrets += [s.strip() for s in env["QOBUZ_APP_SECRETS"].split(",")]
        if env.get("QOBUZ_APP_SECRET"):
            secrets.append(env["QOBUZ_APP_SECRET"])

        if not env.get("QOBUZ_APP_ID"):
            raise config.ConfigError(
                "QOBUZ_APP_ID not found. Checked "
                f"{config.REPO_ROOT / '.env'} and {config.FALLBACK_ENV}."
            )

        password_md5 = None
        if env.get("QOBUZ_PASSWORD"):
            pw = env["QOBUZ_PASSWORD"]
            # Accept an already-hashed password so the plaintext need not be stored.
            is_md5 = len(pw) == 32 and all(c in "0123456789abcdef" for c in pw.lower())
            password_md5 = pw.lower() if is_md5 else hashlib.md5(pw.encode()).hexdigest()

        client = cls(
            app_id=env["QOBUZ_APP_ID"],
            secrets=secrets,
            email=env.get("QOBUZ_EMAIL"),
            password_md5=password_md5,
            auth_token=env.get("QOBUZ_USER_AUTH_TOKEN"),
            rate_per_sec=rate_per_sec,
        )
        client._load_cached_token()
        return client

    # ------------------------------------------------------------------ auth

    def _load_cached_token(self) -> None:
        if self.auth_token or not config.TOKEN_CACHE.is_file():
            return
        try:
            cached = json.loads(config.TOKEN_CACHE.read_text())
        except (json.JSONDecodeError, OSError):
            return
        # Only reuse a token minted for the same account.
        if cached.get("email") == self.email:
            self.auth_token = cached.get("auth_token")
            self.user = cached.get("user")

    def _save_cached_token(self) -> None:
        config.ensure_dirs()
        config.TOKEN_CACHE.write_text(
            json.dumps({"email": self.email, "auth_token": self.auth_token, "user": self.user})
        )
        config.TOKEN_CACHE.chmod(0o600)

    TOKEN_HELP = (
        "A user auth token is required. Qobuz's API no longer serves anonymous\n"
        "requests, and programmatic user/login is blocked at their edge, so the\n"
        "token has to come from a logged-in browser session:\n"
        "\n"
        "  1. Log in at https://play.qobuz.com/ \n"
        "  2. Open devtools -> Application -> Local Storage -> play.qobuz.com\n"
        "  3. Copy the value of the `localuser.token` key\n"
        "\n"
        f"Then add it to {config.REPO_ROOT / '.env'}:\n"
        "  QOBUZ_USER_AUTH_TOKEN=<the token>"
    )

    def login(self, force: bool = False) -> dict[str, Any]:
        """Establish a usable session. Returns the user object when known.

        Prefers a supplied token. Falls back to email/password, which Qobuz
        currently rejects at the edge but is kept in case that changes back.
        """
        if self.auth_token and not force:
            if self.user is None:
                self.user = self._fetch_user()
            return self.user or {}

        if self.email and self.password_md5:
            try:
                data = self._request(
                    "user/login",
                    {"email": self.email, "password": self.password_md5},
                    authed=False,
                )
                self.auth_token = data["user_auth_token"]
                self.user = data.get("user", {})
                self._save_cached_token()
                return self.user
            except QobuzError as exc:
                raise config.ConfigError(
                    f"email/password login failed ({exc}).\n\n{self.TOKEN_HELP}"
                ) from exc

        raise config.ConfigError(self.TOKEN_HELP)

    def _fetch_user(self) -> dict[str, Any]:
        """Confirm the token works and pick up the account details."""
        try:
            data = self._request("user/get", {})
        except QobuzError as exc:
            if exc.status in (401, 403):
                raise config.ConfigError(
                    f"the configured QOBUZ_USER_AUTH_TOKEN was rejected ({exc}).\n"
                    "Tokens expire when you log out of the web player.\n\n"
                    f"{self.TOKEN_HELP}"
                ) from exc
            raise
        user = data.get("user") if isinstance(data.get("user"), dict) else data
        self.user = user
        self._save_cached_token()
        return user

    def _auth_headers(self) -> dict[str, str]:
        return {"X-User-Auth-Token": self.auth_token} if self.auth_token else {}

    # -------------------------------------------------------------- requests

    def _sign(self, endpoint: str, params: dict[str, Any], secret: str) -> dict[str, Any]:
        """Qobuz request signature: md5(endpoint + sorted k+v pairs + ts + secret)."""
        ts = int(time.time())
        to_hash = endpoint.replace("/", "")
        for key in sorted(params):
            if key in ("app_id", "user_auth_token"):
                continue
            to_hash += f"{key}{params[key]}"
        to_hash += f"{ts}{secret}"
        signed = dict(params)
        signed["request_ts"] = ts
        signed["request_sig"] = hashlib.md5(to_hash.encode()).hexdigest()
        return signed

    def _request(
        self,
        endpoint: str,
        params: dict[str, Any] | None = None,
        authed: bool = True,
        secret: str | None = None,
        max_retries: int = 4,
    ) -> dict[str, Any]:
        params = dict(params or {})
        params["app_id"] = self.app_id
        if secret:
            params = self._sign(endpoint, params, secret)

        url = f"{API_BASE}/{endpoint}"
        headers = self._auth_headers() if authed else {}

        for attempt in range(max_retries):
            self._limiter.acquire()
            try:
                resp = self._session.get(url, params=params, headers=headers, timeout=30)
            except requests.RequestException as exc:
                if attempt == max_retries - 1:
                    raise QobuzError(0, endpoint, str(exc)) from exc
                time.sleep(2**attempt)
                continue

            if resp.status_code == 200:
                return resp.json()

            # Rate limited or transient server error: back off and retry.
            if resp.status_code == 429 or resp.status_code >= 500:
                if attempt == max_retries - 1:
                    raise QobuzError(resp.status_code, endpoint, resp.text)
                retry_after = resp.headers.get("Retry-After")
                time.sleep(float(retry_after) if retry_after else 2 ** (attempt + 1))
                continue

            # Token expired: re-login once, then retry.
            if resp.status_code == 401 and authed and attempt == 0 and self.email:
                self.login(force=True)
                headers = self._auth_headers()
                continue

            raise QobuzError(resp.status_code, endpoint, resp.text)

        raise QobuzError(0, endpoint, "exhausted retries")

    def _paginate(
        self, endpoint: str, params: dict[str, Any], container: str, page_size: int = 100
    ) -> Iterator[dict[str, Any]]:
        """Yield every item of a paginated list endpoint."""
        offset = 0
        while True:
            page = self._request(endpoint, {**params, "limit": page_size, "offset": offset})
            block = page.get(container) or {}
            items = block.get("items") or []
            yield from items
            offset += len(items)
            total = block.get("total", 0)
            if not items or offset >= total:
                return

    # ------------------------------------------------------------- endpoints

    def favorites(self, kind: str, page_size: int = 100) -> Iterator[dict[str, Any]]:
        """kind is 'tracks', 'albums' or 'artists'."""
        if kind not in ("tracks", "albums", "artists"):
            raise ValueError(f"unknown favourite kind: {kind}")
        self.login()
        yield from self._paginate(
            "favorite/getUserFavorites", {"type": kind}, kind, page_size=page_size
        )

    def track(self, track_id: int) -> dict[str, Any]:
        return self._request("track/get", {"track_id": track_id})

    def album(self, album_id: str) -> dict[str, Any]:
        return self._request("album/get", {"album_id": album_id})

    def artist(self, artist_id: int) -> dict[str, Any]:
        return self._request("artist/get", {"artist_id": artist_id})

    def similar_artists(self, artist_id: int, limit: int = 50) -> list[dict[str, Any]]:
        data = self._request(
            "artist/getSimilarArtists", {"artist_id": artist_id, "limit": limit}
        )
        return (data.get("artists") or {}).get("items") or []

    def artist_albums(self, artist_id: int, page_size: int = 100) -> Iterator[dict[str, Any]]:
        """Albums for an artist, via artist/get with the albums extra."""
        offset = 0
        while True:
            page = self._request(
                "artist/get",
                {
                    "artist_id": artist_id,
                    "extra": "albums",
                    "limit": page_size,
                    "offset": offset,
                },
            )
            block = page.get("albums") or {}
            items = block.get("items") or []
            yield from items
            offset += len(items)
            if not items or offset >= block.get("total", 0):
                return

    def file_url(self, track_id: int, format_id: int = FORMAT_MP3_320) -> dict[str, Any]:
        """Signed stream URL. Tries each known secret until one is accepted."""
        self.login()
        params = {"track_id": track_id, "format_id": format_id, "intent": "stream"}

        candidates = [self._working_secret] if self._working_secret else list(self.secrets)
        if not candidates:
            raise config.ConfigError("No QOBUZ_APP_SECRET available for signing requests")

        last_error: Exception | None = None
        for secret in candidates:
            try:
                data = self._request("track/getFileUrl", params, secret=secret)
            except QobuzError as exc:
                # A bad secret and a delisted track both show up as a
                # 400-range rejection, so only blame the secret while hunting.
                if exc.status in (400, 401, 403) and self._working_secret is None:
                    last_error = exc
                    continue
                raise AudioUnavailable(
                    f"track {track_id} is not streamable ({exc}). "
                    "Qobuz reports a signature error for delisted tracks; "
                    "check track/get, which returns 404 for those."
                ) from exc
            if data.get("url"):
                self._working_secret = secret
                return data
            last_error = QobuzError(200, "track/getFileUrl", json.dumps(data))

        raise config.ConfigError(
            "None of the configured app secrets produced a valid signature. "
            "The secret may have rotated; re-scrape it with "
            "`two_khz refresh-credentials --write`.\n"
            f"Last error: {last_error}"
        )
