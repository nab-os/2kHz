#!/usr/bin/env python
"""Rust/Python parity test.

The Python path functions are the oracle: they are what the space was tuned
against. The Rust port drives the app. If the two ever disagree, the app is
showing something the pipeline never validated, so this compares them on
identical inputs and fails loudly on any difference.

  uv run python scripts/parity_test.py
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

_WORKDIR = Path(tempfile.mkdtemp(prefix="qsuggest-parity-"))
os.environ["QSUGGEST_DATA_DIR"] = str(_WORKDIR / "data")
os.environ["QSUGGEST_CACHE_DIR"] = str(_WORKDIR / "cache")

import synthetic  # noqa: E402

from qsuggest import paths  # noqa: E402

APP_DIR = Path(__file__).resolve().parents[2] / "app"


def run_rust(args: list[str]) -> list[int]:
    result = subprocess.run(
        ["cargo", "run", "--quiet", "--bin", "parity", "--", *args],
        cwd=APP_DIR,
        capture_output=True,
        text=True,
        env={**os.environ},
    )
    if result.returncode != 0:
        raise RuntimeError(f"rust parity failed: {result.stderr.strip()[:500]}")
    return json.loads(result.stdout.strip())


def main() -> int:
    conn, entries, manifest = synthetic.prepare(_WORKDIR)
    nav = paths.Navigator(conn)

    track_ids = [e[0] for e in entries]
    first, middle, last = track_ids[0], track_ids[len(track_ids) // 2], track_ids[-1]

    cases: list[tuple[str, list[str], list[int]]] = []

    for track in (first, middle, last):
        cases.append(
            (
                f"neighbours {track} k=10",
                ["neighbours", str(track), "10"],
                [t["track_id"] for t in nav.neighbours(track, k=10)],
            )
        )

    # Radio at two penalties: zero for the plain greedy walk, non-zero for the
    # artist term, which is applied differently on each side.
    for track in (first, middle):
        for penalty in (0.0, 0.1):
            cases.append(
                (
                    f"radio {track} steps=10 penalty={penalty}",
                    ["radio", str(track), "10", str(penalty)],
                    [
                        t["track_id"]
                        for t in nav.radio_nearest(track, steps=10, artist_penalty=penalty)
                    ],
                )
            )

    for a, b in ((first, last), (first, middle), (middle, last)):
        cases.append(
            (
                f"graph_path {a} -> {b}",
                ["path", str(a), str(b)],
                [t["track_id"] for t in nav.graph_path(a, b)],
            )
        )
        cases.append(
            (
                f"interpolate {a} -> {b} steps=8",
                ["interpolate", str(a), str(b), "8"],
                [t["track_id"] for t in nav.interpolate(a, b, steps=8)],
            )
        )

    # Text steering: exercises the ONNX export too, since Rust embeds the phrase
    # with the exported graph while Python uses the original torch model.
    from qsuggest.features.clap_ext import ClapExtractor
    from qsuggest.features import models as feature_models

    if (feature_models.MODEL_DIR / "clap_text.onnx").is_file():
        clap = ClapExtractor()
        for phrase in ("a fast aggressive electronic beat", "quiet ambient drone"):
            vector = clap.embed_text([phrase])[0]
            cases.append(
                (
                    f"drift {first} toward {phrase!r}",
                    ["drift", str(first), phrase, "6"],
                    [t["track_id"] for t in nav.drift_to_text(first, vector, steps=6, k=5)],
                )
            )
    else:
        print("skipping drift cases: clap_text.onnx not exported", file=sys.stderr)

    print(f"\nrunning {len(cases)} parity cases\n")
    failures = 0
    for name, rust_args, python_ids in cases:
        rust_ids = run_rust(rust_args)
        if rust_ids == python_ids:
            print(f"  ok    {name}  ({len(python_ids)} tracks)")
        else:
            failures += 1
            print(f"  FAIL  {name}")
            print(f"        python: {python_ids}")
            print(f"        rust  : {rust_ids}")

    print(f"\n{len(cases) - failures}/{len(cases)} cases match")
    shutil.rmtree(_WORKDIR, ignore_errors=True)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
