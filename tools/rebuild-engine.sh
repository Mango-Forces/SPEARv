#!/usr/bin/env sh
set -eu
L=/home/ubuntu/mango-spearv
export PATH=$HOME/.cargo/bin:/usr/bin:/bin
export CARGO_HOME=$L/.build/cargo-home
export CARGO_TARGET_DIR=$L/.build/target-mango
export CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2
export SPEARV_CFITSIO_PREFIX=$L/.build/native/cfitsio-4.7.0-prefix
export SPEARV_PYTHON_PREFIX=$L/.build/runtime/install/opt/spearv/python
export PYO3_PYTHON=/home/ubuntu/mv-flux-data/venv/bin/python
index=spine/target/release/index-service
before=$(sha256sum "$index" 2>/dev/null || true)
flock /tmp/opencode/spearv-ab-20261113/measurement.lock taskset -c 16-19 \
  cargo build --release --offline --manifest-path spine/Cargo.toml -p spearv-binder -p spearv-index-service
cp -f "$CARGO_TARGET_DIR/release/spearv" "$CARGO_TARGET_DIR/release/index-service" spine/target/release/
if [ "$before" != "$(sha256sum "$index")" ]; then
  systemctl --user restart spearv-index
fi
systemctl --user restart spearv-engine
