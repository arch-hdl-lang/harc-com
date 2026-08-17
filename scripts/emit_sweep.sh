#!/usr/bin/env bash
# Blast-radius sweep: emit every fixture under both backends at two git
# revisions and diff the generated C++.
#
# This is a REVIEW tool, not a CI gate. It answers one question that no
# test answers: "how much of the corpus does my change actually move?"
# During harc#559 it was run by hand at every step, and its most useful
# result was almost always ZERO — being able to say "this touches two
# lines of emitted output across the fixture table" is what bounded a change
# whose review kept turning up surprises.
#
# Note the corollary, which is the substance of harc#551: a sweep that
# reports no differences also proves the corpus does not exercise your
# change. Read a clean result as "nothing existing regressed", never as
# "the new behaviour is right".
#
# Usage:
#     scripts/emit_sweep.sh [BASE_REV] [HEAD_REV]
#
# BASE_REV defaults to origin/main, HEAD_REV to the working tree. A named
# revision is checked out into a temporary worktree and built into its own
# CARGO_TARGET_DIR. The default HEAD (working tree) case builds in place,
# in YOUR target dir — that is the one thing here that touches your normal
# build cache. Pass an explicit HEAD_REV to avoid it entirely.
set -uo pipefail

BASE_REV="${1:-origin/main}"
HEAD_REV="${2:-}"
DUT_DIR="tests/dut"
FIX_DIR="tests/fixtures"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TMP="$(mktemp -d)"
cleanup() {
    for wt in "$TMP"/wt_*; do
        [ -d "$wt" ] && git -C "$ROOT" worktree remove --force "$wt" >/dev/null 2>&1
    done
    rm -rf "$TMP"
    git -C "$ROOT" worktree prune >/dev/null 2>&1
}
trap cleanup EXIT

# NOTE: called via $( ), so this runs in a SUBSHELL — anything it assigns
# to a parent variable is lost. The worktree path is therefore derived
# from <label> by both this function and the trap, rather than passed
# back through a variable, which is how an earlier version left the
# trap's cleanup clause permanently dead.
build_at() { # build_at <rev|WORKTREE> <label> -> echoes binary path
    local rev="$1" label="$2"
    if [ "$rev" = "WORKTREE" ]; then
        (cd "$ROOT" && cargo build -q --bin harc) || return 1
        echo "$ROOT/target/debug/harc"; return 0
    fi
    local wt="$TMP/wt_$label"
    git -C "$ROOT" worktree add -q --detach "$wt" "$rev" || return 1
    # A dedicated target dir, so the sweep never writes into the target
    # dir you are working in. It shares nothing, so this IS a cold build
    # of every dependency — the price of not touching your cache.
    (cd "$wt" && CARGO_TARGET_DIR="$TMP/target_$label" cargo build -q --bin harc) || return 1
    echo "$TMP/target_$label/debug/harc"
}

TABLE="$ROOT/tests/fixtures.tbl"
[ -s "$TABLE" ] || { echo "error: $TABLE missing or empty" >&2; exit 1; }

echo "Building $BASE_REV ..." >&2
BASE_BIN="$(build_at "$BASE_REV" base)" || { echo "failed to build $BASE_REV" >&2; exit 1; }
echo "Building ${HEAD_REV:-working tree} ..." >&2
HEAD_BIN="$(build_at "${HEAD_REV:-WORKTREE}" head)" || { echo "failed to build head" >&2; exit 1; }

FIXTURES="$(cat "$TABLE")"
exitdiff=0; textdiff=0; total=0
while IFS='|' read -r name top svs extra ref tstruct; do
    case "$(echo "$name" | xargs)" in '#'*) continue ;; esac
    name="$(echo "$name" | xargs)"; top="$(echo "$top" | xargs)"
    svs="$(echo "$svs" | xargs)"; extra="$(echo "$extra" | xargs)"
    tstruct="$(echo "$tstruct" | xargs)"
    [ -z "$name" ] && continue
    args=("$FIX_DIR/$name.harc")
    for f in $extra; do args+=("$FIX_DIR/$f"); done
    svargs=(); for f in $svs; do svargs+=(--sv "$DUT_DIR/$f"); done
    targs=(); [ -n "$tstruct" ] && targs=(--test "$tstruct")
    for cg in v1 tbir; do
        total=$((total + 1))
        for side in base head; do
            bin="$BASE_BIN"; [ "$side" = head ] && bin="$HEAD_BIN"
            (cd "$ROOT" && "$bin" sim "${args[@]}" "${svargs[@]}" --top "$top" \
                --emit-only --codegen "$cg" --outdir "$TMP/$side/$cg/$name" \
                "${targs[@]}") >/dev/null 2>&1
            eval "rc_$side=$?"
        done
        if [ "$rc_base" != "$rc_head" ]; then
            echo "EXIT  $cg  $name  ($rc_base -> $rc_head)"
            exitdiff=$((exitdiff + 1)); continue
        fi
        b="$TMP/base/$cg/$name/$name.cpp"; h="$TMP/head/$cg/$name/$name.cpp"
        if [ -f "$b" ] && [ -f "$h" ] && ! diff -q "$b" "$h" >/dev/null; then
            echo "TEXT  $cg  $name  ($(diff "$b" "$h" | grep -c '^[<>]') lines)"
            textdiff=$((textdiff + 1))
        fi
    done
done <<<"$FIXTURES"

echo
echo "Swept $total fixture×backend pairs: $exitdiff acceptance changes, $textdiff text changes"
echo "(A clean sweep bounds regressions. It does NOT show the new behaviour is correct —"
echo " if your change has no corpus coverage, this is exactly what you would see. harc#551.)"
