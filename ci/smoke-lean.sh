#!/usr/bin/env bash
# Check the Lean variant: everything in the base image, plus a Lean toolchain
# that `lake` resolves from a `lean-toolchain` file without downloading.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bash "$here/smoke.sh"
lean --version | grep 'version 4.30.0'
lake --version
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
echo 'leanprover/lean4:v4.30.0' > lean-toolchain
cat > lakefile.toml <<'TOML'
name = "smoke"
defaultTargets = ["Smoke"]
[[lean_lib]]
name = "Smoke"
TOML
cat > Smoke.lean <<'LEAN'
theorem smoke : (2 : Nat) ^ 8 % 7 = 4 := by decide
LEAN
lake build 2>&1 | tee build.log
grep -q 'Build completed successfully' build.log
grep -q 'Built Smoke' build.log
# Negative control: a false theorem must be rejected.
echo 'theorem bad : (2 : Nat) ^ 8 % 7 = 5 := by decide' > Smoke.lean
if lake build >bad.log 2>&1; then
  cat bad.log
  echo "Lean accepted a false theorem" >&2
  exit 1
fi
echo "Lean smoke passed"
