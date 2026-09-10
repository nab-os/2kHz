"""The artist block list.

One table, honoured by both halves of the system. The pipeline will not crawl
an artist, follow them to their similar artists, analyse their tracks, or place
them in the space; the Rust app hides whatever is already stored and refuses to
play it.

Blocking filters rather than deletes. That keeps it reversible and means a
block takes effect without re-running `build-space`, every consumer excludes
the artist at query time. `purge` is the destructive version, for when you want
the rows gone rather than hidden.

The match is on `artists.id`. Qobuz sometimes lists the same person under more
than one artist id, and a featured credit on someone else's track carries that
other artist's id, so a block is not a guarantee that no audio by that person
can ever surface, see `resolve`, which is deliberately loose about names so
that all of an artist's ids are easy to find and block together.
"""

from __future__ import annotations

import sqlite3
from datetime import datetime, timezone


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


# The SQL fragment every consumer uses. Kept here so that the definition of
# "blocked" cannot drift between the crawler, the analyser and the space.
NOT_BLOCKED = "t.artist_id IS NULL OR t.artist_id NOT IN (SELECT artist_id FROM blocked_artists)"


def blocked_ids(conn: sqlite3.Connection) -> set[int]:
    return {row[0] for row in conn.execute("SELECT artist_id FROM blocked_artists")}


def is_blocked(conn: sqlite3.Connection, artist_id: int) -> bool:
    return (
        conn.execute(
            "SELECT 1 FROM blocked_artists WHERE artist_id = ?", (artist_id,)
        ).fetchone()
        is not None
    )


def block(
    conn: sqlite3.Connection,
    artist_id: int,
    name: str | None = None,
    reason: str | None = None,
) -> None:
    """Add an artist to the block list, or update the note on an existing one."""
    if name is None:
        row = conn.execute("SELECT name FROM artists WHERE id = ?", (artist_id,)).fetchone()
        name = row[0] if row else None

    conn.execute(
        """
        INSERT INTO blocked_artists (artist_id, name, reason, blocked_at)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(artist_id) DO UPDATE SET
            name   = COALESCE(excluded.name, blocked_artists.name),
            reason = COALESCE(excluded.reason, blocked_artists.reason)
        """,
        (artist_id, name, reason, _now()),
    )

    # Drop them from the crawl frontier so an in-flight crawl stops expanding
    # them; anything already queued would otherwise still be fetched.
    conn.execute(
        "DELETE FROM frontier WHERE kind = 'artist' AND ref_id = ?", (str(artist_id),)
    )
    conn.commit()


def unblock(conn: sqlite3.Connection, artist_id: int) -> bool:
    """Remove a block. Returns whether there was one. Purged data does not come back."""
    changed = conn.execute(
        "DELETE FROM blocked_artists WHERE artist_id = ?", (artist_id,)
    ).rowcount
    conn.commit()
    return changed > 0


def listing(conn: sqlite3.Connection) -> list[sqlite3.Row]:
    return conn.execute(
        """
        SELECT b.artist_id, b.name, b.reason, b.blocked_at,
               (SELECT COUNT(*) FROM tracks t WHERE t.artist_id = b.artist_id) AS tracks
        FROM blocked_artists b
        ORDER BY b.name COLLATE NOCASE
        """
    ).fetchall()


def resolve(conn: sqlite3.Connection, needle: str) -> list[sqlite3.Row]:
    """Find candidate artists by id or name substring, for the CLI.

    Returns every match rather than guessing: an artist with several Qobuz ids
    needs all of them blocked, and that is easier to see than to detect.
    """
    if needle.isdigit():
        rows = conn.execute(
            "SELECT id, name FROM artists WHERE id = ?", (int(needle),)
        ).fetchall()
        if rows:
            return rows

    return conn.execute(
        """
        SELECT a.id, a.name,
               (SELECT COUNT(*) FROM tracks t WHERE t.artist_id = a.id) AS tracks
        FROM artists a
        WHERE a.name LIKE ? COLLATE NOCASE
        ORDER BY tracks DESC, a.name
        LIMIT 25
        """,
        (f"%{needle}%",),
    ).fetchall()


def purge(conn: sqlite3.Connection, artist_id: int) -> dict[str, int]:
    """Delete a blocked artist's stored data outright.

    Hiding is enough for most purposes; this is for actually reclaiming the
    rows. The space keeps its own copy of every vector, so `build-space` and
    `layout` still need re-running afterwards for the map to forget them.
    """
    track_ids = [
        row[0] for row in conn.execute("SELECT id FROM tracks WHERE artist_id = ?", (artist_id,))
    ]
    counts = {"tracks": len(track_ids), "features": 0, "albums": 0, "layout": 0}

    for table in ("features", "layout", "failures"):
        placeholders = ",".join("?" * len(track_ids))
        if track_ids:
            removed = conn.execute(
                f"DELETE FROM {table} WHERE track_id IN ({placeholders})", track_ids
            ).rowcount
            if table in counts:
                counts[table] = removed

    if track_ids:
        conn.execute(
            f"DELETE FROM tracks WHERE id IN ({','.join('?' * len(track_ids))})", track_ids
        )
    counts["albums"] = conn.execute(
        "DELETE FROM albums WHERE artist_id = ?", (artist_id,)
    ).rowcount
    conn.execute("DELETE FROM frontier WHERE kind = 'artist' AND ref_id = ?", (str(artist_id),))
    conn.commit()
    return counts
