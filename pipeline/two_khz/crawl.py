"""Catalog crawler.

Seeds from the account's favourites at seed_distance 0, then expands outward
through artist/getSimilarArtists. The frontier lives in SQLite, so the crawl is
interruptible and resumable at any point.
"""

from __future__ import annotations

import json
import sqlite3
import sys
from datetime import datetime, timezone
from typing import Any

from . import blocklist
from .qobuz import QobuzClient, QobuzError


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


# ------------------------------------------------------------------ upserts
# COALESCE keeps stored values when a sparser payload for the same entity
# arrives, a stub artist inside a track versus a full artist/get.


def upsert_artist(conn: sqlite3.Connection, artist: dict[str, Any]) -> int | None:
    if not artist or artist.get("id") is None:
        return None
    conn.execute(
        """
        INSERT INTO artists (id, name, qobuz_json) VALUES (?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            name       = excluded.name,
            qobuz_json = COALESCE(excluded.qobuz_json, artists.qobuz_json)
        """,
        (artist["id"], artist.get("name") or "Unknown Artist", json.dumps(artist)),
    )
    return int(artist["id"])


def upsert_album(conn: sqlite3.Connection, album: dict[str, Any]) -> str | None:
    if not album or album.get("id") is None:
        return None
    artist_id = upsert_artist(conn, album.get("artist") or {})
    genre = (album.get("genre") or {}).get("name")
    label = (album.get("label") or {}).get("name")
    conn.execute(
        """
        INSERT INTO albums (id, artist_id, title, release_date, label, genre, qobuz_json)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            artist_id    = COALESCE(excluded.artist_id, albums.artist_id),
            title        = excluded.title,
            release_date = COALESCE(excluded.release_date, albums.release_date),
            label        = COALESCE(excluded.label, albums.label),
            genre        = COALESCE(excluded.genre, albums.genre),
            qobuz_json   = COALESCE(excluded.qobuz_json, albums.qobuz_json)
        """,
        (
            str(album["id"]),
            artist_id,
            album.get("title") or "Unknown Album",
            album.get("release_date_original") or album.get("released_at"),
            label,
            genre,
            json.dumps(album),
        ),
    )
    return str(album["id"])


def upsert_track(
    conn: sqlite3.Connection,
    track: dict[str, Any],
    album_id: str | None = None,
    artist_id: int | None = None,
    seed_distance: int = 0,
) -> int | None:
    if not track or track.get("id") is None:
        return None

    if album_id is None and track.get("album"):
        album_id = upsert_album(conn, track["album"])
    if artist_id is None:
        performer = track.get("performer") or track.get("artist") or {}
        artist_id = upsert_artist(conn, performer)

    conn.execute(
        """
        INSERT INTO tracks (id, album_id, artist_id, title, duration, isrc,
                            qobuz_json, seed_distance)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            album_id      = COALESCE(excluded.album_id, tracks.album_id),
            artist_id     = COALESCE(excluded.artist_id, tracks.artist_id),
            title         = excluded.title,
            duration      = COALESCE(excluded.duration, tracks.duration),
            isrc          = COALESCE(excluded.isrc, tracks.isrc),
            qobuz_json    = COALESCE(excluded.qobuz_json, tracks.qobuz_json),
            -- Keep the shortest known distance to a favourite.
            seed_distance = MIN(tracks.seed_distance, excluded.seed_distance)
        """,
        (
            track["id"],
            album_id,
            artist_id,
            track.get("title") or "Unknown Track",
            track.get("duration"),
            track.get("isrc"),
            json.dumps(track),
            seed_distance,
        ),
    )
    return int(track["id"])


# ----------------------------------------------------------------- frontier


def enqueue(conn: sqlite3.Connection, kind: str, ref_id: str | int, priority: int) -> None:
    """Add to the frontier, keeping the lowest priority (closest to a seed)."""
    conn.execute(
        """
        INSERT INTO frontier (kind, ref_id, priority, state) VALUES (?, ?, ?, 'pending')
        ON CONFLICT(kind, ref_id) DO UPDATE SET
            priority = MIN(frontier.priority, excluded.priority)
        """,
        (kind, str(ref_id), priority),
    )


def _mark(conn: sqlite3.Connection, kind: str, ref_id: str, state: str) -> None:
    conn.execute(
        "UPDATE frontier SET state = ? WHERE kind = ? AND ref_id = ?", (state, kind, ref_id)
    )


def _next_batch(conn: sqlite3.Connection, limit: int = 32) -> list[sqlite3.Row]:
    return conn.execute(
        """
        SELECT kind, ref_id, priority FROM frontier
        WHERE state = 'pending'
        ORDER BY priority ASC, kind DESC
        LIMIT ?
        """,
        (limit,),
    ).fetchall()


# --------------------------------------------------------------------- seed


def seed(conn: sqlite3.Connection, client: QobuzClient, verbose: bool = True) -> dict[str, int]:
    """Load every favourite into the catalog at seed_distance 0."""
    stats = {"tracks": 0, "albums": 0, "artists": 0}

    for track in client.favorites("tracks"):
        track_id = upsert_track(conn, track, seed_distance=0)
        if track_id is not None:
            stats["tracks"] += 1
            performer = track.get("performer") or {}
            if performer.get("id") is not None:
                enqueue(conn, "artist", performer["id"], 0)
    conn.commit()

    for album in client.favorites("albums"):
        album_id = upsert_album(conn, album)
        if album_id is not None:
            stats["albums"] += 1
            enqueue(conn, "album", album_id, 0)
            artist = album.get("artist") or {}
            if artist.get("id") is not None:
                enqueue(conn, "artist", artist["id"], 0)
    conn.commit()

    for artist in client.favorites("artists"):
        artist_id = upsert_artist(conn, artist)
        if artist_id is not None:
            stats["artists"] += 1
            enqueue(conn, "artist", artist_id, 0)
    conn.commit()

    if verbose:
        print(
            f"seeded {stats['tracks']} tracks, {stats['albums']} albums, "
            f"{stats['artists']} artists",
            file=sys.stderr,
        )
    return stats


# -------------------------------------------------------------------- crawl


def _expand_artist(
    conn: sqlite3.Connection, client: QobuzClient, artist_id: int, distance: int, max_distance: int
) -> None:
    """Pull an artist's albums, and enqueue their similar artists one hop further out."""
    for album in client.artist_albums(artist_id):
        album_db_id = upsert_album(conn, album)
        if album_db_id is not None:
            enqueue(conn, "album", album_db_id, distance)

    conn.execute(
        "UPDATE artists SET similar_fetched_at = ? WHERE id = ?", (_now(), artist_id)
    )

    if distance < max_distance:
        blocked = blocklist.blocked_ids(conn)
        for similar in client.similar_artists(artist_id):
            similar_id = upsert_artist(conn, similar)
            # A blocked artist is a dead end, not just a hidden one: following
            # them would pull their whole neighbourhood into the catalogue.
            if similar_id is not None and similar_id not in blocked:
                enqueue(conn, "artist", similar_id, distance + 1)


def _expand_album(
    conn: sqlite3.Connection, client: QobuzClient, album_id: str, distance: int
) -> int:
    """Pull an album's tracklist. Returns how many tracks were written."""
    album = client.album(album_id)
    upsert_album(conn, album)
    artist_id = (album.get("artist") or {}).get("id")

    written = 0
    for track in (album.get("tracks") or {}).get("items") or []:
        if upsert_track(conn, track, album_id=album_id, artist_id=artist_id,
                        seed_distance=distance) is not None:
            written += 1
    return written


def _is_blocked(
    conn: sqlite3.Connection, kind: str, ref_id: str, blocked: set[int]
) -> bool:
    """Whether a frontier item belongs to a blocked artist."""
    if not blocked:
        return False
    if kind == "artist":
        return int(ref_id) in blocked
    if kind == "album":
        row = conn.execute("SELECT artist_id FROM albums WHERE id = ?", (ref_id,)).fetchone()
        return row is not None and row[0] in blocked
    return False


def crawl(
    conn: sqlite3.Connection,
    client: QobuzClient,
    max_tracks: int = 5000,
    max_distance: int = 2,
    verbose: bool = True,
) -> dict[str, int]:
    """Work the frontier until the track budget or the distance limit is reached."""
    client.login()
    stats = {
        "artists_expanded": 0,
        "albums_expanded": 0,
        "tracks_added": 0,
        "errors": 0,
        "blocked_skipped": 0,
    }

    def track_count() -> int:
        return conn.execute("SELECT COUNT(*) FROM tracks").fetchone()[0]

    start_count = track_count()

    while True:
        if track_count() >= max_tracks:
            if verbose:
                print(f"reached track budget ({max_tracks})", file=sys.stderr)
            break

        batch = _next_batch(conn)
        if not batch:
            if verbose:
                print("frontier exhausted", file=sys.stderr)
            break

        blocked = blocklist.blocked_ids(conn)

        for row in batch:
            kind, ref_id, distance = row["kind"], row["ref_id"], row["priority"]
            if distance > max_distance:
                _mark(conn, kind, ref_id, "skipped")
                continue

            if _is_blocked(conn, kind, ref_id, blocked):
                _mark(conn, kind, ref_id, "skipped")
                stats["blocked_skipped"] += 1
                continue

            try:
                if kind == "artist":
                    _expand_artist(conn, client, int(ref_id), distance, max_distance)
                    stats["artists_expanded"] += 1
                elif kind == "album":
                    _expand_album(conn, client, ref_id, distance)
                    stats["albums_expanded"] += 1
                _mark(conn, kind, ref_id, "done")
            except QobuzError as exc:
                # A dead id or a region-locked album should not stop the crawl.
                stats["errors"] += 1
                _mark(conn, kind, ref_id, "failed")
                if verbose:
                    print(f"  ! {kind} {ref_id}: {exc}", file=sys.stderr)

            # Commit per item so Ctrl-C never loses more than one unit of work.
            conn.commit()

            if track_count() >= max_tracks:
                break

        if verbose:
            print(
                f"  artists={stats['artists_expanded']} albums={stats['albums_expanded']} "
                f"tracks={track_count()}",
                file=sys.stderr,
            )

    stats["tracks_added"] = track_count() - start_count
    return stats
