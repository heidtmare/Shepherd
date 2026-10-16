#!/usr/bin/env bash
# Downloads runtime dependencies for form recognition into vendor/ (host OS/arch):
#   vendor/pdfium/lib/libpdfium.{so,dylib}         PDF rasterizer (bblanchon/pdfium-binaries)
#   vendor/models/trocr-base-handwritten/*.onnx     handwriting recognizer (fp32; quantized loses accuracy)
# Idempotent. Override versions with PDFIUM_RELEASE / TROCR_REPO.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PDFIUM_RELEASE=${PDFIUM_RELEASE:-chromium/8066}
TROCR_REPO=${TROCR_REPO:-Xenova/trocr-base-handwritten}
DEST=${VENDOR_DIR:-$ROOT/vendor}

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) plat=mac-arm64 ;;
  Darwin-x86_64) plat=mac-x64 ;;
  Linux-x86_64) plat=linux-x64 ;;
  Linux-aarch64|Linux-arm64) plat=linux-arm64 ;;
  *) echo "unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

if ! ls "$DEST"/pdfium/lib/libpdfium.* >/dev/null 2>&1; then
  mkdir -p "$DEST/pdfium"
  echo "pdfium $PDFIUM_RELEASE ($plat)"
  curl -fsSL "https://github.com/bblanchon/pdfium-binaries/releases/download/$PDFIUM_RELEASE/pdfium-$plat.tgz" | tar xz -C "$DEST/pdfium"
fi

model_dir="$DEST/models/trocr-base-handwritten"
mkdir -p "$model_dir"
for f in onnx/encoder_model.onnx onnx/decoder_model.onnx tokenizer.json generation_config.json preprocessor_config.json; do
  out="$model_dir/$(basename "$f")"
  [[ -s "$out" ]] && continue
  echo "model $f"
  curl -fsSL -o "$out.part" "https://huggingface.co/$TROCR_REPO/resolve/main/$f" && mv "$out.part" "$out"
done
echo "ok: $DEST"
