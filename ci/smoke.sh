#!/usr/bin/env bash
# Check the complete installed toolchain, including Verilator FST headers/libs.
set -euo pipefail
verilator --version | grep 'Verilator 5.034'
rustc --version | grep 'rustc 1.94.0'
cargo --version
clang++ --version
ccache --version
pkg-config --modversion z3
xz --version
zstd --version
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
cat > top.sv <<'SV'
module top;
  initial begin
    #1;
    $display("CI smoke passed");
    $finish;
  end
endmodule
SV
verilator --binary --timing --trace-fst --top-module top top.sv \
    -CFLAGS '-std=c++20' -MAKEFLAGS 'CXX=clang++'
./obj_dir/Vtop | grep 'CI smoke passed'
cat > z3.cpp <<'CPP'
#include <z3.h>
int main() {
    Z3_config config = Z3_mk_config();
    Z3_del_config(config);
}
CPP
clang++ z3.cpp -lz3 -o z3-smoke
./z3-smoke
