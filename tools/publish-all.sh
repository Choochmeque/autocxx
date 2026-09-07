#!/bin/bash

set -e

DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )/.."

DIRS="$DIR/parser $DIR/engine $DIR/macro $DIR $DIR/gen/build $DIR/integration-tests $DIR/gen/cmd"

# autocxx-engine builds bindgen from a git submodule, and `cargo package` prunes
# any directory below the package root holding a `Cargo.toml` - which the
# submodule does - so the sources have to be flattened out of it first, with
# those manifests renamed. The result is deliberately left untracked, so the
# publish is `--allow-dirty`; nothing of bindgen's is committed to this repo.
echo "Vendoring bindgen sources for publishing"
AUTOCXX_VENDOR_BINDGEN=1 cargo build -p autocxx-engine
test -f "$DIR/engine/third_party/bindgen-src/lib.rs"

for CRATE in $DIRS; do
  pushd $CRATE
  echo "Publish: $CRATE"
  cargo publish --allow-dirty
  popd
done
