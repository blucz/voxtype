#!/usr/bin/env python3
"""Quantize parakeet-unified-en-0.6b to int8 for the streaming pipeline.

Why this exists
---------------
`parakeet-unified-en-0.6b` ships fp32: a 2.4 GB encoder that the daemon holds
resident for the life of the process (`on_demand_loading = false`, because a
~2s load on every hotkey press is worse). The streaming pipeline re-encodes
`left + chunk + right` seconds of audio on every step -- parakeet-rs calls this
"buffered streaming" and NVIDIA's own model card flags it: "The current
inference pipeline supports only buffered streaming (left context is
recomputed for each chunk)." So the encoder runs hot for the whole recording,
and at fp32 that is the single largest power draw in the daemon.

int8 halves it for free. Measured on a Core Ultra X7 358H (16 cores, AVX-VNNI)
over 55.8s of speech, CPU-seconds per second of audio:

    left/chunk/right     fp32     int8
    1.6 / 0.16 / 0.16    2.27x    1.30x
    1.6 / 0.48 / 0.16    0.83x    0.48x
    5.6 / 0.56 / 0.56    1.84x    0.97x

At 5.6/0.56/0.56 the int8 transcript was byte-identical to fp32 across 160
words. At the other two the only differences were punctuation -- no word
substitutions. Dynamic quantization touches weights only, and a 0.6B
FastConformer has plenty of headroom.

There is no published artifact to download. Quantizing here means the result
is provably derived from the fp32 weights already validated on this machine,
and it keeps the fork from depending on a third party's re-upload.

Output filenames match the input exactly, so `ParakeetUnifiedHandle::load`
loads the int8 directory with no code change.

Usage:
    uv run --with onnx --with onnxruntime scripts/quantize-unified-int8.py
"""

import shutil
import sys
import time
from pathlib import Path

MODELS = Path.home() / ".local/share/voxtype/models"
SRC = MODELS / "parakeet-unified-en-0.6b"
DST = MODELS / "parakeet-unified-en-0.6b-int8"

# parakeet-rs reads these names verbatim; the tokenizer and vocab are copied
# through untouched.
GRAPHS = ("encoder.onnx", "decoder_joint.onnx")
VERBATIM = ("tokenizer.model", "vocab.txt")


def main() -> int:
    try:
        from onnxruntime.quantization import QuantType, quantize_dynamic
    except ImportError:
        sys.exit(
            "onnxruntime is not installed.\n"
            "Run: uv run --with onnx --with onnxruntime "
            "scripts/quantize-unified-int8.py"
        )

    if not SRC.is_dir():
        sys.exit(
            f"Source model not found: {SRC}\n"
            "Run `voxtype setup model parakeet-unified-en-0.6b` first."
        )

    DST.mkdir(parents=True, exist_ok=True)

    for name in VERBATIM:
        if (SRC / name).exists():
            shutil.copy2(SRC / name, DST / name)
            print(f"copied {name}")

    for name in GRAPHS:
        src, dst = SRC / name, DST / name
        print(f"quantizing {name} ...", flush=True)
        start = time.time()
        quantize_dynamic(
            model_input=str(src),
            model_output=str(dst),
            weight_type=QuantType.QInt8,
            per_channel=True,
            # This CPU has AVX-VNNI, so the 7-bit saturation workaround that
            # reduce_range exists for (pre-VNNI AVX2) is unnecessary here and
            # only costs accuracy.
            reduce_range=False,
        )
        print(f"  {dst.stat().st_size / 1e6:.0f} MB in {time.time() - start:.0f}s")

    total = sum(f.stat().st_size for f in DST.iterdir()) / 1e6
    print(f"\nwrote {DST} ({total:.0f} MB)")
    print('Set [parakeet] model = "parakeet-unified-en-0.6b-int8" in config.toml.')
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
