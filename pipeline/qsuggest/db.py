"""Database helpers.

The schema itself lives in `schema.sql` at the repo root, because both halves
create these tables now: Python on connect, and the Rust crawler on a fresh
checkout. One file, so the two cannot drift.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from . import config

SCHEMA_PATH = config.REPO_ROOT / "schema.sql"


def schema_sql() -> str:
    """The shared schema. See schema.sql for why it lives outside this file."""
    return SCHEMA_PATH.read_text()


# Columns added after the first schema shipped. CREATE TABLE IF NOT EXISTS does
# nothing to an existing table, so these have to be applied explicitly.
MIGRATIONS: dict[str, dict[str, str]] = {
    "features": {
        "genre400_f32": "BLOB",
    },
}


def _migrate(conn: sqlite3.Connection) -> list[str]:
    """Add any columns missing from an older database. Returns what was added."""
    applied = []
    for table, columns in MIGRATIONS.items():
        existing = {row["name"] for row in conn.execute(f"PRAGMA table_info({table})")}
        if not existing:
            continue  # table does not exist yet; the schema script will create it
        for name, decl in columns.items():
            if name not in existing:
                conn.execute(f"ALTER TABLE {table} ADD COLUMN {name} {decl}")
                applied.append(f"{table}.{name}")
    if applied:
        conn.commit()
    return applied


def connect(path: Path | None = None) -> sqlite3.Connection:
    """Open the database, creating it and its schema if needed."""
    config.ensure_dirs()
    conn = sqlite3.connect(path or config.DB_PATH)
    conn.row_factory = sqlite3.Row
    conn.executescript(schema_sql())
    _migrate(conn)
    return conn


def counts(conn: sqlite3.Connection) -> dict[str, int]:
    """Row counts for the main tables, for CLI status output."""
    out = {}
    for table in ("artists", "albums", "tracks", "features", "layout", "failures"):
        out[table] = conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
    out["frontier_pending"] = conn.execute(
        "SELECT COUNT(*) FROM frontier WHERE state = 'pending'"
    ).fetchone()[0]
    return out
