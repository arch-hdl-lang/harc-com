#!/bin/bash
# v1 <-> TB-IR EMITTED-TEXT parity across the whole fixture corpus.
#
# This is deliberately NOT run_tbir_equiv.sh. That script trace-diffs two
# simulations, which is the stronger check but needs Verilator, needs both
# backends to actually build and run, and is therefore blind to the case
# where one backend refuses to emit at all. A one-sided rejection is
# exactly the shape harc#559 shipped and CI missed: v1 emitted a program
# the default backend rejected, and every job stayed green because no
# fixture exercised the shape and nothing compared acceptance.
#
# This script needs no Verilator, no Z3 and no simulation — just
# `--emit-only` under both backends — so it runs in the fast check job and
# covers every fixture, including ones the sim jobs cannot run.
#
# Two properties, per fixture:
#
#   1. ACCEPTANCE PARITY. Both backends must agree on whether the program
#      emits at all. The one allowed asymmetry is TB-IR's documented
#      subset gap: a `LowerError::Unsupported` names `--codegen v1` as the
#      escape hatch, and those are tracked separately (harc#548). Any
#      other disagreement — v1 emits and TB-IR errors for some other
#      reason, or TB-IR emits and v1 does not — is a failure.
#
#   2. CONSTRAINT-TEXT PARITY. The §2.4 wrap mask and the whole solver
#      lowering live in a randomize emitter both backends call, but they
#      construct it differently (`build_randomize_emitter` has no per-test
#      statement state). Every `_s.add(...)` line must therefore be
#      byte-identical between the two. The surrounding scaffolding is
#      legitimately different — roughly 200 lines per file — so comparing
#      whole files would be pure noise; the assertion lines are the part
#      that has to agree.
#
# Run from harc-com repo root:
#     ./tests/run_emit_parity.sh
#
# Set HARC=/path/to/harc to override the binary (default: build with
# `cargo build --release --bin harc`).
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HARC="${HARC:-./target/release/harc}"
DUT_DIR="tests/dut"
FIX_DIR="tests/fixtures"
JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"

if [ ! -x "$HARC" ]; then
    echo "Building harc..."
    cargo build --release --bin harc
fi
case "$HARC" in
    /*) ;;
    *) HARC="$PWD/$HARC" ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Shared with run_fixtures.sh — one source of truth for what exists and
# how to invoke it. Row: <name> | <top> | <sv> | <extra harc> | <ref_src> | <test_struct>
TABLE="$SCRIPT_DIR/fixtures.tbl"
# Fail CLOSED. Before the table was extracted it was a heredoc and could
# not go missing; now a bad path would make this gate report success over
# an empty corpus.
[ -s "$TABLE" ] || { echo "error: $TABLE missing or empty" >&2; exit 1; }
FIXTURES="$(cat "$TABLE")"

# Flatten a miette-rendered diagnostic to one line: strip the box-drawing
# continuation gutter and collapse whitespace. miette hard-wraps at 80
# columns when stdout is not a TTY, so a phrase we need to match can be
# split across lines depending on how long the construct name happens to
# be. Matching the flattened form removes that dependency.
flatten_log() {
    printf '%s' "$1" | tr '\n' ' ' | sed 's/[│┌└─╭╰]/ /g' | tr -s ' '
}

# Is a TB-IR rejection a DECLARED subset gap, i.e. one where `--codegen
# v1` is a real escape hatch?
#
# It is NOT enough to look for the flag. `LowerError` deliberately
# distinguishes four cases and all four name it (src/ir/lower/mod.rs):
#
#   Unsupported                        "; re-run with `--codegen v1`"
#   NotImplemented/Rejects             "`--codegen v1` does not implement it either"
#   NotImplemented/EmitsUncompilable   "`--codegen v1` accepts it but emits C++ that does not compile"
#   NotImplemented/SilentlyMisLowers   "`--codegen v1` accepts it but silently emits something else"
#
# Only the first is an escape hatch. The last two mean v1 is BROKEN on
# the construct — v1 emits, TB-IR refuses, and v1's output is known bad.
# That is the single most dangerous shape this gate exists to surface, so
# matching the bare flag would auto-exempt exactly the wrong class. Match
# the escape-hatch phrase itself.
is_subset_gap() {
    flatten_log "$1" | grep -q -- 're-run with `--codegen v1`'
}

KNOWN="$SCRIPT_DIR/emit_parity_known.txt"

# known_exemption <name> <direction> -> 0 if this asymmetry is a recorded
# decision. Rows carry an issue number; see the file header.
known_exemption() {
    [ -f "$KNOWN" ] || return 1
    grep -vE '^\s*(#|$)' "$KNOWN" | while IFS='|' read -r n d rest; do
        [ "$(echo "$n" | xargs)" = "$1" ] && [ "$(echo "$d" | xargs)" = "$2" ] && echo HIT
    done | grep -q HIT
}

run_one() { # run_one <outdir> <row>
    local outdir="$1" row="$2"
    local name top svs extra ref tstruct
    IFS='|' read -r name top svs extra ref tstruct <<<"$row"
    name="$(echo "$name" | xargs)"; top="$(echo "$top" | xargs)"
    svs="$(echo "$svs" | xargs)"; extra="$(echo "$extra" | xargs)"
    tstruct="$(echo "$tstruct" | xargs)"
    [ -z "$name" ] && return 0

    local args=("$FIX_DIR/$name.harc")
    for f in $extra; do args+=("$FIX_DIR/$f"); done
    local svargs=()
    for f in $svs; do svargs+=(--sv "$DUT_DIR/$f"); done
    local targs=()
    [ -n "$tstruct" ] && targs=(--test "$tstruct")

    local rc=() ; local dirs=()
    for cg in v1 tbir; do
        local d="$outdir/$cg"; mkdir -p "$d"; dirs+=("$d")
        "$HARC" sim "${args[@]}" "${svargs[@]}" --top "$top" --emit-only \
            --codegen "$cg" --outdir "$d" "${targs[@]}" >"$d/.log" 2>&1
        rc+=("$?")
    done

    # 1. Acceptance parity.
    if [ "${rc[0]}" != "${rc[1]}" ]; then
        if [ "${rc[0]}" = "0" ] && is_subset_gap "$(cat "${dirs[1]}/.log")"; then
            echo "  SKIP  $name  (TB-IR subset gap, harc#548)"
            echo "__STATUS__ SKIP $name"
            return 0
        fi
        local dir="v1-only"
        [ "${rc[0]}" != "0" ] && dir="tbir-only"
        if known_exemption "$name" "$dir"; then
            echo "  KNOWN $name  ($dir asymmetry, see tests/emit_parity_known.txt)"
            echo "__STATUS__ KNOWN $name"
            return 0
        fi
        echo "  FAIL  $name  (acceptance differs [$dir]: v1 rc=${rc[0]}, tbir rc=${rc[1]})"
        echo "    v1:   $(head -c 400 "${dirs[0]}/.log" | tr '\n' ' ')"
        echo "    tbir: $(head -c 400 "${dirs[1]}/.log" | tr '\n' ' ')"
        echo "__STATUS__ FAIL $name"
        return 0
    fi
    if [ "${rc[0]}" != "0" ]; then
        # Both reject, consistently — that is the property, not a failure.
        echo "  SKIP  $name  (both backends reject)"
        echo "__STATUS__ SKIP $name"
        return 0
    fi

    # 2. Constraint-text parity. Ordered, and including push/pop: Z3's
    # returned model depends on assertion order, and an `_s.add` that
    # moves across a `push` boundary changes which assertions survive the
    # matching `pop`. Sorting would call both of those identical.
    if ! ls "${dirs[0]}"/*.cpp >/dev/null 2>&1 || ! ls "${dirs[1]}"/*.cpp >/dev/null 2>&1; then
        echo "  FAIL  $name  (emitted no .cpp despite exit 0)"
        echo "__STATUS__ FAIL $name"
        return 0
    fi
    local a b
    a="$(cat "${dirs[0]}"/*.cpp | grep -oE '_s\.(add|push|pop)\(.*')"
    b="$(cat "${dirs[1]}"/*.cpp | grep -oE '_s\.(add|push|pop)\(.*')"
    if [ "$a" != "$b" ]; then
        echo "  FAIL  $name  (solver constraint text differs between backends)"
        diff <(printf '%s\n' "$a") <(printf '%s\n' "$b") | head -12 | sed 's/^/    /'
        echo "__STATUS__ FAIL $name"
        return 0
    fi

    # Report whether the text half actually compared anything. Most
    # fixtures have no solver block at all, so a bare "PASS" would
    # overstate what was checked.
    if [ -n "$a" ]; then
        echo "  PASS  $name  (acceptance + $(printf '%s\n' "$a" | wc -l | xargs) solver lines)"
        echo "__STATUS__ PASSC $name"
    else
        echo "  PASS  $name  (acceptance)"
        echo "__STATUS__ PASS $name"
    fi
}

if [ "${1:-}" = "--worker" ]; then
    run_one "$2" "$(cat "$3")"
    exit 0
fi

echo "Running $FIX_DIR v1-vs-tbir emitted-text parity (JOBS=$JOBS)..."
RESDIR="$TMP/rows"; mkdir -p "$RESDIR"
n=0
while IFS= read -r row; do
    [ -z "$(echo "$row" | xargs)" ] && continue
    printf '%s\n' "$row" >"$RESDIR/$n.row"
    n=$((n + 1))
done <<<"$FIXTURES"

i=0
while [ "$i" -lt "$n" ]; do
    running=0
    while [ "$i" -lt "$n" ] && [ "$running" -lt "$JOBS" ]; do
        mkdir -p "$TMP/o$i"
        "$0" --worker "$TMP/o$i" "$RESDIR/$i.row" >"$TMP/o$i.out" 2>&1 &
        i=$((i + 1)); running=$((running + 1))
    done
    wait
done

pass=0; fail=0; skip=0; known=0; passc=0; nostatus=0
for j in $(seq 0 $((n - 1))); do
    if [ ! -f "$TMP/o$j.out" ]; then
        echo "  LOST  worker $j produced no output"
        nostatus=$((nostatus + 1)); continue
    fi
    grep -v '^__STATUS__' "$TMP/o$j.out"
    case "$(grep '^__STATUS__' "$TMP/o$j.out" | awk '{print $2}')" in
        PASSC) passc=$((passc + 1)) ;;
        PASS) pass=$((pass + 1)) ;;
        FAIL) fail=$((fail + 1)) ;;
        SKIP) skip=$((skip + 1)) ;;
        KNOWN) known=$((known + 1)) ;;
        *) echo "  LOST  worker $j produced no status line"
           nostatus=$((nostatus + 1)) ;;
    esac
done

echo
echo "Result: $((pass + passc)) acceptance-parity (of which $passc also compared solver text),"
echo "        $fail divergent, $known known-exempt, $skip skipped (subset gap or both-reject),"
echo "        $nostatus lost"
# Most fixtures have no solver block, so the text half covers a minority
# of the corpus by construction. Say so rather than letting the headline
# number imply corpus-wide emitted-text parity.
[ "$fail" -eq 0 ] && [ "$nostatus" -eq 0 ] && [ "$n" -gt 0 ]
