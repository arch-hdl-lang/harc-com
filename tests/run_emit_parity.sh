#!/bin/bash
# v1 <-> TB-IR EMITTED-TEXT parity across the shared fixture table.
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
# `--emit-only` under both backends — so it runs in the fast check job,
# in seconds, and does not need a fixture to be runnable to compare it.
# It covers the shared fixture table (tests/fixtures.tbl), which is the
# same set run_fixtures.sh drives — not every file in tests/fixtures/.
#
# Two properties, per fixture:
#
#   1. ACCEPTANCE PARITY. Both backends must agree on whether the program
#      emits at all. The only allowed asymmetries are TB-IR's declared
#      subset gaps, which say so in the diagnostic by offering v1 as a
#      real escape hatch — see `is_subset_gap` below, and note that
#      merely naming `--codegen v1` is NOT the criterion. Those are
#      tracked separately (harc#548). Any other disagreement, in either
#      direction, is a failure unless it has a row in
#      tests/emit_parity_known.txt naming an issue.
#
#   2. CONSTRAINT-TEXT PARITY. The §2.4 wrap mask and the whole solver
#      lowering live in a randomize emitter both backends call, but they
#      construct it differently (`build_randomize_emitter` has no per-test
#      statement state). The ordered `_s.add/push/pop/check/set(...)`
#      sequence must be byte-identical between the two — `check()` in
#      particular, since it is the call that consumes the assertions and
#      soft-constraint lowering interleaves it between adds. The surrounding
#      scaffolding is legitimately different — roughly 200 lines per file
#      — so comparing whole files would be pure noise.
#
#      Scope honestly: only ~15 of the ~148 rows emit any solver text at
#      all. For the rest this half compares nothing and the run reports
#      acceptance parity only. The summary line prints both counts.
#
# Run from harc-com repo root:
#     ./tests/run_emit_parity.sh
#
# Set HARC=/path/to/harc to override the binary (default: build with
# `cargo build --release --bin harc`).
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Run from the repo root regardless of where we were invoked. Several
# paths the compiler resolves (SV includes, ref sources) are relative to
# cwd, so a run from elsewhere silently degraded into "both backends
# reject" for much of the corpus — which this gate then scored as skips.
# Resolve a relative $HARC against the INVOKING directory before the cd,
# or `HARC=../target/debug/harc` from tests/ would silently mean something
# else once cwd changes.
_INVOKED_FROM="$PWD"
HARC="${HARC:-$ROOT/target/release/harc}"
case "$HARC" in /*) ;; *) HARC="$_INVOKED_FROM/$HARC" ;; esac
cd "$ROOT" || { echo "error: cannot cd to $ROOT" >&2; exit 1; }
# Absolute, so running from anywhere behaves the same. Relative paths made
# every fixture unreadable from a subdirectory, which the gate then scored
# as "both backends reject" — a green run over nothing.
DUT_DIR="$ROOT/tests/dut"
FIX_DIR="$ROOT/tests/fixtures"
JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"

if [ ! -x "$HARC" ]; then
    echo "Building harc..."
    cargo build --release --bin harc || {
        echo "error: cargo build failed" >&2; exit 1; }
fi
# Without this every row gets rc=127 from both backends, the codes match,
# and all of them take the "both backends reject" path — a green run over
# a corpus that was never compiled.
[ -x "$HARC" ] || { echo "error: $HARC is not executable" >&2; exit 1; }
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

# Collapse a miette-rendered diagnostic to a whitespace-free string:
# strip the box-drawing continuation gutter, then remove ALL whitespace.
#
# miette hard-wraps at 80 columns when stdout is not a TTY, and it breaks
# at hyphens as well as spaces — so `re-run` can arrive as `re-` + newline
# + `run`, and `--codegen` can split too. Joining lines with a space is
# not enough; matching a needle with the whitespace removed as well makes
# the match independent of where any wrap lands.
flatten_log() {
    printf '%s' "$1" | sed 's/[│┌└─╭╰╮╯├┤┬┴]//g' | tr -d '[:space:]'
}

# Is a rejection one where `--codegen v1` is a REAL escape hatch?
#
# It is not enough to look for the flag. Five distinct diagnostics name
# it, and only two of them mean "use v1 instead":
#
#   ESCAPE HATCHES
#     LowerError::Unsupported     "; re-run with `--codegen v1`"
#     tbir emitter (EmitError)    "use --codegen v1 for wide shifts"
#                                 (src/codegen/tbir/expr.rs)
#   NOT ESCAPE HATCHES — v1 is broken on the construct
#     NotImplemented/Rejects            "`--codegen v1` does not implement it either"
#     NotImplemented/EmitsUncompilable  "`--codegen v1` accepts it but emits C++ that does not compile"
#     NotImplemented/SilentlyMisLowers  "`--codegen v1` accepts it but silently emits something else"
#
# The last two are the single most dangerous shape this gate exists to
# surface: v1 emits, the default backend refuses, and v1's output is
# known bad. Matching the bare flag would auto-exempt exactly those.
is_subset_gap() {
    local f
    f="$(flatten_log "$1")"
    case "$f" in
        *'re-runwith`--codegenv1`'*) return 0 ;;
        *'use--codegenv1for'*)       return 0 ;;
    esac
    return 1
}

KNOWN="$SCRIPT_DIR/emit_parity_known.txt"

# known_exemption <name> <direction> -> 0 if this asymmetry is a recorded
# decision. Rows carry an issue number; see the file header.
# NOTE: no pipeline into `grep -q`. Under `set -o pipefail` an early
# match makes grep exit first, the upstream loop dies with SIGPIPE (141),
# and pipefail propagates that as failure — so a recorded exemption near
# the top of a long file would be silently ignored.
known_exemption() {
    [ -f "$KNOWN" ] || return 1
    local n d
    while IFS='|' read -r n d _; do
        case "$n" in ''|'#'*) continue ;; esac
        [ "$(echo "$n" | xargs)" = "$1" ] || continue
        [ "$(echo "$d" | xargs)" = "$2" ] || continue
        return 0
    done <"$KNOWN"
    return 1
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

    # 2. Constraint-text parity. Ordered, and covering every solver-state
    # call, not just `add`: Z3's returned model depends on assertion
    # order, an `_s.add` moving across a `push` boundary changes which
    # assertions survive the matching `pop`, and `_s.check()` is the call
    # that actually consumes the assertions — soft-constraint lowering
    # interleaves it between adds. Sorting, or matching `add` alone,
    # would call all of those identical.
    if ! ls "${dirs[0]}"/*.cpp >/dev/null 2>&1 || ! ls "${dirs[1]}"/*.cpp >/dev/null 2>&1; then
        echo "  FAIL  $name  (emitted no .cpp despite exit 0)"
        echo "__STATUS__ FAIL $name"
        return 0
    fi
    local a b
    a="$(cat "${dirs[0]}"/*.cpp | grep -oE '_s\.(add|push|pop|check|set)\(.*')"
    b="$(cat "${dirs[1]}"/*.cpp | grep -oE '_s\.(add|push|pop|check|set)\(.*')"
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
    # `#` comments, as in every sibling list file. Without this a comment
    # row becomes a fixture literally named "#", which this gate scores as
    # a skip and run_fixtures.sh scores as a failure.
    case "$(echo "$row" | xargs)" in '#'*) continue ;; esac
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
# A run in which NOTHING was actually compared is not a pass. Dispatching
# rows is not enough: if every row lands in SKIP — because the binary is
# broken, the paths are wrong, or TB-IR has started rejecting the corpus
# wholesale with a declared-gap diagnostic — the gate would otherwise
# report success while checking nothing. That is the exact silent-green
# failure this gate exists to remove, so it must not be one itself.
compared=$((pass + passc))
if [ "$compared" -eq 0 ]; then
    echo "error: no fixture was compared — $skip skipped, $known known." >&2
    echo "       A gate that checks nothing must not report success." >&2
fi
[ "$fail" -eq 0 ] && [ "$nostatus" -eq 0 ] && [ "$n" -gt 0 ] && [ "$compared" -gt 0 ]
