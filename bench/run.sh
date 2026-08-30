#!/bin/sh
# Run the benchmark suite across rua (interpreted and JIT'd), lua5.4 and luajit.
#
# Every benchmark prints its result lines first and a `# name ... in NNNs` line
# last. The result lines must be byte-identical across every engine before a
# timing is believed: a fast wrong answer is not a benchmark.
set -e
cd "$(dirname "$0")/.."
RUA=./target/release/rua
BENCHES="nbody binarytrees spectralnorm fannkuch queens matmul wordfreq"
SIZE_nbody=200000 SIZE_binarytrees=16 SIZE_spectralnorm=500
SIZE_fannkuch=9 SIZE_queens=11 SIZE_matmul=200 SIZE_wordfreq=20000

printf '%-14s %10s %10s %10s %10s\n' benchmark rua-interp rua-jit lua5.4 luajit
for b in $BENCHES; do
    size=$(eval echo \$SIZE_$b)
    out_i=$($RUA --no-jit bench/$b.rua $size)
    # warm the JIT's on-disk cache first: a cold run pays rustc, which is a
    # real cost but not the one this table is about
    $RUA bench/$b.rua $size > /dev/null
    out_j=$($RUA bench/$b.rua $size)
    out_l=$(lua5.4 bench/$b.lua $size 2>/dev/null || echo MISSING)
    out_L=$(luajit bench/$b.lua $size 2>/dev/null || echo MISSING)

    # correctness first: the answers have to match
    for pair in "rua-jit:$out_j" "lua5.4:$out_l" "luajit:$out_L"; do
        name=${pair%%:*}
        body=${pair#*:}
        [ "$body" = "MISSING" ] && continue
        if [ "$(echo "$body" | grep -v '^#')" != "$(echo "$out_i" | grep -v '^#')" ]; then
            echo "MISMATCH in $b: $name disagrees with rua-interp" >&2
            echo "$out_i" | grep -v '^#' > /tmp/rua-bench-expected.txt
            echo "$body" | grep -v '^#' > /tmp/rua-bench-got.txt
            diff /tmp/rua-bench-expected.txt /tmp/rua-bench-got.txt >&2 || true
            exit 1
        fi
    done

    t() { echo "$1" | sed -n 's/.* in \([0-9.]*\)s$/\1/p'; }
    printf '%-14s %10s %10s %10s %10s\n' "$b" "$(t "$out_i")" "$(t "$out_j")" "$(t "$out_l")" "$(t "$out_L")"
done
