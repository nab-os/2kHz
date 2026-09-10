"""Assemble the vector space from stored features.

This is a pure function over the database: no network, no audio. Rebuilding
after a weight change takes seconds, which is the whole point of storing raw
descriptors and full embeddings in the first place.

Output contract (read by the Rust app):
  data/space.bin          [n_tracks x n_dims] float32, per-block normalised,
                          UNWEIGHTED, weights are applied at query time so the
                          UI sliders can reshape distances live.
  data/space.json         track id order, block layout, default weights.
  data/semantic_pca.bin   mean + components, so a CLAP text vector can be
                          projected into the semantic block for text steering.
"""

from __future__ import annotations

import json
import sqlite3
import sys
from datetime import datetime, timezone
from typing import Any

import numpy as np

from . import blocklist, config

# Bump whenever the block layout changes: a reordered block keeps the same byte
# count, so the app's length check cannot catch it. See space.rs.
SPACE_VERSION = "space-1"

# Chromatic index per key name, used to place keys on the circle of fifths.
_KEYS = {"C": 0, "C#": 1, "Db": 1, "D": 2, "D#": 3, "Eb": 3, "E": 4, "F": 5,
         "F#": 6, "Gb": 6, "G": 7, "G#": 8, "Ab": 8, "A": 9, "A#": 10, "Bb": 10, "B": 11}

# Tempo folded into one octave, so a track detected at half or double time
# lands in the same place. D&B read as 86 instead of 172 is a known failure.
BPM_FOLD_LOW = 70.0
BPM_FOLD_HIGH = 140.0

N_STYLE_COMPONENTS = 20
N_SEMANTIC_COMPONENTS = 40

DEFAULT_WEIGHTS = {
    "tempo": 1.0,
    "key": 0.4,
    "dynamics": 0.8,
    "timbre": 0.6,
    "mood": 1.2,
    "style": 1.5,
    "era": 0.3,
    "semantic": 2.0,
}


# ---------------------------------------------------------------- primitives


def fold_bpm(bpm: float | None) -> float | None:
    """log2 of the tempo folded into [70, 140) BPM."""
    if not bpm or bpm <= 0 or not np.isfinite(bpm):
        return None
    value = float(bpm)
    while value < BPM_FOLD_LOW:
        value *= 2
    while value >= BPM_FOLD_HIGH:
        value /= 2
    return float(np.log2(value))


def key_coordinates(key: str | None, scale: str | None, strength: float | None) -> list[float]:
    """Place a key on the circle of fifths, so adjacent keys are adjacent points.

    The radius is the detection strength: a track with an ambiguous key sits near
    the origin rather than being confidently placed in the wrong spot.
    """
    if not key or key not in _KEYS:
        return [0.0, 0.0, 0.0]
    radius = float(strength) if strength and np.isfinite(strength) else 0.5
    position = (_KEYS[key] * 7) % 12  # steps around the circle of fifths
    angle = 2 * np.pi * position / 12
    mode = 1.0 if (scale or "").lower() == "major" else -1.0
    return [radius * float(np.cos(angle)), radius * float(np.sin(angle)), radius * mode]


def fit_pca(matrix: np.ndarray, n_components: int) -> tuple[np.ndarray, np.ndarray]:
    """Plain SVD PCA. Returns (mean [d], components [k, d])."""
    mean = matrix.mean(axis=0)
    centred = matrix - mean
    n_components = min(n_components, *centred.shape)
    _, _, vt = np.linalg.svd(centred, full_matrices=False)
    return mean.astype(np.float32), vt[:n_components].astype(np.float32)


def normalise_block(block: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Z-score each column, then L2-normalise each row.

    Both halves matter. Column z-scoring stops one large-range descriptor from
    swamping its neighbours; row L2 normalisation makes every block contribute
    equally before weights, so a 40-d block cannot silently dominate a 2-d one.

    Returns (normalised, mean, std). The mean and std are kept in the manifest
    so an external point, a CLAP text embedding, say, can be projected into
    the same space later for steering.
    """
    block = np.nan_to_num(block, nan=0.0, posinf=0.0, neginf=0.0)
    mean = block.mean(axis=0)
    std = block.std(axis=0)
    std[std < 1e-9] = 1.0
    scaled = (block - mean) / std
    norms = np.linalg.norm(scaled, axis=1, keepdims=True)
    return (
        (scaled / np.clip(norms, 1e-9, None)).astype(np.float32),
        mean.astype(np.float32),
        std.astype(np.float32),
    )


def _impute(values: list[float | None]) -> np.ndarray:
    """Replace missing values with the column median."""
    array = np.array([np.nan if v is None else float(v) for v in values], dtype=np.float64)
    if np.all(np.isnan(array)):
        return np.zeros_like(array)
    array[np.isnan(array)] = float(np.nanmedian(array))
    return array


# -------------------------------------------------------------------- build


def load_rows(conn: sqlite3.Connection) -> list[dict[str, Any]]:
    """Every track that has features, with the metadata the space needs.

    Blocked artists are left out, so a rebuild drops them from the space and
    the map. The app also filters them at load time, which is what makes a
    block take effect before the next rebuild.
    """
    sql = f"""
        SELECT t.id, t.title, t.artist_id, t.album_id, t.seed_distance,
               a.release_date, a.genre AS qobuz_genre,
               f.essentia_json, f.clap_f32, f.genre400_f32
        FROM tracks t
        JOIN features f ON f.track_id = t.id
        LEFT JOIN albums a ON a.id = t.album_id
        WHERE {blocklist.NOT_BLOCKED}
        ORDER BY t.id
    """
    rows = []
    for row in conn.execute(sql):
        rows.append(
            {
                "id": row["id"],
                "release_date": row["release_date"],
                "descriptors": json.loads(row["essentia_json"] or "{}"),
                "clap": np.frombuffer(row["clap_f32"], dtype=np.float32),
                "genre400": np.frombuffer(row["genre400_f32"], dtype=np.float32),
            }
        )
    return rows


def _release_year(release_date: str | None) -> float | None:
    if not release_date or len(release_date) < 4 or not release_date[:4].isdigit():
        return None
    return float(release_date[:4])


def build(
    conn: sqlite3.Connection, weights: dict[str, float] | None = None, verbose: bool = True
) -> dict[str, Any]:
    """Build space.bin / space.json / semantic_pca.bin from stored features."""
    weights = {**DEFAULT_WEIGHTS, **(weights or {})}
    rows = load_rows(conn)
    if not rows:
        raise RuntimeError("no analysed tracks; run `qsuggest analyse` first")

    n = len(rows)
    descriptors = [r["descriptors"] for r in rows]
    if verbose:
        print(f"building space from {n} tracks", file=sys.stderr)

    def column(name: str) -> np.ndarray:
        return _impute([d.get(name) for d in descriptors])

    blocks: dict[str, np.ndarray] = {}
    columns: dict[str, list[str]] = {}

    # tempo
    blocks["tempo"] = np.column_stack(
        [_impute([fold_bpm(d.get("bpm")) for d in descriptors]), column("onset_rate")]
    )
    columns["tempo"] = ["log_bpm_folded", "onset_rate"]

    # key
    blocks["key"] = np.array(
        [
            key_coordinates(d.get("key"), d.get("scale"), d.get("key_strength"))
            for d in descriptors
        ],
        dtype=np.float64,
    )
    columns["key"] = ["fifths_cos", "fifths_sin", "mode"]

    # dynamics
    blocks["dynamics"] = np.column_stack(
        [column("loudness_integrated"), column("dynamic_complexity"), column("loudness_range")]
    )
    columns["dynamics"] = ["loudness_integrated", "dynamic_complexity", "loudness_range"]

    # timbre
    blocks["timbre"] = np.column_stack(
        [
            column("spectral_centroid"),
            column("spectral_rolloff"),
            column("spectral_flatness"),
            column("zero_crossing_rate"),
        ]
    )
    columns["timbre"] = [
        "spectral_centroid", "spectral_rolloff", "spectral_flatness", "zero_crossing_rate"
    ]

    # mood
    mood_names = [
        "danceability", "mood_happy", "mood_sad", "mood_aggressive",
        "mood_relaxed", "mood_party", "approachability", "engagement",
    ]
    blocks["mood"] = np.column_stack([column(name) for name in mood_names])
    columns["mood"] = mood_names

    # style: 400 Discogs activations -> PCA
    genre_matrix = np.vstack([r["genre400"] for r in rows]).astype(np.float64)
    style_mean, style_components = fit_pca(genre_matrix, N_STYLE_COMPONENTS)
    blocks["style"] = (genre_matrix - style_mean) @ style_components.T
    columns["style"] = [f"style_pc{i}" for i in range(blocks["style"].shape[1])]

    # era
    blocks["era"] = _impute([_release_year(r["release_date"]) for r in rows]).reshape(-1, 1)
    columns["era"] = ["release_year"]

    # semantic: CLAP 512 -> PCA
    clap_matrix = np.vstack([r["clap"] for r in rows]).astype(np.float64)
    semantic_mean, semantic_components = fit_pca(clap_matrix, N_SEMANTIC_COMPONENTS)
    blocks["semantic"] = (clap_matrix - semantic_mean) @ semantic_components.T
    columns["semantic"] = [f"clap_pc{i}" for i in range(blocks["semantic"].shape[1])]

    # Normalise and lay out. Order is fixed so the Rust side can rely on it.
    order = ["tempo", "key", "dynamics", "timbre", "mood", "style", "era", "semantic"]
    normalised = []
    layout = []
    cursor = 0
    for name in order:
        block, mean, std = normalise_block(np.asarray(blocks[name], dtype=np.float64))
        normalised.append(block)
        width = block.shape[1]
        layout.append(
            {
                "name": name,
                "start": cursor,
                "end": cursor + width,
                "columns": columns[name],
                "default_weight": weights[name],
                "mean": [float(v) for v in mean],
                "std": [float(v) for v in std],
            }
        )
        cursor += width

    matrix = np.hstack(normalised).astype(np.float32)

    config.ensure_dirs()
    config.SPACE_BIN.write_bytes(matrix.tobytes())

    # Semantic PCA params, so Rust can project a CLAP text vector into the
    # semantic block and use it as a steering direction.
    semantic_pca_path = config.DATA_DIR / "semantic_pca.bin"
    semantic_pca_path.write_bytes(
        semantic_mean.astype(np.float32).tobytes()
        + semantic_components.astype(np.float32).tobytes()
    )

    manifest = {
        "version": SPACE_VERSION,
        "built_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "n_tracks": n,
        "n_dims": int(matrix.shape[1]),
        "track_ids": [r["id"] for r in rows],
        "blocks": layout,
        "weights": weights,
        "semantic_pca": {
            "file": semantic_pca_path.name,
            "input_dim": int(clap_matrix.shape[1]),
            "n_components": int(semantic_components.shape[0]),
        },
    }
    config.SPACE_JSON.write_text(json.dumps(manifest, indent=2))

    if verbose:
        print(f"wrote {matrix.shape[0]} x {matrix.shape[1]} to {config.SPACE_BIN}", file=sys.stderr)
        for block in layout:
            print(
                f"  {block['name']:<10} dims {block['start']:>3}..{block['end']:<3} "
                f"weight {block['default_weight']}",
                file=sys.stderr,
            )

    return manifest


# ------------------------------------------------------------------ loading


def load(weights: dict[str, float] | None = None) -> tuple[np.ndarray, dict[str, Any]]:
    """Load the space and apply weights. Mirrors what the Rust app does."""
    manifest = json.loads(config.SPACE_JSON.read_text())
    matrix = np.frombuffer(config.SPACE_BIN.read_bytes(), dtype=np.float32).reshape(
        manifest["n_tracks"], manifest["n_dims"]
    )
    weighted = matrix.copy()
    active = {**manifest["weights"], **(weights or {})}
    for block in manifest["blocks"]:
        weighted[:, block["start"] : block["end"]] *= float(active[block["name"]])
    return weighted, manifest
