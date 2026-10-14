#!/usr/bin/env bash
# Drop the sherpa-onnx prebuilt native libraries that a restored rust-cache
# gutted, so sherpa-onnx-sys's build script downloads them again. Run after
# the cargo cache is restored, before anything builds (the shared workflows'
# `pre-build`).
#
# sherpa-onnx-sys downloads its prebuilt native libraries into
# target/sherpa-onnx-prebuilt/<archive>/lib and emits a -L flag pointing at
# that directory.
#
# Swatinem/rust-cache deletes every file under target/ it does not recognise,
# recursing through directories it does not own. A restored cache therefore
# brings that tree back with the directories intact and the libraries gone,
# while sherpa-onnx-sys's cached build-script output still names the
# directory. Cargo sees the build script as fresh, never re-downloads, and the
# link fails:
#
#   rust-lld: error: unable to find library -lsherpa-onnx-c-api
#
# Dropping the tree together with sherpa-onnx-sys's own artifacts makes the
# build script re-run and re-download. On a cold cache the directory is absent
# and this is a no-op.
set -euo pipefail

if [ -d target/sherpa-onnx-prebuilt ]; then
  echo "Restored cache contains a gutted target/sherpa-onnx-prebuilt; refreshing it."
  rm -rf target/sherpa-onnx-prebuilt
  cargo clean -p sherpa-onnx-sys || true
  cargo clean -p sherpa-onnx-sys --release || true
else
  echo "No target/sherpa-onnx-prebuilt to refresh."
fi
