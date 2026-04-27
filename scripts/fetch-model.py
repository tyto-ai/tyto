#!/usr/bin/env python3
"""Download the fastembed model (BGESmallENV15 - Xenova/bge-small-en-v1.5) into a
HuggingFace hub cache directory, then prepare it for npm bundling by:

  1. Downloading only the files fastembed actually needs (not every quantization).
  2. Resolving symlinks in snapshots/ to real files so npm pack includes them.
  3. Removing the blobs/ directory (now redundant) to avoid packing duplicate data.

Usage: python3 scripts/fetch-model.py <cache_dir>
"""
import os
import shutil
import sys

from huggingface_hub import hf_hub_download

REPO_ID = "Xenova/bge-small-en-v1.5"
# Only the files fastembed reads for BGESmallENV15 (model_file + TokenizerFiles).
FILES = [
    "onnx/model.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
]


def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <cache_dir>", file=sys.stderr)
        sys.exit(1)
    cache_dir = sys.argv[1]

    print(f"Downloading {REPO_ID} -> {cache_dir}")
    for filename in FILES:
        hf_hub_download(repo_id=REPO_ID, filename=filename, cache_dir=cache_dir)
        print(f"  {filename}")

    # The hub format stores files in blobs/ (content-addressed) and symlinks them
    # from snapshots/<commit>/<file>. npm pack skips symlinks, so snapshots/ would
    # be missing from the tarball and fastembed would not find the model.
    #
    # Fix: resolve symlinks to real files, then delete blobs/ (now redundant).
    # Result: snapshots/<commit>/<file> are real files; no duplication.
    model_dir = f"models--{REPO_ID.replace('/', '--')}"
    model_path = os.path.join(cache_dir, model_dir)

    snapshots_path = os.path.join(model_path, "snapshots")
    resolved = 0
    for root, _dirs, files in os.walk(snapshots_path, followlinks=False):
        for name in files:
            path = os.path.join(root, name)
            if os.path.islink(path):
                target = os.path.realpath(path)
                os.unlink(path)
                shutil.copy2(target, path)
                resolved += 1

    blobs_path = os.path.join(model_path, "blobs")
    if os.path.isdir(blobs_path):
        shutil.rmtree(blobs_path)

    print(f"Resolved {resolved} symlinks, removed blobs/. Model ready.")


if __name__ == "__main__":
    main()
