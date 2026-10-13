#!/bin/sh
# Renders the README films: a 240fps PNG sequence per film (HyperFrames), blended down to 60fps,
# then a looping lossless animated WebP at full 1920x1080 for inline playback on GitHub, plus an
# MP4 for sharing. Full size matters: on high-density screens GitHub draws the film about 1,800
# pixels wide, and anything smaller gets stretched and soft.
# Usage: ./render.sh [film...]   (default: all films)
#
# Each 60fps frame averages the four 240fps frames it spans, the way a camera shutter does.
# Without that motion blur, anything that travels more than a few pixels per frame strobes
# and the film reads as choppy even at 60fps.
#
# Every step is lossless up to the WebP on purpose. Lossy WebP stores mostly partial frames,
# and on slow fades of low-contrast UI those leave blocky trails behind.
set -eu

cd "$(dirname "$0")"
films=${*:-"hero how agents servers"}
masters=${MASTERS:-renders}
out=../films-v2
mkdir -p "$masters" "$out"

for film in $films; do
    sub="$masters/$film-240"
    frames="$masters/$film-frames"
    rm -rf "$sub" "$frames"
    npx --yes hyperframes@0.8.150 render -c "$film.html" --fps 240 --format png-sequence --quiet \
        -o "$sub"
    mkdir -p "$frames"
    ffmpeg -v error -y -framerate 240 -i "$sub/frame_%06d.png" \
        -vf "tmix=frames=4,select='eq(mod(n\,4)\,3)',setpts=N/(60*TB)" -r 60 \
        "$frames/frame_%06d.png"
    rm -rf "$sub"
    ffmpeg -v error -y -framerate 60 -i "$frames/frame_%06d.png" \
        -vf "format=bgra" \
        -c:v libwebp_anim -lossless 1 -compression_level 4 -loop 0 \
        "$out/$film.webp"
    ffmpeg -v error -y -framerate 60 -i "$frames/frame_%06d.png" \
        -c:v libx264 -preset slow -crf 12 -pix_fmt yuv420p -movflags +faststart \
        "$masters/$film.mp4"
    echo "$film: $(du -h "$out/$film.webp" | cut -f1)"
done
