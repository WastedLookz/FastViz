#!/bin/sh
# Creates a folder of test media for checking formats by eye:
#   images/  one file per tricky image case (rotation, CMYK, alpha, 16-bit,
#            animated GIF, AVIF, broken and mislabeled files, ...)
#   videos/  one clip per container, 10-bit HEVC and +-90 degree rotation
# Usage: tools/make-test-media.sh <empty-dir>   (needs python3-pil and ffmpeg)
set -eu
out=${1:?usage: $0 <dir>}
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out/images" "$out/videos"

python3 -I "$here/make_test_images.py" "$out/images"
ffmpeg -hide_banner -loglevel error -y -f lavfi -i "testsrc2=size=1280x720:duration=1" \
    -frames:v 1 -c:v libaom-av1 -still-picture 1 "$out/images/still.avif"

cd "$out/videos"
src="testsrc2=size=640x360:rate=25:duration=6"
clip() { ffmpeg -hide_banner -loglevel error -y -f lavfi -i "$src" "$@"; }
clip -c:v libx264 -pix_fmt yuv420p clip.mp4
clip -c:v libx264 -pix_fmt yuv420p clip.mov
clip -c:v libx264 -pix_fmt yuv420p clip.m4v
clip -c:v libx264 clip.mkv
clip -c:v libvpx -b:v 1M clip.webm
clip -c:v mpeg4 clip.avi
clip -c:v wmv2 clip.wmv
clip -c:v flv1 clip.flv
clip -c:v mpeg2video clip.mpg
clip -c:v mpeg2video -f mpegts clip.ts
clip -c:v libx264 -f mpegts clip.m2ts
clip -c:v libtheora clip.ogv
clip -c:v h263 -s 352x288 clip.3gp
clip -c:v libx265 -pix_fmt yuv420p10le -x265-params log-level=error clip_hevc10.mp4
ffmpeg -hide_banner -loglevel error -y -display_rotation 90 -i clip.mp4 -c copy rot90.mp4
ffmpeg -hide_banner -loglevel error -y -display_rotation -90 -i clip.mp4 -c copy rot270.mp4
echo "Test media in $out"
