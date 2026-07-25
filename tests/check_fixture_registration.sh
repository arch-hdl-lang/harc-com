#!/bin/bash
# Guard against sim fixtures that silently never run.
#
# `tests/run_fixtures.sh` drives its work from an explicit registry table, not
# from a glob of `tests/fixtures/*.harc`. So dropping a new fixture into that
# directory and forgetting the registry row is a SILENT failure: CI stays green
# because the fixture is simply never executed. That happened to
# `soft_constraint_randomize_test` (#514) — fixture file and README row both
# present, registry row missing, zero sim coverage, green CI.
#
# The rule enforced here:
#
#   A fixture that declares a DUT (`let dut : X`) must be reachable from some
#   runner — the run_fixtures.sh registry, another tests/*.sh runner, a Rust
#   test, an equivalence list, or another fixture that includes it.
#
# DUT-less fixtures are deliberately out of scope: `run_fixtures.sh` runs
# "every HARC fixture that has a vendored DUT", and DUT-less fixtures still get
# parse/typecheck coverage from the `harc check every fixture` and `harc fmt
# round-trip` CI steps, which DO glob the whole directory.
#
# Intentional exceptions go in tests/fixtures_unwired_allowlist.txt, one
# `<fixture_name> # <reason>` per line — a reason is mandatory so an entry is a
# deliberate decision rather than a way to silence the check.
#
# Run from harc-com repo root:
#     ./tests/check_fixture_registration.sh

set -uo pipefail

cd "$(dirname "$0")/.."

FIX_DIR="tests/fixtures"
ALLOWLIST="tests/fixtures_unwired_allowlist.txt"

# Names explicitly allowed to be unwired, with their stated reason.
declare -a ALLOWED_NAMES=()
if [ -f "$ALLOWLIST" ]; then
    while IFS= read -r line; do
        # Strip comment-only and blank lines.
        case "$line" in
            ''|\#*) continue ;;
        esac
        name="${line%%#*}"
        name="$(echo "$name" | tr -d '[:space:]')"
        reason="${line#*#}"
        reason="$(echo "$reason" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
        [ -z "$name" ] && continue
        if [ "$reason" = "$line" ] || [ -z "$reason" ]; then
            echo "::error file=$ALLOWLIST::allowlist entry '$name' has no reason; use '<name> # <reason>'"
            exit 1
        fi
        ALLOWED_NAMES+=("$name")
    done < "$ALLOWLIST"
fi

is_allowed() {
    local needle="$1"
    local n
    for n in "${ALLOWED_NAMES[@]:-}"; do
        [ "$n" = "$needle" ] && return 0
    done
    return 1
}

# Is the fixture reachable from anything that actually executes it?
is_wired() {
    local base="$1"
    # A registry row or any other field (extra harc files, --test structs) in
    # run_fixtures.sh. Pipe/space delimited, so match on field boundaries.
    if grep -qE "(^|[ |])${base}(\.harc)?([ |]|$)" tests/run_fixtures.sh 2>/dev/null; then
        return 0
    fi
    # Any other shell runner, Rust test, or fixture list. The allowlist is
    # excluded: it names unwired fixtures, so counting it would make every
    # allowlisted entry look wired (and then stale).
    local candidate
    for candidate in tests/*.sh tests/*.rs tests/*.txt; do
        [ -e "$candidate" ] || continue
        [ "$candidate" = "$ALLOWLIST" ] && continue
        [ "$candidate" = "tests/check_fixture_registration.sh" ] && continue
        if grep -qF "$base" "$candidate" 2>/dev/null; then
            return 0
        fi
    done
    # Included by another fixture (helper/shared definitions).
    if grep -rqF "${base}.harc" "$FIX_DIR" 2>/dev/null; then
        return 0
    fi
    return 1
}

fail=0
unwired=()
stale_allow=()

for f in "$FIX_DIR"/*.harc; do
    [ -e "$f" ] || continue
    base="$(basename "$f" .harc)"

    # Only fixtures that drive a DUT are in scope.
    grep -qE "let[[:space:]]+dut[[:space:]]*:" "$f" || continue

    if is_wired "$base"; then
        # Wired *and* allowlisted => the allowlist entry is stale.
        if is_allowed "$base"; then
            stale_allow+=("$base")
        fi
        continue
    fi

    if is_allowed "$base"; then
        continue
    fi

    unwired+=("$base")
    fail=1
done

for base in "${unwired[@]:-}"; do
    [ -z "$base" ] && continue
    dut="$(grep -oE "let[[:space:]]+dut[[:space:]]*:[[:space:]]*[A-Za-z_][A-Za-z0-9_]*" \
        "$FIX_DIR/$base.harc" | head -1 | awk '{print $NF}')"
    echo "::error file=$FIX_DIR/$base.harc::fixture declares a DUT (${dut:-?}) but no runner references it — it will never execute"
    echo "  fix: add a row to tests/run_fixtures.sh, e.g."
    echo "      $base | ${dut:-TopModule} | ${dut:-top_module}.sv |"
    echo "  or, if it is intentionally not sim-run, add it to $ALLOWLIST with a reason."
done

for base in "${stale_allow[@]:-}"; do
    [ -z "$base" ] && continue
    echo "::error file=$ALLOWLIST::'$base' is now wired to a runner — remove its stale allowlist entry"
    fail=1
done

if [ "$fail" -eq 0 ]; then
    echo "fixture registration: OK (all DUT-driving fixtures are reachable from a runner)"
fi

exit $fail
