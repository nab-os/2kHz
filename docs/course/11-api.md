# 11. The HTTP API

By the end of this chapter you will know every route, how a request is
authenticated and scoped, how a client keeps its copy of the space in sync,
how the pipeline log is streamed, and how errors travel back to the app.

Files: `server/src/routes.rs`, `server/src/auth.rs`, `server/src/main.rs`
(`Failure`), and the client half in `app/src/backend/remote.rs`.

## Thin on purpose

The header of `routes.rs`:

> Thin on purpose: every handler is a scope check and a call into `Hub`.
> There must never be a second implementation here.

A typical handler:

```rust
async fn search(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<SearchQuery>,
) -> Reply<SearchResults> {
    Ok(Json(state.hub.search(&query.q, query.limit).await?))
}
```

Axum *extractors* do the work: `State` gives the shared state, `PlayAuth`
authenticates (below), `Query`/`Path`/`Json` parse the request. The `?` turns
an `anyhow::Error` into a `Failure` (below).

## The routes

| method & path | scope | does |
|---|---|---|
| `GET /api/search?q=&limit=50` | play | Qobuz catalogue search |
| `GET /api/favourites/{tracks,albums,artists}?cap=500` | play | your favourites |
| `POST/DELETE /api/favourites/{kind}/{id}` | play | like / unlike on Qobuz |
| `GET /api/playlists?cap=`, `POST /api/playlists` | play | list; export a sequence as a playlist |
| `GET /api/playlists/{id}` | play | a playlist's tracks |
| `GET /api/albums/{id}` | play | an album's tracklist |
| `POST /api/albums/{id}/fetch` | play | pull the tracklist into the catalogue |
| `GET /api/artists/{id}/albums` | play | discography (own releases) |
| `GET /api/artists/{id}/similar` | play | Qobuz's similar artists |
| `POST /api/artists/{id}/fetch` | play | queue a discography in the frontier |
| `GET /api/tracks/{id}/url?format=5` | play | mint a signed stream URL |
| `POST /api/embed` `{phrase}` | play | CLAP text embedding (512 floats) |
| `GET /api/embed/available` | play | is the text tower present? |
| `GET/POST /api/blocked`, `DELETE /api/blocked/{id}` | play | hide / unhide artists |
| `GET /api/corpus` | play | counts for the pipeline view |
| `GET /api/crawl` | play | crawl status |
| `POST /api/crawl/start` `{max_distance}`, `/stop` | **pipeline** | control the crawl |
| `GET /api/pipeline` | play | running stage, queue, generation |
| `POST /api/pipeline/start` `{stage}`, `/start-full`, `/stop` | **pipeline** | control the stages |
| `GET /api/pipeline/log` | play | **SSE** stream of the stage log |
| `GET /api/sync/manifest` | play | sizes and md5s of the synced files |
| `GET /api/sync/{name}` | play | download one synced file |
| `GET/POST /api/devices`, `DELETE /api/devices/{id}` | **pipeline** | list, pair, revoke devices |
| `GET /api/health` | none | liveness for Docker/compose |

The comment on scopes: "Bounded gestures are `play`, unbounded jobs are
`pipeline`." Fetching one album is bounded; starting a crawl is not. Hiding
an artist is `play` because it only filters and is reversible; `block
--purge`, which deletes rows, is not exposed over HTTP at all. Liking a
track is `play` because it changes the Qobuz account's favourites, not the
catalogue.

## Authentication: `auth.rs`

### Tokens

One user, several devices, so **one token per device** rather than a
password, a password could not be revoked one phone at a time.

`AuthStore::issue(name, scope)`:

- 32 random bytes → 64 hex characters;
- stores `sha256(token)` in `devices.token_hash`, with name, scope, created
  time;
- returns the token **once** in a `PairingGrant`. Only the hash is kept, so
  a lost token means re-pairing.

`AuthStore::verify(token)` hashes the presented token and looks it up, then
updates `last_seen`. The comment notes the comparison is SQLite's, not
constant-time, fine for a 256-bit random token, which cannot be guessed a
byte at a time; it would not be fine for a password.

### Scopes

```rust
pub enum Scope { Play, Pipeline }
impl Scope {
    pub fn covers(self, needed: Scope) -> bool {
        self == needed || (self == Scope::Pipeline && needed == Scope::Play)
    }
}
```

`pipeline` implies `play`. Even `play` needs a token: a stream URL is minted
against the user's account, so handing those out is sharing it.

### Extractors

```rust
pub struct PlayAuth(pub Device);
pub struct PipelineAuth(pub Device);

impl FromRequestParts<AppState> for PlayAuth {
    type Rejection = Failure;
    async fn from_request_parts(parts, state) -> Result<Self, Failure> {
        authorise(parts, state, Scope::Play).map(PlayAuth)
    }
}
```

`authorise` reads `Authorization: Bearer <token>`, verifies it, and checks
the scope, returning 401 (no/unknown token) or 403 (wrong scope) with a
helpful message. Because every handler takes one of these as an argument,
**forgetting auth is a compile-visible omission** rather than a missing
middleware line. The one deliberate exception is `/api/health`, which says
nothing except that something is listening.

### Pairing paths

- CLI: `two-khz-server pair --name phone --scope play` prints the token and
  the two env vars to set.
- API: `POST /api/devices` (pipeline scope), the pipeline view's
  **Devices** panel. A `play` phone cannot mint itself a promotion.

## Errors: `Failure`

```rust
pub struct Failure { status: StatusCode, message: String }

impl From<anyhow::Error> for Failure {           // any `?` becomes a 500
    fn from(err) -> Self { Self { status: 500, message: format!("{err:#}") } }
}
impl IntoResponse for Failure {                  // {"message": "..."}
    fn into_response(self) -> Response { (self.status, Json(ApiError { message })).into_response() }
}
```

`{err:#}` is anyhow's alternate format: the whole context chain on one line.
The comment is explicit that this is **deliberately not sanitised**, a
single-user system behind a VPN, where a real message beats sending someone
to the logs on another machine.

On the client, `Remote::check` turns a non-2xx back into an error:

- 401 → "the server rejected this device's token";
- 403 → "this device is not paired for that";
- **404 with an empty body** → "the server does not have this feature yet,
  it is older than this app". An `ApiError` 404 comes from a handler; an
  *empty* one is axum finding no route at all, which happens when the app is
  newer than the server. Client and server are deployed separately and
  drift, so this is worth saying;
- otherwise "server returned {status}: {message}".

## Sync

```rust
const SYNCED: [&str; 4] = ["space.bin", "space.json", "semantic_pca.bin", "catalog.db"];
```

`GET /api/sync/manifest` reads each file that exists and returns its name,
size and **md5 digest** (`two_khz::api::digest`), plus the current
`generation`. A missing file is skipped, not an error, a corpus without a
built space simply has nothing to sync yet.

`GET /api/sync/{name}` serves one file as `application/octet-stream`. It
checks the name against the allow-list rather than sanitising a path: "the
set is fixed and small, so there is no reason to accept a path at all."

md5 because it is already a dependency (request signing), and this is a
cache key, not a security boundary.

On the client, `Remote::sync_space`:

1. fetch the manifest;
2. for each file, compute the local file's md5; skip if equal;
3. otherwise download it, write `<name>.partial`, rename over the target;
4. return whether anything changed.

Comparing digests, not just `generation`, covers a server restart (which
resets `generation` to 0) and a client that was offline for several
rebuilds.

Two costs to be aware of: the manifest handler reads every synced file into
memory to hash it, on every request (tens of MB for `catalog.db`); and the
client hashes its local copies each time too. Both are fine at current sizes
and sync frequency, and both are places to cache if they ever are not.

## Streaming the log: SSE

`GET /api/pipeline/log` returns **Server-Sent Events**: a long-lived HTTP
response with `Content-Type: text/event-stream`, where each event is
`data: <line>\n\n`.

```rust
async fn pipeline_log(State(state), _: PlayAuth) -> impl IntoResponse {
    let (sender, receiver) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move {
        let mut cursor = 0u64;
        loop {
            let slice = hub.pipeline_log_since(cursor).await?;
            for line in slice.lines { if sender.send(Ok(Event::default().data(line))).await.is_err() { return; } }
            cursor = slice.cursor;
            tokio::time::sleep(LOG_POLL).await;          // 200ms
        }
    });
    Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::default())
}
```

- The cursor starts at 0, so a new connection gets the **retained buffer
  first** (up to 400 lines), then new lines.
- The task polls the in-memory buffer every 200ms, one memory read, not a
  request.
- When the client disconnects, `send` fails and the task ends.
- `keep_alive` sends comment frames during silence (layout can be quiet for
  minutes) so proxies do not time the connection out.

On the client, `Remote::ensure_streaming` starts one reconnecting task for
the life of the process. It reads the byte stream, splits frames on blank
lines, strips `data:`, and pushes each line into the client's *own*
`LogBuffer`. The UI reads that local buffer (`pipeline_log` returns
`log.all()`). On a dropped connection it logs "log stream lost" and retries
after 3s. On a non-2xx answer (a revoked token, say) it logs "log stream
refused", clears the `streaming` flag and ends, and because the pipeline
view calls `pipeline_log` every 200ms, the next call starts a fresh attempt.
So a refused stream is retried at the poll rate, not the 3s backoff (see
chapter 19).

"Clear" in the UI clears only the local mirror, the server's buffer is
shared by every device, and one phone clearing its view must not blank the
desktop's.

## A note on transport security

The server speaks plain HTTP and binds loopback by default. Tokens and
signed stream URLs are credentials, so anything beyond loopback belongs
behind WireGuard/Tailscale or a TLS proxy. In Docker the bind must be
`0.0.0.0` to cross the container boundary, so compose restricts the
*published* port to `127.0.0.1` instead.

## Check yourself

1. What makes it hard to accidentally add an unauthenticated route?
2. Why is starting a crawl `pipeline` scope but fetching an album `play`?
3. What does an empty-bodied 404 tell the client?
4. Why does sync compare md5 digests instead of trusting `generation`?
5. What stops the SSE task when the phone goes to sleep?

## Exercises

1. `curl` the whole flow by hand: pair a device, then
   `curl -H "Authorization: Bearer $T" localhost:7700/api/sync/manifest`, and
   `curl -N … /api/pipeline/log` in another terminal while running a stage.
2. Add a `GET /api/version` route returning the server's crate version, and
   make the app's Settings screen show it.
3. Make the manifest handler cache digests keyed on `(name, mtime, size)`.
