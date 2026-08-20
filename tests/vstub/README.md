# Minimal Verilator stub headers

Enough of the Verilator surface for `g++ -fsyntax-only` to typecheck an
emitted HARC testbench. NOT a simulator: every method is a no-op and
every port is a plain member.

They exist so a test can answer one question — *does the C++ a backend
emitted actually typecheck?* — without a Verilator install. The suite
had 85 prose mentions of "g++" and no way to check any of them; see
`tests/differential.rs`.

Ports here must match the DUTs in `tests/dut/`. If a differential row
fails with "no member named X", the stub is behind the DUT, not the
compiler under test.
