#!/bin/sh
# Builds rua, then runs the C embedding demo and the Rust plugin demo.
set -e
cd "$(dirname "$0")/.."

echo "== building rua =="
cargo build --release --workspace

echo
echo "== C embedding (demo/embed.c) =="
cc demo/embed.c -I include -L target/release -lrua -lm -o target/release/embed
LD_LIBRARY_PATH=target/release target/release/embed

echo
echo "== Rust plugin over the C ABI (demo/plugin.rs) =="
rustc -O --crate-type cdylib demo/plugin.rs -o demo/libruaplugin.so
./target/release/rua demo/plugin.rua
