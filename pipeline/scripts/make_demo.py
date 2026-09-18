#!/usr/bin/env python
"""Build a synthetic demo corpus, including the 2D layout, into a directory.

Useful for exercising the desktop app before a real crawl exists:

  uv run python scripts/make_demo.py /tmp/two-khz-demo
  cd ../app && TWO_KHZ_DATA_DIR=/tmp/two-khz-demo/data cargo run
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

if len(sys.argv) < 2:
    print(__doc__)
    raise SystemExit(2)

WORKDIR = Path(sys.argv[1]).resolve()
os.environ["TWO_KHZ_DATA_DIR"] = str(WORKDIR / "data")
os.environ["TWO_KHZ_CACHE_DIR"] = str(WORKDIR / "cache")

import synthetic  # noqa: E402

from two_khz import space  # noqa: E402


def main() -> int:
    conn, entries, manifest = synthetic.prepare(WORKDIR)
    print(f"  {manifest['n_tracks']} tracks x {manifest['n_dims']} dims")

    print("laying out ...")
    import numpy as np
    import umap

    vectors, manifest = space.load()
    # n_neighbors must stay below the corpus size for tiny demo sets.
    n_neighbors = min(15, max(2, manifest["n_tracks"] - 1))
    coords = umap.UMAP(
        n_neighbors=n_neighbors, min_dist=0.1, metric="cosine", random_state=42
    ).fit_transform(vectors)

    conn.executemany(
        """
        INSERT INTO layout (track_id, x, y) VALUES (?, ?, ?)
        ON CONFLICT(track_id) DO UPDATE SET x = excluded.x, y = excluded.y
        """,
        [
            (int(t), float(x), float(y))
            for t, (x, y) in zip(manifest["track_ids"], np.asarray(coords))
        ],
    )
    conn.commit()

    print(f"\ndemo corpus ready at {WORKDIR / 'data'}")
    print(f"run the app with:\n  cd app && TWO_KHZ_DATA_DIR={WORKDIR / 'data'} cargo run")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
