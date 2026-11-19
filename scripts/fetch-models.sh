#!/usr/bin/env bash
# Download the local speech models (about 450 MB download, ~700 MB on disk).
# Safe to re-run: anything already present is skipped.
set -euo pipefail
cd "$(dirname "$0")/.."

BASE=https://github.com/k2-fsa/sherpa-onnx/releases/download
mkdir -p models

if [ ! -f kokoro-multi-lang-v1_0/model.onnx ]; then
  echo "voice: Kokoro multi-lang v1.0"
  curl -fL "$BASE/tts-models/kokoro-multi-lang-v1_0.tar.bz2" | tar xj
fi

if [ ! -f models/sherpa-onnx-whisper-small.en/small.en-encoder.int8.onnx ]; then
  echo "ears: Whisper small.en (int8)"
  curl -fL "$BASE/asr-models/sherpa-onnx-whisper-small.en.tar.bz2" | tar xj -C models
  # only the int8 models are used; drop the full-precision copies (~900 MB)
  rm -f models/sherpa-onnx-whisper-small.en/small.en-{encoder,decoder}.onnx
fi

if [ ! -f models/silero_vad.onnx ]; then
  echo "ears: Silero VAD"
  curl -fL -o models/silero_vad.onnx "$BASE/asr-models/silero_vad.onnx"
fi

echo "models ready"
