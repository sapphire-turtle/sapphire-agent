#!/usr/bin/env bash
# Fail unless the binary at $1 links sherpa-onnx statically (the shared
# release build's `post-build`, which passes the built file in $BIN).
#
# Under shared linking the libraries stay in `target/` and nothing writes an
# RPATH, so the uploaded binary dies at every install site with
# "libsherpa-onnx-c-api.so: cannot open shared object file". That shipped once
# already (#182) because only Windows packaged the libraries beside the
# executable.
set -euo pipefail

bin="$1"
case "$RUNNER_OS" in
  Linux) deps=$(ldd "$bin") ;;
  macOS) deps=$(otool -L "$bin") ;;
  # No dumpbin without the VS dev shell, so use a proxy: sherpa-onnx-sys drops
  # its DLLs next to the exe only when built shared, so finding one is the
  # failure this guards.
  Windows) deps=$(ls "$(dirname "$bin")"/*.dll 2>/dev/null || true) ;;
  *) echo "::error::no dynamic-link check for $RUNNER_OS"; exit 1 ;;
esac
if grep -i sherpa <<<"$deps"; then
  echo "::error::$bin links sherpa-onnx dynamically; this asset would not start"
  exit 1
fi
echo "$bin links no sherpa-onnx library — self-contained."
