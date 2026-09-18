"""Analysis driver: excerpt -> Essentia + CLAP -> stored features.

Because excerpts are discarded, this pass stores everything worth having: the
raw descriptors, the CLAP embedding, the 400 style activations, and the full
1280-d EffNet embedding. The last one means any future classification head can
be run later without touching the network again.

Two pools, because the work splits cleanly in two. Fetching an excerpt is
network-bound and must respect one global request budget, so it runs on threads
in this process sharing the single rate-limited client. Extraction is
CPU-bound, about 19 core-seconds per track, and runs in worker processes,
since Essentia holds a TensorFlow session and CLAP a torch model, neither of
which tolerates being driven from several threads. Only this process writes to
SQLite.
"""

from __future__ import annotations

import json
import multiprocessing
import os
import sqlite3
import sys
import time
from collections import deque
from concurrent import futures
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import numpy as np

from . import blocklist
from .audio import AudioCache, AudioError, ensure_excerpt
from .features import clap_ext, essentia_ext
from .qobuz import AudioUnavailable, QobuzClient, QobuzError

VERSION = f"{essentia_ext.VERSION}+{clap_ext.VERSION}"

# Extraction costs ~19 core-seconds per track and both libraries thread
# internally, so a few fat workers beat many thin ones.
THREADS_PER_WORKER = 4


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def pending_tracks(
    conn: sqlite3.Connection, limit: int = 0, retry_failed: bool = False
) -> list[sqlite3.Row]:
    """Tracks with no features, stale features, and (optionally) past failures.

    Ordered by seed_distance so the user's own library is analysed first.
    """
    sql = f"""
        SELECT t.id, t.title, t.duration, t.seed_distance
        FROM tracks t
        LEFT JOIN features f ON f.track_id = t.id
        LEFT JOIN failures x ON x.track_id = t.id
        WHERE (f.track_id IS NULL OR f.extractor_version != ?)
          AND ({blocklist.NOT_BLOCKED})
    """
    params: list[object] = [VERSION]
    if not retry_failed:
        sql += " AND x.track_id IS NULL"
    sql += " ORDER BY t.seed_distance ASC, t.id ASC"
    if limit:
        sql += " LIMIT ?"
        params.append(limit)
    return conn.execute(sql, params).fetchall()


def _record_failure(conn: sqlite3.Connection, track_id: int, stage: str, reason: str) -> None:
    conn.execute(
        """
        INSERT INTO failures (track_id, stage, reason, failed_at) VALUES (?, ?, ?, ?)
        ON CONFLICT(track_id) DO UPDATE SET
            stage = excluded.stage, reason = excluded.reason, failed_at = excluded.failed_at
        """,
        (track_id, stage, reason[:500], _now()),
    )


def store(
    conn: sqlite3.Connection,
    track_id: int,
    descriptors: dict,
    clap: np.ndarray,
    effnet: np.ndarray,
    genre400: np.ndarray,
) -> None:
    conn.execute(
        """
        INSERT INTO features (track_id, extractor_version, essentia_json,
                              clap_f32, effnet_f32, genre400_f32, analysed_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(track_id) DO UPDATE SET
            extractor_version = excluded.extractor_version,
            essentia_json     = excluded.essentia_json,
            clap_f32          = excluded.clap_f32,
            effnet_f32        = excluded.effnet_f32,
            genre400_f32      = excluded.genre400_f32,
            analysed_at       = excluded.analysed_at
        """,
        (
            track_id,
            VERSION,
            json.dumps(descriptors),
            clap.astype(np.float32).tobytes(),
            effnet.astype(np.float32).tobytes(),
            genre400.astype(np.float32).tobytes(),
            _now(),
        ),
    )
    # A track that now analyses cleanly should not stay on the failure list.
    conn.execute("DELETE FROM failures WHERE track_id = ?", (track_id,))


# ----------------------------------------------------------- worker process

# Built once per worker by _worker_init; module-global because that is the only
# state a ProcessPoolExecutor task can reach.
_EXTRACTORS: tuple[Any, Any] | None = None


def _worker_init(threads: int) -> None:
    """Give this worker its own extractors and its slice of the machine.

    The thread caps have to be set before torch and TensorFlow are first
    imported, which works because the extractor modules import them lazily,
    left uncapped, every worker helps itself to all cores and the pool runs
    slower than a single process.
    """
    global _EXTRACTORS

    for variable in (
        "OMP_NUM_THREADS",
        "MKL_NUM_THREADS",
        "OPENBLAS_NUM_THREADS",
        "NUMEXPR_NUM_THREADS",
        "TF_NUM_INTRAOP_THREADS",
    ):
        os.environ[variable] = str(threads)

    extractors = (essentia_ext.EssentiaExtractor(), clap_ext.ClapExtractor())

    try:
        import torch

        torch.set_num_threads(threads)
    except ImportError:  # CPU-only builds without torch cannot run CLAP anyway
        pass

    _EXTRACTORS = extractors


def _extract(job: tuple[int, str]) -> tuple[int, tuple | None, str | None]:
    """Worker task: all the CPU work, on an excerpt that is already local.

    Failures come back as a string rather than an exception: tracebacks from
    Essentia and torch do not reliably survive pickling, and a message is all
    the failures table stores anyway.
    """
    track_id, path = job
    try:
        essentia, clap = _EXTRACTORS  # type: ignore[misc]
        excerpt = Path(path)
        descriptors, effnet, genre400 = essentia.extract(excerpt)
        return track_id, (descriptors, clap.embed_audio(excerpt), effnet, genre400), None
    except Exception as exc:  # noqa: BLE001 - reported, never raised across the pool
        return track_id, None, f"{type(exc).__name__}: {exc}"


def default_workers() -> int:
    """Enough workers to saturate the cores, given each takes several threads."""
    return max(1, (os.cpu_count() or 4) // THREADS_PER_WORKER)


# -------------------------------------------------------------------- driver


def run(
    conn: sqlite3.Connection,
    client: QobuzClient,
    limit: int = 0,
    retry_failed: bool = False,
    cache_gb: float = 20.0,
    workers: int = 0,
    download_workers: int = 0,
    verbose: bool = True,
) -> dict[str, int]:
    """Analyse every pending track. Safe to interrupt and resume."""
    todo = pending_tracks(conn, limit=limit, retry_failed=retry_failed)
    if not todo:
        if verbose:
            print("nothing to analyse", file=sys.stderr)
        return {"done": 0, "failed": 0, "total": 0}

    workers = workers or default_workers()
    workers = max(1, min(workers, len(todo)))
    threads = max(1, (os.cpu_count() or 4) // workers)
    # Fetching overlaps extraction, so a little depth here is what stops the
    # workers idling. The client's own rate limiter is the real governor.
    download_workers = download_workers or max(4, workers)

    if verbose:
        print(
            f"analysing {len(todo)} tracks (extractor {VERSION})\n"
            f"  {workers} workers x {threads} threads, {download_workers} fetchers",
            file=sys.stderr,
        )

    # Establish the session once, here: the fetch threads share this client and
    # would otherwise race to log in on their first call.
    client.login()

    cache = AudioCache(max_bytes=int(cache_gb * 1024**3))
    stats = {"done": 0, "failed": 0, "total": len(todo)}
    started = time.monotonic()

    def fail(row: sqlite3.Row, reason: str) -> None:
        _record_failure(conn, row["id"], "analyse", reason)
        conn.commit()
        stats["failed"] += 1
        if verbose:
            print(f"  ! {row['id']} {row['title'][:40]}: {reason}", file=sys.stderr)

    def fetch(row: sqlite3.Row) -> Path:
        return ensure_excerpt(client, row["id"], row["duration"], cache=cache)

    pending = deque(todo)
    downloads: dict[futures.Future, sqlite3.Row] = {}
    extractions: dict[futures.Future, sqlite3.Row] = {}
    # Excerpts land on disk as they are fetched, so cap how far ahead we run.
    in_flight_cap = workers * 2 + download_workers
    finished = 0

    # spawn, not fork: forking a process that has already imported torch or
    # TensorFlow is a reliable way to deadlock.
    context = multiprocessing.get_context("spawn")

    try:
        with futures.ThreadPoolExecutor(
            download_workers, thread_name_prefix="fetch"
        ) as fetcher, futures.ProcessPoolExecutor(
            workers,
            mp_context=context,
            initializer=_worker_init,
            initargs=(threads,),
        ) as pool:
            while pending or downloads or extractions:
                while pending and len(downloads) + len(extractions) < in_flight_cap:
                    row = pending.popleft()
                    downloads[fetcher.submit(fetch, row)] = row

                done, _ = futures.wait(
                    set(downloads) | set(extractions),
                    return_when=futures.FIRST_COMPLETED,
                )

                for future in done:
                    if future in downloads:
                        row = downloads.pop(future)
                        try:
                            path = future.result()
                        except (
                            AudioError,
                            AudioUnavailable,
                            QobuzError,
                            RuntimeError,
                            ValueError,
                            # OSError covers a vanished or unreadable excerpt.
                            # Not a RuntimeError, so it used to escape here.
                            OSError,
                        ) as exc:
                            fail(row, f"{type(exc).__name__}: {exc}")
                            finished += 1
                            continue
                        # Downloaded: hand it straight to a worker.
                        extractions[pool.submit(_extract, (row["id"], str(path)))] = row
                        continue

                    row = extractions.pop(future)
                    track_id, payload, error = future.result()
                    if error is not None:
                        fail(row, error)
                    else:
                        descriptors, clap_emb, effnet, genre400 = payload
                        store(conn, track_id, descriptors, clap_emb, effnet, genre400)
                        conn.commit()
                        stats["done"] += 1
                    finished += 1

                    if verbose and finished % 10 == 0:
                        rate = finished / max(time.monotonic() - started, 1e-6)
                        remaining = (len(todo) - finished) / max(rate, 1e-6)
                        print(
                            f"  {finished}/{len(todo)}  {rate:.2f} tracks/s"
                            f"  ({rate * 3600:.0f}/hour)  eta {remaining / 60:.1f} min",
                            file=sys.stderr,
                        )
    except KeyboardInterrupt:
        # Everything already stored is committed; the rest stays pending.
        if verbose:
            print(
                f"\ninterrupted after {stats['done']} tracks, rerun to resume",
                file=sys.stderr,
            )

    return stats
