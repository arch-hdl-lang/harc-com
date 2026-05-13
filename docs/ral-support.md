# Design doc: Register Abstraction Layer (RAL) support

**Status:** Proposed (RFC, not yet implemented).
**Date logged:** 2026-05-13.
**Scope:** Add first-class register-block, address-map, and memory primitives
to HARC v1 so a single declarative source compiles to a high-performance
mirror + predict + frontdoor/backdoor + constraint layer — replacing the
runtime-metaprogramming model of UVM RAL.

## 1. Motivation

UVM RAL is the de facto standard for register access in SV-based DV, and it's
slow. The slowness is structural, not a library bug:

- Every register is a `uvm_reg` class instance, every field a `uvm_reg_field`
  — built at runtime via the UVM factory + `uvm_object_registry` machinery.
- Every access is dispatched through virtual functions on the class graph
  (`uvm_reg::write` → `uvm_reg_map::do_write` → `uvm_reg_adapter::reg2bus` →
  user adapter), with per-call `uvm_sequence_item` allocation.
- Mirror / desired / predicted state is held in SV class objects with
  per-field bookkeeping; `uvm_reg_predictor` walks observer chains on every
  bus transaction.
- The `build_phase` for a real SoC reg model takes seconds-to-minutes and
  allocates dozens of MB of class graph before the first stimulus.
- Coverage hooks, callbacks, and adapter passes fire per access, all paying
  the SV class-dispatch tax.

The cost is paid every cycle, every simulation, by every test. For a
verification language whose stated goal is "UVM affordances at Verilator
speed" (see [README](../README.md)), a runtime-metaprogramming RAL is the
wrong layer.

HARC already lowers transactors, sequences, and reference models to
compiled C++ coroutines with no virtual dispatch (see
`tests/fixtures/axilite_regs_full_test.harc` and
`tests/fixtures/axilite_constraint_test.harc` for the pattern). The same
treatment applied to register state turns a runtime class graph into a
compile-time POD struct + constant address table.

## 2. Goals and non-goals

**Goals:**

1. Declarative `regblock` syntax with typed fields, access policy, reset
   values, and bit positions — matching the expressive scope of
   SystemRDL 2.0's `reg` + `field`.
2. `addrmap` containers for chip-level composition with per-instance base
   addresses, multi-map support, and aliasing.
3. `mem` blocks for memory-mapped storage regions (RAMs, scratchpads),
   with three explicit storage modes (sparse / dense / none).
4. Compile-time lowering to:
   - A POD mirror struct (nested for hierarchical addrmaps).
   - A `constexpr` address + policy table.
   - Frontdoor accessors that lower to coroutine bus calls.
   - Backdoor accessors that lower to direct Verilator handle ops.
   - Bit-bash / walk-all sequences unrolled at compile time.
5. Surface compatibility with the future RDL → HARC generator
   (`rdl2harc`, see §8), so one `.rdl` source produces both the DUT-side
   register block (via existing `rdl2arch`) and the DV-side mirror layer
   without divergence risk.

**Non-goals:**

1. Runtime field-policy overrides / RAL-mod-style monkey-patching. Field
   policy is fixed at compile time; overrides happen via `extend regblock`
   syntax.
2. Dynamic register-layout swapping within one simulation run. If the DUT
   reconfigures its layout, the user re-binds a different `regblock` to
   the bus rather than mutating one model.
3. UVM-RAL-bug-compatible semantics. We will not emulate UVM corner cases
   that exist only because of UVM's class architecture.
4. Multi-map cross-aliasing of `mem` blocks in v0.1 (see §11.2).

## 3. Surface — `regblock`

```harc
regblock AxiDmaRegs bound to BusAxiLite width 32 endian little
    register DMACR @ 0x00
        field RS       : bit  @ 0   reset 0  access rw
        field Reset    : bit  @ 2   reset 0  access w1c
        field IRQ_IOC  : bit  @ 12  reset 0  access rw
    end register

    register DMASR @ 0x04
        field Halted   : bit  @ 0   reset 1  access ro
        field Idle     : bit  @ 1   reset 1  access ro
        field IRQ_IOC  : bit  @ 12  reset 0  access w1c
    end register

    register MM2S_SA  @ 0x18  access rw
        field value : uint<32> @ 0 reset 0
    end register
end regblock
```

### Field access policy keywords

Selected to map 1:1 onto SystemRDL's `sw=` and `onwrite=` / `onread=`
actions, so `rdl2harc` is a syntactic rewrite, not a semantic translation:

| HARC keyword | RDL equivalent | Effect |
|---|---|---|
| `rw`     | `sw=rw`              | Read-write, no side-effect on access |
| `ro`     | `sw=r`               | Read-only; writes ignored |
| `wo`     | `sw=w`               | Write-only; reads return 0 |
| `w1c`    | `sw=rw onwrite=woclr` | Write-1-to-clear |
| `w1s`    | `sw=rw onwrite=woset` | Write-1-to-set |
| `w0c`    | `sw=rw onwrite=wzc`  | Write-0-to-clear |
| `w0s`    | `sw=rw onwrite=wzs`  | Write-0-to-set |
| `wclr`   | `sw=rw onwrite=wclr` | Any write clears |
| `wset`   | `sw=rw onwrite=wset` | Any write sets |
| `rc`     | `sw=r  onread=rclr`  | Read clears |
| `rs`     | `sw=r  onread=rset`  | Read sets |

This list intentionally tracks `rdl2arch` v0.1's supported subset; expansion
(`intr`, `stickybit`, edge-detect) follows the same precedent.

### Multi-bit fields and reset values

```harc
register CTRL @ 0x08
    field MODE   : uint<3>  @ 4   reset 0b011  access rw
    field CHAN   : uint<4>  @ 8   reset 0      access rw
    field SCALE  : uint<8>  @ 16  reset 0xFF   access rw
end register
```

Reset values are part of the decl and form the initial mirror state. Width
is checked against the declared field type at compile time.

## 4. Surface — `addrmap`

```harc
addrmap SocRegs bound to BusAxiLite
    instance dma0  : AxiDmaRegs @ 0x4000_0000  size 0x1000
    instance dma1  : AxiDmaRegs @ 0x4000_1000  size 0x1000  alias of dma0
    instance uart0 : UartRegs   @ 0x5000_0000  size 0x0100
    instance gpio  : GpioRegs   @ 0x6000_0000  size 0x0100
end addrmap

addrmap SocRegsDebug bound to BusAxiLite
    instance dma0  : AxiDmaRegs @ 0xF000_0000
    instance uart0 : UartRegs   @ 0xF000_1000
end addrmap
```

### Composition rules

- `addrmap` may contain `instance` of `regblock`, `mem`, or another
  `addrmap`. Nesting is unbounded; address offsets compose by addition.
- `alias of <other_instance>` declares two address windows backed by one
  mirror cell. Predict updates the shared cell regardless of which window
  the bus access traveled through.
- `size` is optional; when present, the compiler checks that no two
  non-aliased windows overlap, with the offending pair pointed at in the
  error message.
- Multiple `addrmap`s may instantiate the same `regblock` at different
  bases (different CPU views, debug bus, security domain). The mirror tree
  is shared if the same `let` instance is rebound; cloned if a fresh
  `let` is created.

## 5. Surface — `mem`

```harc
mem ScratchPad
    depth   1024
    width   32
    access  rw
    mirror  sparse          // sparse (default) | dense | none
end mem

mem DdrWindow
    depth   0x4000_0000     // 1 GB
    width   32
    access  rw
    mirror  none
    reference DdrRefModel   // user-supplied transactor for predict
end mem
```

### Storage modes

| Mode | Storage emitted | Predict | When to use |
|---|---|---|---|
| `sparse` *(default)* | hashmap (`std::unordered_map<uint32_t, T>`); zero alloc until first touch | insert/check on access | Most mems — tests touch a handful of entries, rest stays free |
| `dense` | heap-boxed `std::array<T, depth>` (see §7.3) | inline array index | Small/medium mems with broad coverage (FIFOs, small scratchpads ≤ ~64 KB) |
| `dense inline` | embedded `std::array<T, depth>` directly in the mirror struct | inline array index | Only valid if `depth × width ≤ 128 bytes`. Opt-in escape hatch for hot inner loops on tiny mems |
| `none` | no internal storage | delegates to `reference` transactor if bound; otherwise no predict | Multi-MB / multi-GB regions (DDR, HBM); requires a reference model when predict matters |

### Memory access syntax

```harc
regs.scratch[42] = 0xDEAD             // frontdoor write (idx → base + 42*4)
let x = regs.scratch[42]              // frontdoor read + predict update
backdoor regs.scratch[42] = 0xBEEF    // direct DUT handle store
backdoor regs.scratch.preload("img.hex")    // bulk image preload helper
```

The `preload` helper is a codegen builtin: for `dense`, it lowers to a
single `memcpy` after parsing the file; for `sparse`, it walks the file
inserting hashmap entries. For `none` with a reference model, it calls
the reference's bulk-load hook.

## 6. Surface — `extend`

HARC already has `extend test T` (see [spec.md §8](../spec.md)); the same
pattern applies to regblocks and addrmaps:

```harc
// Original decl somewhere — possibly rdl2harc-generated.
regblock AxiDmaRegs ...
    // ... base fields ...
end regblock

// Customization without modifying the generated source.
extend regblock AxiDmaRegs
    // Override a field's policy for one test scope (e.g. relax RO to RW
    // for a fault-injection campaign).
    override field DMASR.Halted access rw
end extend
```

`extend` is **compile-time only**; there is no runtime override factory.
This is intentional: it preserves the "no surprise at sim time" property
of the HARC design, at the cost of recompilation when overrides change.

## 7. Lowering

### 7.1 Mirror struct

```cpp
// For: regblock AxiDmaRegs { register DMACR; register DMASR; register MM2S_SA; }
struct AxiDmaRegs_Mirror {
    uint32_t DMACR;
    uint32_t DMASR;
    uint32_t MM2S_SA;
};

// For: addrmap SocRegs { instance dma0: AxiDmaRegs; instance scratch: ScratchPad; ... }
struct SocRegs_Mirror {
    AxiDmaRegs_Mirror dma0;
    AxiDmaRegs_Mirror dma1;     // independent storage; if `alias of dma0`, dma1 is a reference
    ScratchPad_Mirror scratch;
    UartRegs_Mirror   uart0;
    GpioRegs_Mirror   gpio;
    DdrWindow_Mirror  ddr;      // 0 bytes when mirror=none
};
```

Composition is C++ struct nesting; the compiler folds offsets at
compile time, so `regs.dma0.MM2S_SA` and a flat `regs_flat[OFFSET]`
produce the same load/store after optimization.

### 7.2 Address + policy tables

```cpp
constexpr std::array<RegEntry, N_regs> SocRegs_AddrTable = {{
    { .name="dma0.DMACR",   .offset=0x4000'0000 + 0x00, .policy_mask=0x1005 /* W1C bit 2 */ },
    { .name="dma0.DMASR",   .offset=0x4000'0000 + 0x04, .policy_mask=0x1003 /* RO 0..1, W1C 12 */ },
    { .name="dma0.MM2S_SA", .offset=0x4000'0000 + 0x18, .policy_mask=0xFFFFFFFF /* RW */ },
    // ...
}};
```

This table drives `bitbash(regs)`, address-overlap checks, and runtime
diagnostic messages ("write to RO field ignored at addr 0x...").

### 7.3 Dense mem heap-boxing

Dense storage is heap-boxed by default, not inlined into the mirror struct.
This keeps the top-level `SocRegs_Mirror` small (one pointer per dense mem)
regardless of how many large mems the chip contains. See the
[separate analysis](#71-mirror-struct) for the cost rationale; numerically:

| Layout | `SocRegs_Mirror` size for chip with 10× 64 KB dense mems |
|---|---|
| Inline | ~640 KB |
| Heap-boxed (default) | < 200 bytes |

```cpp
struct ScratchPad_Mirror {
    std::unique_ptr<std::array<uint32_t, 1024>> data;
    // policy state, addr base, depth
};
```

Allocation is **eager by default** (allocated at test-struct construction)
to keep sim timing reproducible. `mirror dense lazy` opts into first-touch
allocation for big regression suites where most tests touch a subset of
mems.

The `inline` escape hatch (`mirror dense inline`) is permitted only when
`depth × width ≤ 128 bytes`; the compiler emits a warning above that.

### 7.4 Frontdoor access

```harc
regs.dma0.DMACR.RS = 1
```

lowers to (schematically):

```cpp
mirror.dma0.DMACR = (mirror.dma0.DMACR & ~0x1u) | 0x1u;
co_await bus_axil.write(DMA0_BASE + 0x00, mirror.dma0.DMACR);
```

For W1C, the write-side mask is applied to the mirror automatically; for
RO, the write is dropped with a warning at sim time (or hard-failed under
`--strict`); for `rclr`/`rset`, the read-side mirror update reflects the
side effect.

### 7.5 Backdoor access

```harc
backdoor regs.uart0.RBR = 0x42
```

lowers to a direct Verilator handle store:

```cpp
dut->root__uart0__rbr_q = 0x42;
mirror.uart0.RBR = 0x42;     // keep mirror coherent
```

The hierarchical path comes from one of:

1. An explicit `hdl_path` annotation in the HARC decl.
2. A `hdl_path_slice` property carried over from the source RDL (when
   `rdl2harc`-generated).
3. A user-supplied lookup table passed to the test runner.

If no path is available, the compiler **refuses to emit** the backdoor
accessor and reports the missing path at the decl site. We will not
generate broken backdoor code that fails silently at runtime.

### 7.6 Constraint randomization

```harc
randomize(regs.dma0.MM2S_SA) with {
    regs.dma0.MM2S_SA % 4 == 0
    regs.dma0.MM2S_SA < 0x10000
}
```

reuses the existing `randomize` lowering (see
`tests/fixtures/axilite_constraint_test_sim.harc` for the emitted shape):
a Z3 expression built from the field's declared width and the inline
constraints, solved per iteration, with the result written via the
normal frontdoor path. The variable namespace is path-qualified
(`dma0_MM2S_SA`, `uart0_CTRL`, etc.) so cross-IP constraints stay
unambiguous.

### 7.7 Bit-bash / walk-all sequences

`bitbash(regs)` is a codegen builtin that expands at compile time to a
flat sequence of writes and reads over `SocRegs_AddrTable`, with policy
masks pre-applied (so RO fields are read-only-tested, W1C fields get a
write-1-then-readback pattern, etc.). `bitbash(regs.dma0)` filters the
expansion to one subtree.

No runtime reflection; the unrolled code is visible in the emitted C++.

## 8. RDL alignment — `rdl2harc`

A future `rdl2harc` tool (see
[`rdl_codegen_roadmap`](../../.claude/projects/-Users-shuqingzhao-github-harc-com/memory/rdl_codegen_roadmap.md))
will lower SystemRDL 2.0 to `.harc` regblock / addrmap / mem source,
paralleling the existing `rdl2arch` DUT-side generator.

The design choices in this RFC are deliberately RDL-shaped to keep that
translation a syntactic rewrite:

| RDL construct | HARC mapping |
|---|---|
| `addrmap` | `addrmap` |
| `regfile` | nested `addrmap` (or name-prefix; pick to match `rdl2arch`) |
| `reg` | `register` inside a `regblock` |
| `field` | `field` |
| `sw=rw / r / w` | `access rw / ro / wo` |
| `onwrite = woclr / woset / wclr / wset / wzc / wzs / wzt` | `access w1c / w1s / wclr / wset / w0c / w0s / wclr` |
| `onread = rclr / rset` | `access rc / rs` |
| `mementries`, `memwidth` | `depth`, `width` |
| `external mem` | `mem ... mirror none` (reference required) |
| `hdl_path` / `hdl_path_slice` | annotation on register/mem |
| reg/mem array | `instance name[N]` |
| `alias` | `alias of` |

`rdl2harc` v0.1 should ship with the same feature subset as `rdl2arch`
v0.1 (no `intr`, no counter fields, no edge-detect) so the two tools are
in lockstep.

## 9. Performance properties

Compared to UVM RAL on the same DUT and same test:

| Cost | UVM RAL | HARC RAL |
|---|---|---|
| Per-register storage | `uvm_reg` class + `uvm_reg_field` objects (~hundreds of bytes per reg) | 4 bytes (one mirror cell) + entry in `constexpr` address table |
| Model build phase | seconds to minutes for SoC-scale | one `memset` of mirror BSS — microseconds |
| Per-access dispatch | virtual function chain through `uvm_reg → uvm_reg_map → uvm_reg_adapter` | direct function call into the coroutine bus accessor |
| Per-access allocation | `uvm_sequence_item` constructed per access | none |
| Predict | observer chain via `uvm_reg_predictor` | inline mirror update at the access site |
| Backdoor | `uvm_hdl_deposit` DPI call with string path | direct Verilator handle store |
| Bit-bash sequence | runtime reflection over reg model | compile-time-unrolled access list |
| Coverage hooks | per-access SV callback dispatch | inline bitmask update on the mirror cell |

The asymptotic claim: **HARC RAL has the same memory footprint as the
fields it models, no runtime build phase, and no per-access metaprogramming
overhead.**

The honest caveat: anything that wants *true* runtime reflection (e.g. a
generic UVC that walks "every reg in any model" via a string interface)
gives that up. We argue this is the right trade for DV that targets a
known DUT.

## 10. Scope of the v0.1 implementation

Phase 1 (this RFC's MVP):

1. `regblock` parse + AST + semantic checks (field width, bit position
   overlap, reset width).
2. `addrmap` with single-level composition, base addresses, overlap
   check, aliasing.
3. Mirror struct codegen (POD, struct of structs, with `clone()` for
   snapshots).
4. Address + policy table codegen as `constexpr` arrays.
5. Frontdoor `read` / `write` lowering through the existing
   `bound to <Bus>` machinery (no changes to the bus protocol layer).
6. Constraint integration: `randomize(regs.path.field)` plumbed into the
   existing Z3 lowering.
7. Access policy enforcement: `rw / ro / wo / w1c / w1s / wclr / wset /
   rc / rs`. (The full RDL-aligned set in §3 is the eventual target;
   ship the common ones first.)
8. `bitbash(regs)` builtin (compile-time unroll, no runtime reflection).

Phase 2 (post-MVP):

1. `mem` decl in `sparse` and `dense` modes (no `none` / reference yet).
2. `backdoor` accessors for registers and dense mems.
3. Multi-level `addrmap` nesting.
4. Multiple-map support (`addrmap SocRegsDebug` parallel to `SocRegs`).
5. `extend regblock` override syntax.

Phase 3:

1. `mem mirror none` with reference-model hook.
2. `mem` preload from `.hex` / `.bin`.
3. Cross-IP `coverpoint` over mirror state.
4. Edge-detect / sticky / interrupt fields (matching `rdl2arch`'s
   eventual expansion).

`rdl2harc` itself is a separate repo, gated on Phase 1 of this RFC.

## 11. Open questions

### 11.1 Eager vs lazy mirror init for dense mems

Phase-2 doc proposes eager-by-default with `lazy` opt-in. Open whether
lazy should be the default for very-large dense mems (≥ 1 MB) where the
allocation jitter is itself noticeable. **Tentative answer:** keep eager,
warn at codegen if `depth × width ≥ 1 MB` and `lazy` is not specified.

### 11.2 Mem aliasing across maps

RDL allows the same `mem` to be visible at different addresses in
different `addrmap`s. v0.1 will only support `alias of` *within* a single
addrmap; cross-map mem aliasing requires deciding whose mirror is
canonical when both maps are bound simultaneously. Defer to Phase 3.

### 11.3 Multi-bus chips

A real SoC has AXI4-Lite on some IPs, APB on others, AHB elsewhere. The
chip-level `addrmap` currently has one `bound to <Bus>` for the entire
container. **Tentative answer:** allow per-instance `bound to` overrides:

```harc
addrmap SocRegs bound to BusAxiLite
    instance dma0  : AxiDmaRegs @ 0x4000_0000
    instance gpio  : GpioRegs   @ 0x6000_0000 bound to BusApb
end addrmap
```

This needs the bus-bridging story (one bus, gated to the right child
based on address) to be a v0.1 capability or a clear deferral.

### 11.4 Coverage syntax over mirror state

The mirror lets `coverpoint regs.dma0.DMACR.RS && regs.uart0.CTRL.EN`
work mechanically — but the existing coverage syntax in spec.md §6
doesn't currently bind to mirror paths. Needs minor spec extension;
parking this for a separate RFC once Phase 1 lands.

### 11.5 RDL `external` register (vs `external mem`)

RDL allows `external` on a single `reg` too — meaning the storage isn't
modeled in RDL. The HARC equivalent is `register ... external` which
takes a per-register `reference` hook. Out of scope for v0.1; add when
the mem-reference pattern is proven.

## 12. Trade-offs accepted

- **No runtime introspection.** A "list every register in any model" UVC
  must be regenerated from declarations, not discovered dynamically.
- **`extend regblock` requires recompilation.** Acceptable: HARC is a
  compiled language; recompiling is fast and offsets are still folded.
- **Field-policy vocabulary is fixed by RDL alignment.** Custom policies
  (vendor-specific quirks) require either expressing them as a
  combination of standard policies or extending the codegen.
- **Backdoor paths must be known at compile time.** No string-based
  hierarchical-path resolution at sim start. Tradeoff for the
  "no silent failure" property.

## 13. References

- HARC v1 spec: [`../spec.md`](../spec.md)
- HARC README: [`../README.md`](../README.md)
- Existing axilite test patterns:
  [`tests/fixtures/axilite_regs_full_test.harc`](../tests/fixtures/axilite_regs_full_test.harc),
  [`tests/fixtures/axilite_constraint_test.harc`](../tests/fixtures/axilite_constraint_test.harc)
- `rdl2arch` (sibling DUT-side generator): https://github.com/arch-hdl-lang/rdl2arch
- `rdl2arch-riscv` (RISC-V CSR specialization): https://github.com/arch-hdl-lang/rdl2arch-riscv
- SystemRDL 2.0 spec (Accellera): the source vocabulary `rdl2harc` will lower from
- `systemrdl-compiler` (MIT-licensed RDL frontend): the parser both
  `rdl2arch` and the future `rdl2harc` build on
