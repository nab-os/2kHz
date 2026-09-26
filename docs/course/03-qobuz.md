# 3. Talking to Qobuz

By the end of this chapter you will know where the credentials come from,
how a request is signed, how one rate limit is shared by every part of the
server, why a stream URL can fail with a "signature" error for reasons that
have nothing to do with signatures, and what shapes the app actually sees.

Two files: `server/src/qobuz.rs` (the client, the *only* place credentials
are held) and `app/src/qobuz.rs` (the plain data shapes and parsers, shared
with the app so both sides agree on the wire).

## Credentials: three things, two sources

A Qobuz API call needs:

1. an **app id**: identifies the application;
2. an **app secret**: used only to *sign* `track/getFileUrl`;
3. a **user auth token**: identifies the signed-in user.

Qobuz does not issue app credentials to individuals. Every third-party
client uses the web player's own, which rotate with player releases. And
since April 2026 the API refuses anonymous password logins, so the user
token must come from a browser.

### `Credentials::from_env`

Merges, in increasing priority:

1. `../qobuz_stream/.env` (a sibling project's file, a historical
   convenience),
2. `<env_dir>/.env` (default: repo root; in Docker: `/data`),
3. the process environment.

`QOBUZ_APP_SECRETS` is a comma-separated list, and `QOBUZ_APP_SECRET` is
appended to it. `QOBUZ_PASSWORD` is MD5-hashed unless it already looks like
a 32-hex-digit MD5 (the legacy `user/login` path wants the hash). The
parser, `parse_env`, is a tiny hand-written `.env` reader that strips
quotes and skips comments.

### `login.rs`: scraping the web player

`two-khz-server login` does two things.

**It scrapes the app id and secrets** (`fetch_bundle`, `parse_bundle`):

1. GET `https://play.qobuz.com/login` and find `<script src="/resources/…/bundle.js">`.
2. Download the ~9MB bundle.
3. Regex out `production:{api:{appId:"…",appSecret:"…"`, the app id, and
   sometimes a directly usable secret.
4. The real secrets are obfuscated. Each is split into a **seed**,
   `.initialSeed("…", window.utimezone.<city>)`, plus an **info** and
   **extras** string attached to the matching timezone entry
   `name:"Europe/London",info:"…",extras:"…"`. Concatenate
   seed+info+extras and base64-decode.
5. Keep results that are 32 lowercase hex characters.

`base64_decode` is hand-written, and one detail is load-bearing: **it stops
at the first `=`**. The concatenated blob is the secret, its padding, then
44 characters of decoy; decoding straight through yields junk. The test
`pulls_credentials_out_of_a_bundle` uses a real (September 2026) sample.

**It gets a user token via OAuth** (`browser_login`):

1. Bind a TCP listener on `127.0.0.1:0` (any free port).
2. Open the browser to
   `https://www.qobuz.com/signin/oauth?ext_app_id=<id>&redirect_url=http://localhost:<port>`.
3. `catch_code` is a ~40-line hand-rolled HTTP server: accept connections,
   read the request line, look for `code_autorisation=` (Qobuz's spelling)
   or `code=` in the query string, reply with a small HTML page. Requests
   without a code (a favicon) get a 400 and the loop continues.
4. `exchange_code` trades the code, plus a `privateKey` scraped from the
   bundle, at `oauth/callback` for a token.

`update_env` then rewrites `.env` in place, keeping unrelated lines,
backing the old file up to `.env.bak`, and `chmod 600`-ing both.

`refresh-credentials [--write]` re-runs just the scraping half, for when the
secrets rotate.

The manual fallback (documented in `TOKEN_HELP` and the README) is copying
`localuser.token` out of the web player's local storage.

## Signing

Only `track/getFileUrl` needs a signature. `QobuzClient::sign`:

```
md5( endpoint-without-slashes
   + for each (key, value) in sorted params, except app_id and user_auth_token: key + value
   + unix-timestamp
   + secret )
```

`params` is a `BTreeMap`, so iteration is already sorted. The timestamp and
signature go on the query string as `request_ts` and `request_sig`. Because
the signature includes a timestamp, `request` re-signs on every retry.

## The request loop

`QobuzClient::request(endpoint, params, secret)`:

- builds `GET https://www.qobuz.com/api.json/0.2/<endpoint>` with the params,
  `app_id`, and headers `X-App-Id` and `X-User-Auth-Token`;
- **waits for a rate-limit slot** (`acquire`, below);
- up to 4 attempts:
  - network error → sleep 1, 2, 4s and retry;
  - HTTP 429 or 5xx → sleep `Retry-After` if given, else 2, 4, 8s, retry;
  - any other non-2xx → fail immediately with the first 200 bytes of body;
  - 2xx → parse JSON.

## The shared rate limit

```rust
pub const DEFAULT_RATE_PER_SEC: f64 = 2.0;
const BURST: f64 = 4.0;

#[derive(Clone)]
pub struct RateLimit(Arc<Mutex<TokenBucket>>);
```

A **token bucket**: the bucket holds up to `BURST` tokens and refills at
`rate_per_sec`. Each request takes one. `acquire`:

```rust
let wait = {
    let mut bucket = self.limiter.0.lock()…;
    let elapsed = now - bucket.last;
    bucket.tokens = (bucket.tokens + elapsed * rate).min(BURST);
    bucket.last = now;
    if bucket.tokens >= 1.0 { bucket.tokens -= 1.0; None }
    else {
        let deficit = (1.0 - bucket.tokens) / rate;
        bucket.tokens = 0.0;
        bucket.last = now + deficit;       // ← the trick
        Some(deficit)
    }
};
if let Some(wait) = wait { tokio::time::sleep(wait).await; }
```

The deficit is *claimed under the lock* by pushing `last` into the future,
then slept *outside* the lock. The next caller computes a negative `elapsed`
(no refill) and queues up behind. So concurrent callers form an orderly
line instead of all waking at once and racing. And the lock is a
`std::sync::Mutex` held for nanoseconds, never across an await.

Why share it? Because the budget protects *the account*, and there is one
account. The crawl thread, the analyse stage and every browsing request
build their own `QobuzClient`, but pass the same `RateLimit`
(`QobuzClient::sharing`). So a crawl makes browsing slower, not blocked
(design.md measured searches at 0.2 to 2.4s with a crawl running).

Two limits of this design are worth knowing. The bucket is shared *within
a process*: a stage run from the command line while `serve` is running has
its own bucket, so the account can see both budgets at once (chapter 19).
And `rate_per_sec` lives on the client, not the bucket, so two clients
sharing a bucket with different rates would each refill it at their own
rate; in practice only the CLI `crawl --rate` changes it.

## Pagination

`paginate(endpoint, params, container, cap)` walks a list endpoint 100 at a
time using `limit`/`offset`, until `cap` items, a short page, or the
reported `total`. `playlist_tracks` and `artist_albums_inner` are
hand-written variants because their items are nested under a different key.

## Stream URLs and the "signature" that isn't

`file_url(track_id, format_id)`:

- tries the known-good secret if there is one, else every configured
  secret, calling `track/getFileUrl` with `intent=stream`;
- remembers the first secret that returns a `url` (`working_secret`);
- if none work, the error depends on history:
  - no secret has ever worked → "no configured app secret produced a valid
    signature (it may have rotated). Run refresh-credentials";
  - a secret *has* worked before → "track N is not streamable (likely
    delisted)".

That second branch exists because **Qobuz reports a delisted track as a
signature error**. Once you know the secret is good, blaming the track is
the right call.

Format ids (`app/src/qobuz.rs`): `5` = MP3 320, `6` = FLAC 16/44,
`7` = FLAC hi-res. Analysis always uses 5 (chapter 5); playback uses
whatever the player's quality menu says.

## Other calls

| method | endpoint | used by |
|---|---|---|
| `user` | `user/get` | `whoami` |
| `search` | `catalog/search` | the search box |
| `favorites_raw` | `favorite/getUserFavorites` | crawl seeding, library views |
| `favorite_write` | `favorite/create` / `favorite/delete` | the heart / follow buttons |
| `user_playlists`, `playlist_tracks` | `playlist/getUserPlaylists`, `playlist/get` | library |
| `album_raw`, `album_tracks` | `album/get` | crawl, album views |
| `artist_albums_*` | `artist/get?extra=albums` | crawl, artist views |
| `similar_artists_*` | `artist/getSimilarArtists` | crawl expansion, artist views |
| `export_playlist` | `playlist/create` + `playlist/addTracks` | exporting a generated sequence |

The code comment on `favorite_write` is candid that `favorite/create` and
`favorite/delete` have not been confirmed against a live account, they are
inferred from the read side.

`login()` is a no-op when a token is present, and otherwise attempts the
legacy `user/login` with email + MD5 password (now blocked by Qobuz, kept
for completeness), failing with `TOKEN_HELP`.

## The shared shapes: `app/src/qobuz.rs`

The server never sends raw Qobuz JSON to clients. It parses into flat
structs that both crates share:

- **`RemoteTrack`**: id, title (with `version` appended in parentheses:
  "Song (Live)"), artist + id, album + id, duration, `streamable`, `hires`,
  `image`, `isrc`, `released`, `performers`.
- **`RemoteAlbum`**, **`RemoteArtist`**, **`RemotePlaylist`**, and
  **`SearchResults`**.

`RemoteTrack::parse(value, context)` is worth reading closely. Tracks nested
inside an album omit the album and often the performer, so the caller passes
the parent album as `context` and missing fields are filled from it. The
artist is taken from `performer`, else `artist`, else the album's artist.
A missing `streamable` means "assume playable".

### Images

`image(value)` handles Qobuz's three shapes: a plain URL string; a bag of
named sizes (albums use `thumbnail/small/large`, artists
`small/medium/large/extralarge/mega`) searched in `IMAGE_SIZES` order; or a
newer `images.portrait.hash` from which a URL is assembled.

`cover_url(album_id)` *guesses* a cover URL from an album id: Qobuz files
covers at `covers/<last2>/<prev2>/<id>_230.jpg`. Space tracks carry only an
album id, so this is the only way generated sequences get art. A wrong guess
costs nothing, because covers are drawn as CSS backgrounds that simply stay
blank on a 404 (chapter 16).

### Identity

```rust
pub fn identity(&self) -> TrackIdentity {
    match self.isrc.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(isrc) => TrackIdentity::Recording(isrc.to_ascii_uppercase()),
        None => TrackIdentity::Entry(self.id),
    }
}
```

Two entries are "the same music" if they share an ISRC (case-insensitive,
blank treated as absent); an untagged track is only ever a duplicate of
itself. The comment states the safety argument: a missed duplicate is a
nuisance, a wrongly dropped track is a bug. The queue uses this (ch. 15).

### `own_releases`

`artist/get?extra=albums` returns every album an artist is *credited* on,
which for a much-covered band is mostly other people's cover albums
(measured: Rage Against the Machine, 42 returned, 13 theirs). `own_releases`
keeps only albums whose main artist is this one (or unreported). It is
applied to the typed result for the UI, and again on the client
(`Remote::artist_albums`) in case the server is older. The *raw* variant
used by the crawl stays unfiltered, because a credited album is still a
fine place to expand the frontier to.

## Check yourself

1. Which Qobuz calls need the app secret?
2. Why does `acquire` set `bucket.last` into the future?
3. A stream URL request fails with a signature error. Under what condition
   does the client blame the track instead of the credentials?
4. Why does `base64_decode` stop at the first `=`?
5. Why is `own_releases` applied on both the server and the client?

## Exercises

1. Write a unit test for `QobuzClient::sign` with a fixed timestamp (you
   will need to factor the timestamp out). Check that `app_id` and
   `user_auth_token` are excluded from the hash.
2. Simulate eight concurrent `acquire()` calls on a fresh bucket in a test
   with `tokio::time::pause()`. Confirm the first four are immediate (the
   burst) and the rest are spaced 0.5s apart.
3. Run `two-khz-server refresh-credentials` (no `--write`) and compare the
   secrets it finds with your `.env`.
