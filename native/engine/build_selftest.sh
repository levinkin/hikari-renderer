#!/bin/bash
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
sdk="$(cd "$here/../.." && pwd)/sdk"
build="$here/build"
mkdir -p "$build"
(cd "$here/../server" && cargo build --release)
out="$(dirname "$(ls -t "$here"/../server/target/release/build/hikari-*/out/libhikari_engine.a | head -1)")"
clang++ -c -O2 -std=c++17 -I"$here" -fobjc-arc -DSELFTEST_MODEL="\"$sdk/Samples/Resources/Haru/Haru.model3.json\"" "$here/selftest.mm" -o "$build/selftest.o"
clang++ "$build/selftest.o" "$out/libhikari_engine.a" "$out/libcubism_framework.a" \
    -framework Metal -framework Foundation -framework ImageIO -framework CoreGraphics -framework AppKit -framework QuartzCore -lc++ -o "$build/engine_selftest"
