"""Command line entry point for the offline pipeline."""

from __future__ import annotations

import argparse
import json
import sys

from . import config, db


# ------------------------------------------------------------------- qobuz


def cmd_whoami(args: argparse.Namespace) -> int:
    from .qobuz import QobuzClient

    client = QobuzClient.from_env()
    user = client.login()
    print(f"user id     : {user.get('id')}")
    print(f"email       : {user.get('email')}")
    print(f"display name: {user.get('display_name')}")
    creds = user.get("credential") or {}
    print(f"subscription: {creds.get('label') or creds.get('description') or 'unknown'}")
    return 0


def _update_env(updates: dict[str, str]) -> None:
    """Rewrite .env, preserving every other line and backing up first."""
    env_path = config.REPO_ROOT / ".env"
    lines = env_path.read_text().splitlines() if env_path.is_file() else []

    if lines:
        backup = config.REPO_ROOT / ".env.bak"
        backup.write_text("\n".join(lines) + "\n")
        backup.chmod(0o600)
        print(f"backed up existing .env to {backup.name}", file=sys.stderr)

    out, seen = [], set()
    for line in lines:
        key = line.split("=", 1)[0].strip()
        if key in updates:
            out.append(f"{key}={updates[key]}")
            seen.add(key)
        else:
            out.append(line)
    for key, value in updates.items():
        if key not in seen:
            out.append(f"{key}={value}")

    env_path.write_text("\n".join(out) + "\n")
    env_path.chmod(0o600)


def cmd_login(args: argparse.Namespace) -> int:
    """Browser-based OAuth login; stores the resulting token in .env."""
    from . import bundle, oauth

    source = bundle.fetch_bundle()  # ~9MB, so fetch it once
    app_id, secrets = bundle.extract(source)
    private_key = bundle.extract_private_key(source)

    token = oauth.login(
        app_id=app_id,
        private_key=private_key,
        timeout=args.timeout,
        open_browser=not args.no_browser,
    )

    _update_env(
        {
            "QOBUZ_APP_ID": app_id,
            "QOBUZ_APP_SECRETS": ",".join(secrets),
            "QOBUZ_USER_AUTH_TOKEN": token,
        }
    )
    print(f"logged in; token saved to {config.REPO_ROOT / '.env'}")
    return cmd_whoami(args)


def cmd_refresh_credentials(args: argparse.Namespace) -> int:
    """Re-scrape app_id and signing secrets from the Qobuz web player."""
    from . import bundle

    app_id, secrets = bundle.refresh()
    joined = ",".join(secrets)

    if not args.write:
        print("\nAdd these to your .env:\n")
        print(f"QOBUZ_APP_ID={app_id}")
        print(f"QOBUZ_APP_SECRETS={joined}")
        print("\n(re-run with --write to update the file automatically)")
        return 0

    _update_env({"QOBUZ_APP_ID": app_id, "QOBUZ_APP_SECRETS": joined})
    print(f"updated .env with app_id and {len(secrets)} candidate secrets")
    return 0


def cmd_favorites(args: argparse.Namespace) -> int:
    from .qobuz import QobuzClient

    client = QobuzClient.from_env()
    shown = 0
    for item in client.favorites(args.kind):
        if args.kind == "tracks":
            artist = (item.get("performer") or {}).get("name", "?")
            album = (item.get("album") or {}).get("title", "?")
            print(f"{item['id']:>12}  {artist} - {item.get('title')}  [{album}]")
        elif args.kind == "albums":
            artist = (item.get("artist") or {}).get("name", "?")
            print(f"{item['id']:>12}  {artist} - {item.get('title')}")
        else:
            print(f"{item['id']:>12}  {item.get('name')}")
        shown += 1
        if args.limit and shown >= args.limit:
            break
    print(f"\n{shown} {args.kind} shown", file=sys.stderr)
    return 0


def cmd_crawl(args: argparse.Namespace) -> int:
    from . import crawl as crawl_mod
    from .qobuz import QobuzClient

    client = QobuzClient.from_env(rate_per_sec=args.rate)
    conn = db.connect()

    if not args.no_seed:
        crawl_mod.seed(conn, client)

    stats = crawl_mod.crawl(
        conn, client, max_tracks=args.max_tracks, max_distance=args.max_distance
    )
    print(
        f"expanded {stats['artists_expanded']} artists, {stats['albums_expanded']} albums; "
        f"+{stats['tracks_added']} tracks, {stats['errors']} errors"
    )
    return 0


# ----------------------------------------------------------------- analysis


def cmd_analyse(args: argparse.Namespace) -> int:
    from . import analyse
    from .qobuz import QobuzClient

    client = QobuzClient.from_env(rate_per_sec=args.rate)
    conn = db.connect()
    stats = analyse.run(
        conn,
        client,
        limit=args.limit,
        retry_failed=args.retry_failed,
        cache_gb=args.cache_gb,
        workers=args.workers,
        download_workers=args.download_workers,
    )
    print(f"analysed {stats['done']}/{stats['total']} tracks, {stats['failed']} failed")
    return 0


def cmd_block(args: argparse.Namespace) -> int:
    """Add an artist to the block list, by id or by name."""
    from . import blocklist

    conn = db.connect()
    matches = blocklist.resolve(conn, args.artist)
    if not matches:
        print(f"no artist matching {args.artist!r} in the catalogue.", file=sys.stderr)
        print("Block by numeric id if they have not been crawled yet.", file=sys.stderr)
        return 1

    # Blocking the wrong person is worse than an extra keystroke, so an
    # ambiguous name asks rather than guesses.
    if len(matches) > 1 and not args.all:
        print(f"{len(matches)} artists match {args.artist!r}:", file=sys.stderr)
        for row in matches:
            tracks = row["tracks"] if "tracks" in row.keys() else 0
            print(f"  {row['id']:>10}  {row['name']}  ({tracks} tracks)", file=sys.stderr)
        print("\nRe-run with an id, or --all to block every match.", file=sys.stderr)
        return 1

    for row in matches:
        blocklist.block(conn, row["id"], name=row["name"], reason=args.reason)
        print(f"blocked {row['name']} ({row['id']})")
        if args.purge:
            counts = blocklist.purge(conn, row["id"])
            print(
                f"  purged {counts['tracks']} tracks, {counts['features']} features, "
                f"{counts['albums']} albums"
            )

    if args.purge:
        print("\nRe-run build-space and layout to drop them from the map.")
    else:
        print("\nHidden everywhere from now on; no rebuild needed.")
    return 0


def cmd_unblock(args: argparse.Namespace) -> int:
    from . import blocklist

    conn = db.connect()
    matches = blocklist.resolve(conn, args.artist)
    ids = [row["id"] for row in matches] if matches else []
    if args.artist.isdigit():
        ids = ids or [int(args.artist)]

    lifted = [i for i in ids if blocklist.unblock(conn, i)]
    if not lifted:
        print(f"no block found for {args.artist!r}", file=sys.stderr)
        return 1
    for artist_id in lifted:
        print(f"unblocked {artist_id}")
    print("Purged data does not come back; re-crawl to restore it.")
    return 0


def cmd_blocked(args: argparse.Namespace) -> int:
    from . import blocklist

    rows = blocklist.listing(db.connect())
    if not rows:
        print("no artists blocked")
        return 0
    for row in rows:
        reason = f"  - {row['reason']}" if row["reason"] else ""
        print(f"{row['artist_id']:>10}  {row['name'] or '?'}  "
              f"({row['tracks']} tracks, since {row['blocked_at'][:10]}){reason}")
    return 0


def cmd_build_space(args: argparse.Namespace) -> int:
    from . import space

    weights = json.loads(args.weights) if args.weights else None
    conn = db.connect()
    manifest = space.build(conn, weights=weights)
    print(f"space: {manifest['n_tracks']} tracks x {manifest['n_dims']} dims")
    return 0


def cmd_layout(args: argparse.Namespace) -> int:
    """UMAP projection to 2D, stored for the app's map."""
    import numpy as np

    from . import space

    try:
        import umap
    except ImportError:
        print("umap-learn is required: uv sync --extra layout", file=sys.stderr)
        return 2

    vectors, manifest = space.load()
    reducer = umap.UMAP(
        n_neighbors=args.neighbors, min_dist=args.min_dist, metric="cosine", random_state=42
    )
    coords = reducer.fit_transform(vectors)

    conn = db.connect()
    # Rewrite wholesale rather than upserting: the layout is a pure function
    # of the space, so a track that has left it must not keep its coordinates.
    stale = conn.execute("DELETE FROM layout").rowcount
    conn.executemany(
        "INSERT INTO layout (track_id, x, y) VALUES (?, ?, ?)",
        [
            (int(t), float(x), float(y))
            for t, (x, y) in zip(manifest["track_ids"], np.asarray(coords))
        ],
    )
    conn.commit()
    dropped = stale - len(manifest["track_ids"])
    if dropped > 0:
        print(f"dropped {dropped} stale layout rows", file=sys.stderr)
    print(f"laid out {len(manifest['track_ids'])} tracks")
    return 0


# --------------------------------------------------------------- navigation


def _navigator(args: argparse.Namespace):
    from .paths import Navigator

    weights = json.loads(args.weights) if getattr(args, "weights", None) else None
    return Navigator(db.connect(), weights=weights)


def _print_tracks(tracks: list[dict], show_similarity: bool = False) -> None:
    for n, t in enumerate(tracks, start=1):
        bpm = f"{t['bpm']:>5.1f}" if t.get("bpm") else "    -"
        extra = f"  sim={t['similarity']}" if show_similarity and "similarity" in t else ""
        print(
            f"{n:>3}. [{t['track_id']:>10}] {bpm} bpm  "
            f"{t['artist'][:28]:<28}  {t['title'][:40]:<40}{extra}"
        )


def cmd_neighbours(args: argparse.Namespace) -> int:
    nav = _navigator(args)
    _print_tracks(
        nav.neighbours(args.track, k=args.k, exclude_same_artist=args.exclude_same_artist),
        show_similarity=True,
    )
    return 0


def cmd_path(args: argparse.Namespace) -> int:
    from .paths import Constraints

    nav = _navigator(args)
    constraints = Constraints(
        artist_cooldown=args.artist_cooldown, max_bpm_delta=args.max_bpm_delta
    )
    if args.mode == "graph":
        tracks = nav.graph_path(args.from_track, args.to_track, constraints=constraints)
    else:
        tracks = nav.interpolate(
            args.from_track, args.to_track, steps=args.steps, constraints=constraints
        )
    if not tracks:
        print("no path found", file=sys.stderr)
        return 1
    _print_tracks(tracks)
    return 0


def cmd_drift(args: argparse.Namespace) -> int:
    from .features.clap_ext import ClapExtractor

    nav = _navigator(args)
    clap = ClapExtractor()
    vector = clap.embed_text([args.toward])[0]

    anchors = nav.text_anchors(vector, k=args.anchors)
    print(f"closest to {args.toward!r}:", file=sys.stderr)
    for i in anchors[:3]:
        print(f"    {nav.artist_name[i]} - {nav.title[i]}", file=sys.stderr)
    print(file=sys.stderr)

    _print_tracks(
        nav.drift_to_text(args.from_track, vector, steps=args.steps, k=args.anchors)
    )
    return 0


def cmd_radio(args: argparse.Namespace) -> int:
    nav = _navigator(args)
    _print_tracks(nav.radio(args.from_track, steps=args.steps, temperature=args.temperature))
    return 0


def cmd_evaluate(args: argparse.Namespace) -> int:
    """Space sanity: do tracks from one album land near each other?

    If this metric is poor the space is noise, and no amount of path logic will
    rescue it. Watch it whenever the weights change.
    """
    import numpy as np

    nav = _navigator(args)
    by_album: dict[str, list[int]] = {}
    for i, album in enumerate(nav.album_id):
        if album:
            by_album.setdefault(album, []).append(i)
    groups = {a: idx for a, idx in by_album.items() if len(idx) > 1}
    if not groups:
        print("not enough multi-track albums to evaluate", file=sys.stderr)
        return 1

    ranks, top1 = [], 0
    for indices in groups.values():
        for i in indices:
            sims = nav.unit @ nav.unit[i]
            sims[i] = -np.inf
            order = np.argsort(-sims)
            same = set(indices) - {i}
            rank = next(r for r, j in enumerate(order, start=1) if int(j) in same)
            ranks.append(rank)
            if rank == 1:
                top1 += 1

    ranks_array = np.array(ranks)
    print(f"albums evaluated      : {len(groups)}")
    print(f"tracks evaluated      : {len(ranks)}")
    print(f"same-album is top-1   : {top1}/{len(ranks)} ({100 * top1 / len(ranks):.1f}%)")
    print(f"median rank           : {np.median(ranks_array):.1f}")
    print(f"mean rank             : {ranks_array.mean():.1f}  (of {len(nav.track_ids)} tracks)")
    return 0


# -------------------------------------------------------------------- misc


def cmd_status(args: argparse.Namespace) -> int:
    conn = db.connect()
    for table, n in db.counts(conn).items():
        print(f"{table:<18} {n:>8}")
    if config.SPACE_JSON.is_file():
        manifest = json.loads(config.SPACE_JSON.read_text())
        print(f"{'space':<18} {manifest['n_tracks']} x {manifest['n_dims']} "
              f"(built {manifest['built_at']})")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="two-khz", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    def with_weights(p):
        p.add_argument("--weights", help='JSON block weights, e.g. \'{"tempo": 2.0}\'')
        return p

    p = sub.add_parser("whoami", help="verify credentials and print the account")
    p.set_defaults(func=cmd_whoami)

    p = sub.add_parser("login", help="sign in through the browser and store a token")
    p.add_argument("--timeout", type=int, default=300, help="seconds to wait for redirect")
    p.add_argument("--no-browser", action="store_true", help="print the URL instead of opening it")
    p.set_defaults(func=cmd_login)

    p = sub.add_parser(
        "refresh-credentials", help="re-scrape app_id and secrets from the web player"
    )
    p.add_argument("--write", action="store_true", help="update .env in place")
    p.set_defaults(func=cmd_refresh_credentials)

    p = sub.add_parser("favorites", help="list the account's favourites")
    p.add_argument("--kind", choices=("tracks", "albums", "artists"), default="tracks")
    p.add_argument("--limit", type=int, default=0, help="0 means no limit")
    p.set_defaults(func=cmd_favorites)

    p = sub.add_parser("crawl", help="seed from favourites and expand via similar artists")
    p.add_argument("--max-tracks", type=int, default=5000)
    p.add_argument("--max-distance", type=int, default=2, help="max hops from a favourite")
    p.add_argument("--rate", type=float, default=2.0, help="requests per second")
    p.add_argument("--no-seed", action="store_true", help="skip favourites, resume the frontier")
    p.set_defaults(func=cmd_crawl)

    p = sub.add_parser("analyse", help="download excerpts and extract features")
    p.add_argument("--limit", type=int, default=0, help="0 means every pending track")
    p.add_argument("--retry-failed", action="store_true")
    p.add_argument("--cache-gb", type=float, default=20.0, help="audio cache size cap")
    p.add_argument("--rate", type=float, default=2.0)
    p.add_argument(
        "--workers",
        type=int,
        default=0,
        help="extraction processes; 0 picks one per 4 hardware threads",
    )
    p.add_argument(
        "--download-workers",
        type=int,
        default=0,
        help="excerpt fetch threads; 0 tracks --workers",
    )
    p.set_defaults(func=cmd_analyse)

    p = with_weights(sub.add_parser("build-space", help="assemble vectors from features"))
    p.set_defaults(func=cmd_build_space)

    p = sub.add_parser("block", help="hide an artist everywhere")
    p.add_argument("artist", help="artist id, or part of their name")
    p.add_argument("--reason", help="note to keep with the block")
    p.add_argument("--all", action="store_true", help="block every name match")
    p.add_argument(
        "--purge",
        action="store_true",
        help="also delete their stored tracks, features and albums",
    )
    p.set_defaults(func=cmd_block)

    p = sub.add_parser("unblock", help="lift a block")
    p.add_argument("artist", help="artist id, or part of their name")
    p.set_defaults(func=cmd_unblock)

    p = sub.add_parser("blocked", help="list blocked artists")
    p.set_defaults(func=cmd_blocked)

    p = sub.add_parser("layout", help="UMAP projection to 2D for the map")
    p.add_argument("--neighbors", type=int, default=15)
    p.add_argument("--min-dist", type=float, default=0.1)
    p.set_defaults(func=cmd_layout)

    p = with_weights(sub.add_parser("neighbours", help="nearest tracks to one track"))
    p.add_argument("track", type=int)
    p.add_argument("-k", type=int, default=10)
    p.add_argument("--exclude-same-artist", action="store_true")
    p.set_defaults(func=cmd_neighbours)

    p = with_weights(sub.add_parser("path", help="build a path between two tracks"))
    p.add_argument("--from", dest="from_track", type=int, required=True)
    p.add_argument("--to", dest="to_track", type=int, required=True)
    p.add_argument("--steps", type=int, default=12)
    p.add_argument("--mode", choices=("interpolate", "graph"), default="graph")
    p.add_argument("--artist-cooldown", type=int, default=3)
    p.add_argument("--max-bpm-delta", type=float, default=None)
    p.set_defaults(func=cmd_path)

    p = with_weights(sub.add_parser("drift", help="walk from a track toward a described mood"))
    p.add_argument("--from", dest="from_track", type=int, required=True)
    p.add_argument("--toward", required=True, help='e.g. "darker and slower"')
    p.add_argument("--steps", type=int, default=12)
    p.add_argument("--anchors", type=int, default=5, help="tracks averaged to fix the target")
    p.set_defaults(func=cmd_drift)

    p = with_weights(sub.add_parser("radio", help="stochastic walk from a track"))
    p.add_argument("--from", dest="from_track", type=int, required=True)
    p.add_argument("--steps", type=int, default=20)
    p.add_argument("--temperature", type=float, default=0.3)
    p.set_defaults(func=cmd_radio)

    p = with_weights(sub.add_parser("evaluate", help="space sanity check"))
    p.set_defaults(func=cmd_evaluate)

    p = sub.add_parser("status", help="row counts for the local database")
    p.set_defaults(func=cmd_status)

    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except config.ConfigError as exc:
        print(f"configuration error:\n{exc}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
