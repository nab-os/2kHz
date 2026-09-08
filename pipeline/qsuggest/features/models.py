"""Download and cache Essentia's pretrained TensorFlow models.

Each model ships a .pb graph plus a .json metadata file giving its class labels
and, crucially, its input/output tensor names, those vary per graph, so we read
them from the metadata rather than hardcoding them.

Weights are CC BY-NC-SA 4.0: fine for personal use, not for a commercial product.
"""

from __future__ import annotations

import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

from .. import config

MODEL_BASE = "https://essentia.upf.edu/models"
# Deliberately not under the overridable DATA_DIR: model weights are shared
# across corpora and should not be re-downloaded by a test run.
MODEL_DIR = config.REPO_ROOT / "data" / "models"

# All heads run on the same Discogs-EffNet embedding, so the expensive part is
# computed once per track and reused.
MODELS: dict[str, str] = {
    "effnet": "feature-extractors/discogs-effnet/discogs-effnet-bs64-1",
    "genre400": "classification-heads/genre_discogs400/genre_discogs400-discogs-effnet-1",
    "danceability": "classification-heads/danceability/danceability-discogs-effnet-1",
    "mood_happy": "classification-heads/mood_happy/mood_happy-discogs-effnet-1",
    "mood_sad": "classification-heads/mood_sad/mood_sad-discogs-effnet-1",
    "mood_aggressive": "classification-heads/mood_aggressive/mood_aggressive-discogs-effnet-1",
    "mood_relaxed": "classification-heads/mood_relaxed/mood_relaxed-discogs-effnet-1",
    "mood_party": "classification-heads/mood_party/mood_party-discogs-effnet-1",
    # Stand-ins for arousal/valence: emomusic has no Discogs-EffNet variant, and
    # using its musicnn version would force a second embedding pass per track.
    "approachability": (
        "classification-heads/approachability/approachability_regression-discogs-effnet-1"
    ),
    "engagement": "classification-heads/engagement/engagement_regression-discogs-effnet-1",
    "voice_instrumental": (
        "classification-heads/voice_instrumental/voice_instrumental-discogs-effnet-1"
    ),
}

# Heads whose positive class we reduce to a single probability, with the label
# that counts as "positive" in the metadata's class list.
BINARY_HEADS = {
    "danceability": "danceable",
    "mood_happy": "happy",
    "mood_sad": "sad",
    "mood_aggressive": "aggressive",
    "mood_relaxed": "relaxed",
    "mood_party": "party",
}

REGRESSION_HEADS = ("approachability", "engagement")


def _download(url: str, dest: Path, attempts: int = 4) -> None:
    """Fetch to a temp file then rename, so an interrupted download is never seen."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")
    print(f"  downloading {dest.name}", file=sys.stderr)

    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(url, timeout=300) as resp, tmp.open("wb") as fh:
                while chunk := resp.read(1 << 16):
                    fh.write(chunk)
            tmp.replace(dest)
            return
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            tmp.unlink(missing_ok=True)
            if attempt == attempts - 1:
                raise RuntimeError(f"failed to download {url}: {exc}") from exc
            time.sleep(2 ** (attempt + 1))


def ensure(name: str) -> tuple[Path, dict]:
    """Return (graph path, metadata) for a model, downloading it on first use."""
    if name not in MODELS:
        raise KeyError(f"unknown model: {name}")
    rel = MODELS[name]
    pb = MODEL_DIR / f"{name}.pb"
    meta_path = MODEL_DIR / f"{name}.json"

    if not pb.is_file():
        _download(f"{MODEL_BASE}/{rel}.pb", pb)
    if not meta_path.is_file():
        _download(f"{MODEL_BASE}/{rel}.json", meta_path)

    return pb, json.loads(meta_path.read_text())


def ensure_all() -> None:
    for name in MODELS:
        ensure(name)


def tensor_names(meta: dict, purpose: str = "predictions") -> tuple[str, str]:
    """Pull the input/output tensor names out of a model's metadata.

    These genuinely vary per graph, the regression heads output model/Identity
    while the classifiers output model/Softmax, so they must be read, never
    assumed. `purpose` selects which output: "predictions" or "embeddings".
    """
    schema = meta.get("schema") or {}
    inputs = schema.get("inputs") or []
    outputs = schema.get("outputs") or []
    if not inputs or not outputs:
        raise ValueError(f"model metadata has no input/output schema: {meta.get('name')}")

    chosen = next((o for o in outputs if o.get("output_purpose") == purpose), None)
    if chosen is None:
        raise ValueError(f"no output with purpose {purpose!r} in {meta.get('name')}")
    return inputs[0]["name"], chosen["name"]


def classes(meta: dict) -> list[str]:
    return meta.get("classes") or []
