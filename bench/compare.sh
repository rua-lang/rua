#!/bin/sh
# Best-of-N across the engines, with the LuaJIT ratio spelled out.
#
# `run.sh` is the correctness gate and prints one run. This is for judging a
# change: it warms the JIT's disk cache first (otherwise the first timing is
# mostly `rustc`), takes the best of N runs per engine, and reports the ratio
# that matters. Wall time on a loaded machine is noisy — `--insn` counts
# instructions instead, which is not.
set -e
cd "$(dirname "$0")/.."
RUA=./target/release/rua
N=${N:-5}
BENCHES=${BENCHES:-"nbody binarytrees spectralnorm fannkuch queens matmul wordfreq lisp"}
SIZE_nbody=200000 SIZE_binarytrees=16 SIZE_spectralnorm=500
SIZE_fannkuch=9 SIZE_queens=11 SIZE_matmul=200 SIZE_wordfreq=20000
SIZE_lisp=3

# the seconds a run reports, or the instructions it retires
best() {
    lo=""
    i=0
    while [ $i -lt "$N" ]; do
        if [ "$MODE" = insn ]; then
            v=$(perf stat -e instructions "$@" 2>&1 >/dev/null |
                sed -n 's/^ *\([0-9,]*\) *instructions.*/\1/p' | tr -d ,)
        else
            v=$("$@" 2>/dev/null | sed -n 's/.* in \([0-9.]*\)s$/\1/p')
        fi
        [ -n "$v" ] || v=0
        if [ -z "$lo" ] || [ "$(echo "$v < $lo" | bc -l)" = 1 ]; then lo=$v; fi
        i=$((i + 1))
    done
    echo "$lo"
}

MODE=time
[ "$1" = "--insn" ] && MODE=insn

printf '%-14s %9s %9s %9s %9s %9s\n' benchmark rua-interp rua-jit lua5.4 luajit vs-luajit
for b in $BENCHES; do
    size=$(eval echo \$SIZE_$b)
    $RUA bench/$b.rua "$size" >/dev/null 2>&1        # warm the JIT cache
    i=$(best $RUA --no-jit bench/$b.rua "$size")
    j=$(best $RUA bench/$b.rua "$size")
    l=$(best lua5.4 bench/$b.lua "$size")
    L=$(best luajit bench/$b.lua "$size")
    rua=$(echo "if ($i < $j) $i else $j" | bc -l)
    ratio=$(echo "scale=2; $rua / $L" | bc -l 2>/dev/null || echo -)
    printf '%-14s %9s %9s %9s %9s %8sx\n' "$b" "$i" "$j" "$l" "$L" "$ratio"
done
