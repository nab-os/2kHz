"""Export CLAP's text tower to ONNX, so the desktop app can steer by text.

Only the text branch is exported. The audio tower stays in Python: it runs
during analysis and is never needed at query time, so shipping it would be dead
weight in the app.

The tokenizer is exported alongside, since the Rust side has to reproduce the
exact same token ids for the embedding to land in the right place.

  uv run python -m two_khz.features.onnx_export
"""

from __future__ import annotations

import shutil
import sys
from pathlib import Path

import numpy as np

from . import models
from .clap_ext import CHECKPOINT, ClapExtractor

ONNX_NAME = "clap_text.onnx"
TOKENIZER_NAME = "clap_tokenizer.json"

# Phrases used to check the exported graph against the original model.
CHECK_PHRASES = [
    "aggressive distorted electric guitars",
    "solo piano, quiet and intimate",
    "fast electronic dance music with a heavy kick drum",
]


def export(output_dir: Path | None = None, verbose: bool = True) -> Path:
    import torch

    output_dir = output_dir or models.MODEL_DIR
    output_dir.mkdir(parents=True, exist_ok=True)
    onnx_path = output_dir / ONNX_NAME

    extractor = ClapExtractor()
    model, processor = extractor._load()  # noqa: SLF001 (deliberate internal use)
    model = model.to("cpu").eval()

    class TextTower(torch.nn.Module):
        """Thin wrapper so the ONNX graph has exactly two inputs and one output."""

        def __init__(self, clap):
            super().__init__()
            self.clap = clap

        def forward(self, input_ids, attention_mask):
            out = self.clap.get_text_features(
                input_ids=input_ids, attention_mask=attention_mask
            )
            return getattr(out, "pooler_output", out)

    tower = TextTower(model).eval()

    sample = processor(text=CHECK_PHRASES, return_tensors="pt", padding=True)
    args = (sample["input_ids"], sample["attention_mask"])

    if verbose:
        print(f"exporting text tower to {onnx_path} ...", file=sys.stderr)

    with torch.no_grad():
        torch.onnx.export(
            tower,
            args,
            str(onnx_path),
            input_names=["input_ids", "attention_mask"],
            output_names=["text_embedding"],
            dynamic_axes={
                "input_ids": {0: "batch", 1: "sequence"},
                "attention_mask": {0: "batch", 1: "sequence"},
                "text_embedding": {0: "batch"},
            },
            opset_version=17,
            dynamo=False,
        )

    # The Rust side must tokenise identically, so ship the tokenizer too.
    tokenizer_source = _find_tokenizer_json()
    if tokenizer_source is None:
        raise RuntimeError(
            f"could not locate tokenizer.json for {CHECKPOINT} in the HF cache"
        )
    shutil.copyfile(tokenizer_source, output_dir / TOKENIZER_NAME)

    if verbose:
        print(f"wrote {onnx_path.name} and {TOKENIZER_NAME}", file=sys.stderr)

    return onnx_path


def _find_tokenizer_json() -> Path | None:
    from huggingface_hub import try_to_load_from_cache

    cached = try_to_load_from_cache(CHECKPOINT, "tokenizer.json")
    return Path(cached) if isinstance(cached, str) else None


def verify(output_dir: Path | None = None, tolerance: float = 1e-3) -> float:
    """Compare ONNX output against the torch model. Returns max abs difference."""
    import onnxruntime as ort

    output_dir = output_dir or models.MODEL_DIR
    extractor = ClapExtractor()
    _, processor = extractor._load()  # noqa: SLF001

    reference = extractor.embed_text(CHECK_PHRASES)

    session = ort.InferenceSession(str(output_dir / ONNX_NAME), providers=["CPUExecutionProvider"])
    encoded = processor(text=CHECK_PHRASES, return_tensors="np", padding=True)
    outputs = session.run(
        None,
        {
            "input_ids": encoded["input_ids"].astype(np.int64),
            "attention_mask": encoded["attention_mask"].astype(np.int64),
        },
    )[0]
    norms = np.linalg.norm(outputs, axis=-1, keepdims=True)
    produced = outputs / np.clip(norms, 1e-9, None)

    difference = float(np.abs(produced - reference).max())
    print(f"max abs difference vs torch: {difference:.2e} "
          f"({'ok' if difference < tolerance else 'TOO LARGE'})")
    return difference


if __name__ == "__main__":
    export()
    raise SystemExit(0 if verify() < 1e-3 else 1)
