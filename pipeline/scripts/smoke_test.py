#!/usr/bin/env python
"""End-to-end smoke test on a synthetic corpus.

Runs the real extractors and the real space assembly over audio with known
properties, then checks the space recovers the structure we put in. Exercises
everything except the Qobuz network layer, so it runs without credentials.

  uv run python scripts/smoke_test.py
"""

from __future__ import annotations

import os
import shutil
import sys
import tempfile
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

# Redirect the data dir before importing qsuggest, so this never touches a real
# corpus. Model weights live outside DATA_DIR and are still shared.
_WORKDIR = Path(tempfile.mkdtemp(prefix="qsuggest-smoke-"))
os.environ["QSUGGEST_DATA_DIR"] = str(_WORKDIR / "data")
os.environ["QSUGGEST_CACHE_DIR"] = str(_WORKDIR / "cache")

import synthetic  # noqa: E402

from qsuggest import paths, space  # noqa: E402
from qsuggest.features import clap_ext  # noqa: E402


def album_indices(nav: paths.Navigator, album_index: int) -> list[int]:
    return [i for i, a in enumerate(nav.album_id) if a == f"album-{album_index}"]


def main() -> int:
    print(f"workdir: {_WORKDIR}")
    conn, entries, manifest = synthetic.prepare(_WORKDIR)
    print(f"  {manifest['n_tracks']} tracks x {manifest['n_dims']} dims")

    nav = paths.Navigator(conn)
    problems = 0

    # 1. Tempo recovery: the BPM we synthesised should be what Essentia reports.
    print("\n--- tempo recovery ---")
    for album_index, (name, bpm, *_r) in enumerate(synthetic.ALBUMS):
        if not bpm:
            continue
        detected = np.array([nav.bpm[i] for i in album_indices(nav, album_index)])
        ratios = detected / bpm
        # Half and double time are accepted: that is exactly what the tempo
        # folding in the space exists to absorb.
        ok = all(
            np.isclose(r, 1, atol=0.06) or np.isclose(r, 0.5, atol=0.03)
            or np.isclose(r, 2, atol=0.12)
            for r in ratios
        )
        problems += 0 if ok else 1
        print(f"  {name:<18} want {bpm:>3}  got {np.round(detected, 1)}  "
              f"{'ok' if ok else 'MISMATCH'}")

    # 2. Space sanity: the nearest neighbour should come from the same album.
    #    If this fails the space is noise and no path logic can rescue it.
    print("\n--- same-album neighbour test ---")
    ranks, top1 = [], 0
    for album_index in range(len(synthetic.ALBUMS)):
        indices = album_indices(nav, album_index)
        for i in indices:
            sims = nav.unit @ nav.unit[i]
            sims[i] = -np.inf
            same = set(indices) - {i}
            rank = next(r for r, j in enumerate(np.argsort(-sims), start=1) if int(j) in same)
            ranks.append(rank)
            top1 += rank == 1
    share = 100 * top1 / len(ranks)
    print(f"  same-album is top-1: {top1}/{len(ranks)} ({share:.0f}%)")
    print(f"  median rank        : {np.median(ranks):.1f} of {len(nav.track_ids)}")
    if share < 70:
        problems += 1
        print("  MISMATCH: the space is not recovering album structure")

    # 3. A path between the two most distant albums.
    print("\n--- path: ambient -> drum and bass ---")
    ambient = nav.track_ids[album_indices(nav, 0)[0]]
    dnb = nav.track_ids[album_indices(nav, 4)[0]]
    for step in nav.graph_path(int(ambient), int(dnb)):
        bpm = f"{step['bpm']:.0f}" if step["bpm"] else "-"
        print(f"  {bpm:>5}  {step['artist']:<12} {step['title']}")

    # 4. Text steering: an obviously matching phrase should find its album.
    print("\n--- text steering ---")
    clap = clap_ext.ClapExtractor()
    rows = space.load_rows(conn)
    clap_matrix = np.vstack([r["clap"] for r in rows])
    expectations = {
        "a fast aggressive electronic beat": ("album-1", "album-4"),
        "quiet ambient drone, no drums": ("album-0", "album-7"),
    }
    for phrase, acceptable in expectations.items():
        best = int(np.argmax(clap_matrix @ clap.embed_text([phrase])[0]))
        index = nav.index_of[rows[best]["id"]]
        hit = nav.album_id[index] in acceptable
        problems += 0 if hit else 1
        print(f"  {phrase!r:<42} -> {nav.album_title[index]}  {'ok' if hit else 'UNEXPECTED'}")

    print(f"\nproblems: {problems}")
    shutil.rmtree(_WORKDIR, ignore_errors=True)
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
