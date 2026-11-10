#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "${1:?bundle path required}" && pwd -P)"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

python3 "$REPO/utils/verify_gstreamer_macos.py" "$ROOT" --arch "$(uname -m)"

# Never reuse a host registry or let a developer framework mask missing bundle files.
unset DYLD_FALLBACK_LIBRARY_PATH DYLD_FRAMEWORK_PATH DYLD_FALLBACK_FRAMEWORK_PATH
unset DYLD_INSERT_LIBRARIES
export GST_PLUGIN_SYSTEM_PATH=""
export GST_PLUGIN_SYSTEM_PATH_1_0=""
export GST_PLUGIN_PATH="$ROOT/lib/gstreamer-1.0"
export GST_PLUGIN_PATH_1_0="$GST_PLUGIN_PATH"
export GST_PLUGIN_SCANNER="$ROOT/libexec/gstreamer-1.0/gst-plugin-scanner"
export GST_PLUGIN_SCANNER_1_0="$GST_PLUGIN_SCANNER"
export GST_REGISTRY="$WORK/registry.bin"
export GST_REGISTRY_1_0="$GST_REGISTRY"
export DYLD_LIBRARY_PATH="$ROOT/lib"
export GST_DEBUG_NO_COLOR=1
unset GST_DEBUG_FILE

LAUNCH="$ROOT/bin/gst-launch-1.0"
INSPECT="$ROOT/bin/gst-inspect-1.0"

"$LAUNCH" --version 2>&1 | tee "$WORK/version.log"
"$LAUNCH" fakesrc num-buffers=1 ! fakesink 2>&1 | tee "$WORK/startup.log"
"$ROOT/bin/gst-device-monitor-1.0" --version 2>&1 | tee "$WORK/device-version.log"
VERSION="$(awk '/^gst-launch-1.0 version / { print $3; exit }' "$WORK/version.log")"
if [[ -z "$VERSION" ]]; then
  echo "FAIL unable to determine runtime version" >&2
  exit 1
fi
if [[ -f "$ROOT/version.txt" && "$(cat "$ROOT/version.txt")" != "$VERSION" ]]; then
  echo "FAIL recorded asset version differs from runtime $VERSION" >&2
  exit 1
fi

for el in appsrc appsink h264parse h265parse vtdec vtdec_hw avdec_h264 \
  videoconvert videoscale glupload glcolorconvert glshader glimagesink osxvideosink \
  playbin3 hlsdemux2 souphttpsrc qtdemux tsdemux aacparse faad avdec_aac \
  audioconvert audioresample; do
  if "$INSPECT" "$el" >"$WORK/inspect-$el.log" 2>&1; then
    plugin_version="$(awk '$1 == "Version" { print $2; exit }' "$WORK/inspect-$el.log")"
    if [[ "$plugin_version" != "$VERSION" ]]; then
      echo "FAIL $el plugin version $plugin_version differs from runtime $VERSION" >&2
      exit 1
    fi
    echo "ok   $el ($plugin_version)"
  else
    cat "$WORK/inspect-$el.log" >&2
    echo "FAIL $el" >&2
    exit 1
  fi
done

python3 - "$REPO/scripts/gstreamer/testdata/black-16x16.h264.b64" "$WORK/test.h264" <<'PY'
import base64
from pathlib import Path
import sys
Path(sys.argv[2]).write_bytes(base64.b64decode(Path(sys.argv[1]).read_bytes()))
PY

# Synthetic black 16x16, three frames, generated with FFmpeg/libx264:
# ffmpeg -f lavfi -i color=c=black:s=16x16:r=1 -frames:v 3 -c:v libx264 \
#   -preset ultrafast -tune zerolatency -pix_fmt yuv420p -f h264 test.h264
# Registration alone is insufficient: demand decoded buffers and clean EOS.
for decoder in avdec_h264 vtdec; do
  if ! "$LAUNCH" -v filesrc location="$WORK/test.h264" ! h264parse ! "$decoder" \
    ! identity name=decoded silent=false ! fakesink sync=false \
    >"$WORK/decode-$decoder.log" 2>&1; then
    cat "$WORK/decode-$decoder.log" >&2
    exit 1
  fi
  if ! grep -E 'last-message = chain' "$WORK/decode-$decoder.log" >/dev/null; then
    cat "$WORK/decode-$decoder.log" >&2
    echo "FAIL $decoder produced no decoded video buffers" >&2
    exit 1
  fi
  echo "ok   $decoder decoded H.264 buffers to EOS"
done

if grep -Ei 'Failed to load plugin|Library not loaded|wrong architecture' "$WORK"/*.log; then
  echo "FAIL plugin loading errors in isolated bundle" >&2
  exit 1
fi
echo "Packaged GStreamer plugin and H.264 decode checks passed"
