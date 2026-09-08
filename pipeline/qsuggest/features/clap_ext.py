"""CLAP embeddings: a joint audio/text space.

The audio tower gives the "semantic" block of the vector space. The text tower
is what makes text-steered drift possible, a phrase and a track land in the
same 512-d space, so "hazier" is a direction you can actually walk in.

Checkpoint choice matters: laion/larger_clap_music has a broken text tower whose
embeddings are near-constant regardless of input (mean pairwise cosine ~0.999),
which would silently destroy steering while still "working". Use the unfused
general checkpoint.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np

VERSION = "clap-htsat-unfused-1"
CHECKPOINT = "laion/clap-htsat-unfused"

CLAP_SAMPLE_RATE = 48000
# CLAP's audio branch consumes 10s windows. A 90s excerpt is chunked and
# mean-pooled rather than truncated, so the embedding reflects the whole excerpt.
WINDOW_SECONDS = 10


class ClapExtractor:
    """Lazily loads the model; reuse one instance across a run."""

    def __init__(self, checkpoint: str = CHECKPOINT, device: str | None = None):
        self.checkpoint = checkpoint
        self._device = device
        self._model = None
        self._processor = None

    def _load(self):
        if self._model is None:
            import torch
            from transformers import ClapModel, ClapProcessor

            self._device = self._device or ("cuda" if torch.cuda.is_available() else "cpu")
            self._model = ClapModel.from_pretrained(self.checkpoint).to(self._device).eval()
            self._processor = ClapProcessor.from_pretrained(self.checkpoint)
        return self._model, self._processor

    @staticmethod
    def _normalise(vectors: np.ndarray) -> np.ndarray:
        norms = np.linalg.norm(vectors, axis=-1, keepdims=True)
        return vectors / np.clip(norms, 1e-9, None)

    @staticmethod
    def _pooled(output) -> np.ndarray:
        """get_*_features returns a bare tensor on transformers 4 and an output
        object on 5, where the projected 512-d vector is pooler_output."""
        tensor = getattr(output, "pooler_output", output)
        return tensor.detach().cpu().numpy()

    def _load_audio(self, path: Path) -> np.ndarray:
        import essentia.standard as es

        return es.MonoLoader(
            filename=str(path), sampleRate=CLAP_SAMPLE_RATE, resampleQuality=4
        )()

    def embed_audio(self, path: Path) -> np.ndarray:
        """Mean-pooled, L2-normalised 512-d embedding for an excerpt."""
        import torch

        model, processor = self._load()
        audio = self._load_audio(path)
        if audio.size == 0:
            raise RuntimeError(f"decoded no audio from {path}")

        window = WINDOW_SECONDS * CLAP_SAMPLE_RATE
        chunks = [audio[i : i + window] for i in range(0, len(audio), window)]
        # Drop a trailing stub that is too short to be meaningful.
        chunks = [c for c in chunks if len(c) >= window // 2] or [audio]

        inputs = processor(
            audio=[c.astype(np.float32) for c in chunks],
            sampling_rate=CLAP_SAMPLE_RATE,
            return_tensors="pt",
        ).to(self._device)

        with torch.no_grad():
            embeddings = self._pooled(model.get_audio_features(**inputs))

        # Normalise per window before averaging, so a loud window cannot dominate.
        return self._normalise(self._normalise(embeddings).mean(axis=0)).astype(np.float32)

    def embed_text(self, texts: list[str]) -> np.ndarray:
        """L2-normalised 512-d embeddings for phrases, in the same space as audio."""
        import torch

        model, processor = self._load()
        inputs = processor(text=texts, return_tensors="pt", padding=True).to(self._device)
        with torch.no_grad():
            embeddings = self._pooled(model.get_text_features(**inputs))
        return self._normalise(embeddings).astype(np.float32)


def self_check(extractor: ClapExtractor | None = None) -> float:
    """Guard against the near-constant-text-embedding failure mode.

    Returns mean pairwise cosine between unrelated phrases. Healthy checkpoints
    sit near 0.2-0.3; anything above ~0.9 means the text tower is broken and any
    steering built on it would be meaningless.
    """
    extractor = extractor or ClapExtractor()
    phrases = [
        "aggressive distorted electric guitars",
        "solo piano, quiet and intimate",
        "fast electronic dance music with a heavy kick drum",
        "a cappella choir singing in a cathedral",
    ]
    vectors = extractor.embed_text(phrases)
    sims = vectors @ vectors.T
    off_diagonal = sims[~np.eye(len(phrases), dtype=bool)]
    return float(off_diagonal.mean())
