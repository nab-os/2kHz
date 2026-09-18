"""Essentia feature extraction: interpretable descriptors plus EffNet mood heads.

One Discogs-EffNet embedding pass per track feeds every classification head, so
the expensive work happens once. MusicExtractor supplies the classical
descriptors (BPM, key, loudness) that the neural heads do not cover.
"""

from __future__ import annotations

import warnings
from pathlib import Path
from typing import Any

import numpy as np

from . import models

VERSION = "essentia-1"

# EffNet was trained on 16kHz mono audio.
EFFNET_SAMPLE_RATE = 16000

# The classical descriptors we keep. MusicExtractor computes far more; these are
# the ones that earn a place in the space or help explain a result.
_MUSIC_EXTRACTOR_KEYS = {
    "bpm": "rhythm.bpm",
    "beats_count": "rhythm.beats_count",
    "onset_rate": "rhythm.onset_rate",
    "danceability_classic": "rhythm.danceability",
    "key": "tonal.key_edma.key",
    "scale": "tonal.key_edma.scale",
    "key_strength": "tonal.key_edma.strength",
    "loudness_integrated": "lowlevel.loudness_ebu128.integrated",
    "loudness_range": "lowlevel.loudness_ebu128.loudness_range",
    "dynamic_complexity": "lowlevel.dynamic_complexity",
    "spectral_centroid": "lowlevel.spectral_centroid.mean",
    "spectral_rolloff": "lowlevel.spectral_rolloff.mean",
    "spectral_flatness": "lowlevel.barkbands_flatness_db.mean",
    "spectral_energy": "lowlevel.spectral_energy.mean",
    "zero_crossing_rate": "lowlevel.zerocrossingrate.mean",
}


class EssentiaExtractor:
    """Lazily loads models; reuse one instance across a whole analysis run."""

    def __init__(self) -> None:
        self._effnet = None
        self._heads: dict[str, Any] = {}
        self._genre_classes: list[str] = []

    # ------------------------------------------------------------- model load

    @staticmethod
    def _quieten() -> None:
        """Essentia logs a warning per TensorFlow call; across thousands of
        tracks that buries anything worth reading."""
        import essentia

        essentia.log.warningActive = False
        essentia.log.infoActive = False

    def _load_effnet(self):
        if self._effnet is None:
            import essentia.standard as es

            self._quieten()

            pb, meta = models.ensure("effnet")
            _, out = models.tensor_names(meta, purpose="embeddings")
            self._effnet = es.TensorflowPredictEffnetDiscogs(
                graphFilename=str(pb), output=out
            )
        return self._effnet

    def _load_head(self, name: str):
        if name not in self._heads:
            import essentia.standard as es

            pb, meta = models.ensure(name)
            inp, out = models.tensor_names(meta, purpose="predictions")
            self._heads[name] = (
                es.TensorflowPredict2D(graphFilename=str(pb), input=inp, output=out),
                models.classes(meta),
            )
        return self._heads[name]

    def genre_classes(self) -> list[str]:
        if not self._genre_classes:
            _, meta = models.ensure("genre400")
            self._genre_classes = models.classes(meta)
        return self._genre_classes

    # --------------------------------------------------------------- helpers

    @staticmethod
    def _pool_get(pool, key: str) -> Any:
        try:
            value = pool[key]
        except Exception:
            return None
        if isinstance(value, np.ndarray):
            return float(value.mean()) if value.size else None
        if isinstance(value, (np.floating, np.integer)):
            return float(value)
        return value

    def _music_extractor(self, path: Path) -> dict[str, Any]:
        import essentia.standard as es

        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            pool, _ = es.MusicExtractor(
                lowlevelStats=["mean", "stdev"],
                rhythmStats=["mean"],
                tonalStats=["mean"],
            )(str(path))

        out: dict[str, Any] = {}
        for name, key in _MUSIC_EXTRACTOR_KEYS.items():
            out[name] = self._pool_get(pool, key)
        return out

    def _binary_head(self, name: str, embeddings: np.ndarray) -> float | None:
        """Mean activation of a head's positive class across patches."""
        model, classes = self._load_head(name)
        positive = models.BINARY_HEADS[name]
        if positive not in classes:
            return None
        preds = np.asarray(model(embeddings))
        return float(preds[:, classes.index(positive)].mean())

    def _regression_head(self, name: str, embeddings: np.ndarray) -> float | None:
        model, _ = self._load_head(name)
        preds = np.asarray(model(embeddings))
        return float(preds.mean())

    # ------------------------------------------------------------------- run

    def extract(self, path: Path) -> tuple[dict[str, Any], np.ndarray, np.ndarray]:
        """Analyse one excerpt.

        Returns (descriptors, effnet embedding [1280], genre400 probabilities [400]).
        """
        import essentia.standard as es

        features = self._music_extractor(path)

        audio = es.MonoLoader(
            filename=str(path), sampleRate=EFFNET_SAMPLE_RATE, resampleQuality=4
        )()
        if audio.size == 0:
            raise RuntimeError(f"decoded no audio from {path}")

        # [n_patches, 1280], one embedding per ~3s patch.
        patch_embeddings = np.asarray(self._load_effnet()(audio))
        if patch_embeddings.ndim != 2 or patch_embeddings.shape[0] == 0:
            raise RuntimeError(f"unexpected embedding shape {patch_embeddings.shape}")

        genre_model, _ = self._load_head("genre400")
        genre_probs = np.asarray(genre_model(patch_embeddings)).mean(axis=0)

        for name in models.BINARY_HEADS:
            features[name] = self._binary_head(name, patch_embeddings)
        for name in models.REGRESSION_HEADS:
            features[name] = self._regression_head(name, patch_embeddings)

        # Voice/instrumental is for filtering and explanation, not a space dimension.
        vi_model, vi_classes = self._load_head("voice_instrumental")
        vi_preds = np.asarray(vi_model(patch_embeddings)).mean(axis=0)
        if "instrumental" in vi_classes:
            features["instrumental"] = float(vi_preds[vi_classes.index("instrumental")])

        # Top styles by name, so a result can be explained without the full vector.
        classes = self.genre_classes()
        top = np.argsort(genre_probs)[::-1][:5]
        features["top_styles"] = [
            {"style": classes[i], "p": round(float(genre_probs[i]), 4)} for i in top
        ]

        return features, patch_embeddings.mean(axis=0).astype(np.float32), genre_probs.astype(
            np.float32
        )
