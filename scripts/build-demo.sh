#!/usr/bin/env bash
# ==============================================================================
# xstory - Full Production Asset Automation Pipeline
# 1. Optimizes 4K Retina Banner (assets/images/xstory-banner.jpg / .png)
# 2. Encodes Master MP4 from raw screen recording (.mov)
# 3. Compiles High-Fidelity Bayer Dithered GIF (assets/gif/xstory-preview.gif)
# 4. Compresses payloads via gifsicle / sips
# ==============================================================================

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OUTPUT_DIR="assets"
mkdir -p "$OUTPUT_DIR/gif" "$OUTPUT_DIR/video" "$OUTPUT_DIR/images"

echo "=========================================================="
echo "  xstory Production Assets Pipeline (Full Automation)     "
echo "=========================================================="

# ── 1. Banner Processing (4K Retina) ───────────────────────────
echo ""
echo "── [1/3] Processing 4K Retina Banner ──"

BANNER_SRC=""
# Auto-detect existing banner files (handling typo xstoy -> xstory)
if [[ -f "$OUTPUT_DIR/images/xstoy-banner.jpg" ]]; then
    mv -f "$OUTPUT_DIR/images/xstoy-banner.jpg" "$OUTPUT_DIR/images/xstory-banner.jpg"
    BANNER_SRC="$OUTPUT_DIR/images/xstory-banner.jpg"
elif [[ -f "$OUTPUT_DIR/images/xstory-banner.jpg" ]]; then
    BANNER_SRC="$OUTPUT_DIR/images/xstory-banner.jpg"
elif [[ -f "$OUTPUT_DIR/images/xstory-banner.png" ]]; then
    BANNER_SRC="$OUTPUT_DIR/images/xstory-banner.png"
elif [[ -f "$OUTPUT_DIR/images/xstoy-banner.png" ]]; then
    mv -f "$OUTPUT_DIR/images/xstoy-banner.png" "$OUTPUT_DIR/images/xstory-banner.png"
    BANNER_SRC="$OUTPUT_DIR/images/xstory-banner.png"
fi

if [[ -n "$BANNER_SRC" && -f "$BANNER_SRC" ]]; then
    echo "Found banner: $BANNER_SRC"
    # Ensure Retina 4K scaling (max width 3840px, keeping aspect ratio)
    if command -v sips >/dev/null 2>&1; then
        sips -Z 3840 "$BANNER_SRC" >/dev/null 2>&1 || true
    fi
    echo "✓ 4K Retina banner ready: $BANNER_SRC"
else
    echo "Notice: No banner found in $OUTPUT_DIR/images/. Skipping banner optimization."
fi

# ── 2. Screen Recording Video & GIF Processing ─────────────────
echo ""
echo "── [2/3] Processing Screen Recording (.mov -> .mp4 + .gif) ──"

INPUT_VIDEO="${1:-}"
if [[ -z "$INPUT_VIDEO" ]]; then
    # Auto-detect video source in assets/video/ or current directory
    if [[ -f "$OUTPUT_DIR/video/xstory.mov" ]]; then
        INPUT_VIDEO="$OUTPUT_DIR/video/xstory.mov"
    elif [[ -f "$OUTPUT_DIR/video/xstory-demo.mov" ]]; then
        INPUT_VIDEO="$OUTPUT_DIR/video/xstory-demo.mov"
    else
        INPUT_VIDEO=$(ls -t "$OUTPUT_DIR/video/"*.mov *.mov ~/Desktop/Screen\ Recording*.mov 2>/dev/null | head -n 1 || true)
    fi
fi

if [[ -z "$INPUT_VIDEO" || ! -f "$INPUT_VIDEO" ]]; then
    echo "Error: No screen recording found. Place xstory.mov in $OUTPUT_DIR/video/ or pass file as argument."
    exit 1
fi

echo "Source video: $INPUT_VIDEO"

MP4_OUT="$OUTPUT_DIR/video/xstory-demo.mp4"
GIF_RAW="$OUTPUT_DIR/gif/xstory-raw.gif"
GIF_FINAL="$OUTPUT_DIR/gif/xstory-preview.gif"
PALETTE_PNG="$OUTPUT_DIR/palette.png"

START_OFFSET="${2:-}"
DURATION="${3:-}"

TIME_FLAGS=()
if [[ -n "$START_OFFSET" ]]; then
    TIME_FLAGS+=("-ss" "$START_OFFSET")
fi
if [[ -n "$DURATION" ]]; then
    TIME_FLAGS+=("-t" "$DURATION")
fi

# Step A: Encode Master MP4 (1280px width, H.264, 12 FPS, CRF 23, slow preset)
echo "Encoding Master MP4 (1280px, H.264, CRF 23)..."
ffmpeg -y ${TIME_FLAGS[@]+"${TIME_FLAGS[@]}"} -i "$INPUT_VIDEO" \
  -vf "scale=1280:-2:flags=lanczos" \
  -r 12 -c:v libx264 -preset slow -crf 23 -pix_fmt yuv420p -an \
  "$MP4_OUT"

# Step B: Generate 256-Color Palette (Lanczos filter, diff stats mode)
echo "Generating optimal 256-color palette (Lanczos)..."
ffmpeg -y -i "$MP4_OUT" \
  -vf "fps=12,scale=800:-2:flags=lanczos,format=rgb24,palettegen=stats_mode=diff" \
  -frames:v 1 -update 1 "$PALETTE_PNG"

# Step C: Render High-Fidelity Dithered GIF (Bayer algorithm)
echo "Rendering dithered GIF (Bayer)..."
ffmpeg -y -i "$MP4_OUT" -i "$PALETTE_PNG" \
  -filter_complex "fps=12,scale=800:-2:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=3" \
  -loop 0 "$GIF_RAW"
rm -f "$PALETTE_PNG"

# Step D: Gifsicle compression (if installed)
if command -v gifsicle >/dev/null 2>&1; then
    echo "Optimizing GIF payload via Gifsicle (O3, lossy=80)..."
    gifsicle -O3 --lossy=80 --colors 256 "$GIF_RAW" -o "$GIF_FINAL"
    rm -f "$GIF_RAW"
else
    mv "$GIF_RAW" "$GIF_FINAL"
fi

# ── 3. Summary & Verification ──────────────────────────────────
echo ""
echo "── [3/3] Production Artifacts Ready ──"
ls -lh "$OUTPUT_DIR"/images/xstory-banner.* "$MP4_OUT" "$GIF_FINAL" 2>/dev/null || true
echo ""
echo "✓ Pipeline completed successfully."
