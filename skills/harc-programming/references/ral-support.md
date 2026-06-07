# Design doc: Register Abstraction Layer (RAL) support

**Status:** Phase 1 largely shipped. Phase 2 + 3 still proposed.
**Date logged:** 2026-05-13. Updated 2026-05-14 after Phase 1a–1e.
**Scope:** Add first-class register-block, address-map, and memory primitives
to HARC v1 so a single declarative source compiles to a high-performance
mirror + predict + frontdoor/backdoor + constraint layer — replacing the
runtime-metaprogramming model of UVM RAL.

## Implementation status

| Item | Status | Lands in |
|---|---|---|
| `regblock` decl + bind helper + register-level R/W frontdoor | **Shipped** | PR #95 (Phase 1a) |
| Field-level decomposition (`field N : T @ POS`) | **Shipped** | PR #97 (Phase 1b) |
| Read-side mirror predict | **Shipped** | PR #98 (Phase 1c) |
| `rw` / `ro` / `wo` access policies | **Shipped** | PR #98 (Phase 1c) |
| `bitbash(regs)` compile-time-unrolled walk-all | **Shipped** | PR #99 (Phase 1d) |
| `addrmap` composition (flat) — multiple regblock instances at distinct bases | **Shipped** | PR #100 (Phase 1e) |
| Passive `record_write(addr,data)` / `record_read(addr)` mirror API | **Shipped** | Phase 1f |
| Per-register write callbacks (`on regs.REG ... end on`) | **Shipped** | Phase 1f |
| `w1c` / `w1s` / `wclr` / `wset` / `rc` / `rs` policies | **Proposed (deferred)** | testability problem — needs W1C-aware DUT |
| `alias of` instance aliasing | **Proposed** | Phase 2 |
| Nested `addrmap`s (addrmap inside addrmap) | **Proposed** | Phase 2 |
| Multi-map (two addrmaps over the same instances) | **Proposed** | Phase 2 |
| Per-instance bus binding override | **Proposed** | Phase 2 |
| `extend regblock` | **Proposed** | Phase 2 |
| `mem` blocks (sparse / dense / none) | **Proposed** | Phase 2 / 3 |
| Backdoor accessors (Verilator direct-handle reads/writes) | **Proposed** | Phase 2 |
| `mem mirror none` + reference model | **Proposed** | Phase 3 |
| RDL → HARC generator (`rdl2harc`) | **Proposed** | sibling repo, gated on Phase 1 |

The shipped Phase 1 surface delivers what the RFC framed as the MVP for
real DV: typed register/field decls, frontdoor reads/writes routed
through a helper transactor (`via Helper` clause), automatic mirror with
read-side predict, register-level access policies that drop bus traffic
appropriately, compile-time bit-bash, and chip-level composition.

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

## 3. Surface — `regblock`  *(shipped, Phase 1a–1c)*

What shipped: registers can be single-line or block-form (with
fields); access policies `rw`/`ro`/`wo` are recognized; the helper
that routes bus traffic is named with the `via <Transactor>` clause
(not the RFC's original `bound to <Bus>` — see §10 for the
rationale). The `endian` keyword is parsed but unused.

```harc
regblock AxiDmaRegs via AxilHelper width 32
    register DMACR @ 0x00 access rw
        field RS       : bit  @ 0   reset 0  access rw
        // `w1c`/`w1s`/etc. policies are not in the shipped slice yet
        // — see the status table. `rw` is the placeholder.
        field IRQ_IOC  : bit  @ 12  reset 0  access rw
    end register DMACR

    register DMASR @ 0x04 access ro
        field Halted   : bit  @ 0   reset 1  access ro
        field Idle     : bit  @ 1   reset 1  access ro
    end register DMASR

    register MM2S_SA  @ 0x18  access rw
end regblock AxiDmaRegs
```

The single-line `register N @ A access X` form (no fields, no `end
register` closer) coexists with the block form. Most fixtures use
single-line; field-bearing registers use the block form.

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

### 3.2 Passive record API + per-register write callbacks  *(shipped, Phase 1f)*

The Phase 1a frontdoor (`regs.REG = v` / `let x = regs.REG`) is *active*:
each access issues a bus transaction through the `via` helper. A large
class of DV code is instead *passive* — a checker observes bus traffic
(forwarded from a monitor) and shadows CSR state without driving the
bus. Before Phase 1f that meant a hand-written address-decode ladder:

```harc
if addr == CONTROL_REG
    csr_shadow.control = data
elsif addr == config_group_addr(0)
    csr_shadow.config_group0 = data
// ... one branch per register ...
end if
```

Phase 1f gives the checker the decode for free:

```harc
regs.record_write(addr, data)     // decode addr -> mirror cell, update it
let v = regs.record_read(addr)    // decode addr -> mirror cell, read it
```

Both are **passive and mirror-only** — they never touch the bus (the
monitor already saw the transaction). `record_write` masks the value to
the register width and updates the matching mirror cell; `record_read`
returns the mirror cell for that address (or `0` for an unmapped
address). The address decode is folded at codegen time into a generated
`<Regblock>_record_read` function and an inline `record_write` ladder, so
the checker never enumerates registers by hand and can't forget one when
a register is added to the block.

**Per-register write callbacks** let a checker recompute derived state
when a register is recorded:

```harc
// impl/test scope, alongside the `let` bindings (not inside `run`).
on regs.CONTROL
    // body runs after record_write updates the CONTROL mirror cell.
    // the observed value is bound to `data` (a uint64).
    if data != 0
        ...
    end if
end on
```

A callback fires from inside `record_write`'s decode for exactly the
matching register — there is no `if addr == ...` switch in the body. The
body captures the enclosing scope by reference (same `[&]` model as the
existing `on obj.method pre/post` hooks) so it can mutate test-scope
scoreboards/counters.

**Scope and limitations (intentional for Phase 1f):**

- **Register granularity only.** Callbacks attach to a register, not a
  field. Field-level callbacks would need a synthesized per-field
  dispatch; deferred. (This was a deliberate call — see the design
  discussion: a *per-method* hook that forces the user to re-decode the
  address inside the body would reintroduce the very ladder this feature
  removes, so the dispatch is folded per-register by the codegen.)
- **Regblock bindings only.** `record_write` / `record_read` resolve
  against a `let regs : <Regblock> = bind <helper>` binding. Addrmap-level
  passive record (`chip.record_write(addr, data)` decoding across
  instances) is a natural follow-up but not in this slice.
- **Policy-agnostic.** A passive record stores what the monitor observed,
  regardless of `ro`/`wo` (the DUT did whatever it did on the wire — the
  mirror reflects that). Policy-aware *prediction* (e.g. `w1c` observed
  write-1 clears) lands with the `w1c`/`w1s`/`rc`/`rs` keyword set.

**Behaviour and footguns:**

The Phase 1f API has a handful of edges that aren't obvious from the
surface syntax. They're listed here so you don't trip over them.

- **The active frontdoor does not fire callbacks.** `regs.A = v` issues
  a bus write through the `via` helper but does *not* invoke
  `regs_cbs.A` — callbacks are wired only to the passive
  `regs.record_write(addr, data)` decode. The split is intentional: a
  callback exists to recompute checker state when a *monitor-observed*
  write lands in the mirror, and the active path is meant for stimulus
  scripts that already know what they wrote. If you want a single hook
  that fires for both, drive the active path then call `record_write`
  with the same `(addr, data)` from the stimulus thread.
- **Re-registering an `on regs.REG` body silently overwrites.** The
  callback holder is a single `std::function` per register; the last
  `on regs.REG ... end on` in lexical order wins. There is no warning.
  If two test components both want to observe the same register, fold
  their bodies into one `on` block (or split the regblock).
- **Unmapped addresses are no-ops.** `regs.record_write(0xFFFF, v)` on
  an address that isn't a declared register falls through every branch
  of the decode and updates nothing; the corresponding
  `regs.record_read(0xFFFF)` returns `0`. This matches the "checker
  shadows what the monitor saw" model — an unrecognized address is the
  checker's problem, not the monitor's — but it means a typo in an
  address literal won't surface as a sim error. Cross-check addresses
  against `<Regblock>_AddrTable` when you suspect drift.
- **DUT reset is not auto-propagated.** When the DUT resets, the
  shadow mirror keeps whatever values it had — the mirror's reset
  values (from `register A @ 0x00 reset 0x...`) are applied only at
  test construction, not on subsequent DUT reset events. If your DUT
  has a runtime reset that should clear shadow state, drive
  `record_write` for each register from the reset-observing logic.
- **Callback recursion is bounded by `HARC_RAL_CB_MAX_DEPTH = 16`.** A
  callback body can itself call `record_write` — common when a CSR
  write triggers a derived-state write to another register. The
  codegen wraps each decode in a per-binding depth counter; if the
  counter crosses 16, the TB logs a `FATAL` (`sim_log_line("FATAL",
  ...); errors++; _fatal = true`) and the current test instance
  aborts at end of cycle. 16 is deep enough for realistic CSR
  cascades and shallow enough to catch a self-write
  (`on regs.A { regs.record_write(0x00, data) }`) before the C++
  stack blows. Override at compile time by defining
  `HARC_RAL_CB_MAX_DEPTH` before the generated TU is compiled. (The
  `static constexpr` declaration is guarded with `#ifndef`.)
- **Closures capture by `[&]`.** Same model as `on obj.method` hooks
  (cross-ref harc-com#316). Mutation of test-scope scoreboards from a
  callback is the intended use; sharing scoreboard state across
  threads needs the same locking as any other `[&]` capture in the
  run scope.

See `tests/fixtures/regblock_record_test.harc` for the end-to-end shape.

## 4. Surface — `addrmap`  *(Phase 1e shipped, advanced composition pending)*

What shipped: flat addrmap with multiple instances at distinct bases.
Helper-routed (`via Helper`) like regblock. Access patterns
`chip.inst.REG` and `chip.inst.REG.FIELD` work end-to-end.

```harc
addrmap SocRegs via AxilHelper
    instance dma0  : AxiDmaRegs @ 0x4000_0000
    instance uart0 : UartRegs   @ 0x5000_0000
end addrmap SocRegs
```

### Composition rules

| Rule | Status |
|---|---|
| Flat addrmap with multiple instances at distinct bases | **Shipped** |
| `alias of` (two windows over one mirror cell) | **Proposed** |
| Nested addrmaps (addrmap inside addrmap) | **Proposed** |
| `size` clause + overlap checking | **Proposed** |
| Multiple addrmaps over the same regblock type at different bases | **Shipped** (just declare two addrmaps; each has its own mirror tree per `let` instance) |
| Per-instance bus binding override | **Proposed** |

`bound to <Bus>` (the original RFC vocabulary) is not what shipped;
the helper transactor routes bus traffic via the `via <Helper>`
clause on both `regblock` and `addrmap`. See §10 for the rationale.

## 5. Surface — `mem`  *(Proposed — not yet implemented)*

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

## 6. Surface — `extend`  *(Proposed — not yet implemented)*

HARC already has `extend test T` (see [spec.md §8](../spec.md)); the same
pattern is proposed for regblocks and addrmaps:

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

The lowering sections below mix shipped and proposed material. Items
covered by Phase 1a–1e (regblock + addrmap + field-level access +
ro/wo + read predict + bitbash) match the production codegen
mostly-faithfully — the one substantive divergence is `via Helper`
vs `bound to <Bus>` for naming the protocol layer (§7.4). `mem`
and backdoor lowering remain proposed.

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

### 7.3 Dense mem heap-boxing  *(Proposed — `mem` is not implemented yet)*

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

### 7.4 Frontdoor access  *(shipped, with one divergence from the RFC)*

The shipped lowering routes through a user-supplied helper transactor
named with the `via <Helper>` clause, **not** an auto-synthesized
accessor on a `bound to <Bus>` decl. The helper exposes
`write(addr, data)` and `read(addr) -> data` methods (typically a
~20-line `hookable` pair against a stdlib bus type — see the
existing `axilite_regs_full_test` and the new `regblock_*` fixtures
for the convention). Auto-derived bus accessors from a `bound to`
clause remain on the roadmap; the `via Helper` form ships now
because it composes cleanly with the existing transactor machinery
without teaching the codegen about each bus protocol.

`regs.dma0.DMACR.RS = 1` (4-level addrmap+subfield) actually lowers
to:

```cpp
chip.dma0.DMACR = (chip.dma0.DMACR & ~((uint32_t)0x1u << 0))
               | ((((uint32_t)(1)) & 0x1u) << 0);
AxilHelper_write(helper, (0x4000'0000ull + 0x00ull), chip.dma0.DMACR);
```

For RO fields the bus write is dropped with a `// RO field — write
to bus suppressed (...)` marker comment so the codegen output stays
auditable. Reads use the assignment-expression form
`(chip.dma0.DMACR = AxilHelper_read(helper, EFFECTIVE_OFFSET))` so the
mirror sees the bus return (read-side predict) and the expression
yields the read value.

WO registers/fields serve reads from the mirror without bus traffic
(a real WO register would return garbage on a read; the mirror-only
path lets `let x = regs.WO_REG; assert x == prev_write` round-trip
cleanly).

For `w1c`/`w1s`/`rc`/`rs`, the policy-aware mirror update will follow
once those keywords ship. The Phase 1c slice covers `rw`/`ro`/`wo`
explicitly; the remaining policies parse only as errors directing
users at this doc.

### 7.5 Backdoor access  *(Proposed — not yet implemented)*

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

### 7.6 Constraint randomization  *(Proposed — works in principle on top of the existing `randomize` lowering, but no fixture currently exercises `randomize(regs.…)`)*

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

### 7.7 Bit-bash / walk-all sequences  *(shipped)*

`bitbash(regs)` is a codegen builtin that expands at compile time to a
flat sequence of writes and reads over each RW register: write
all-ones, read back, compare; write zero, read back, compare.
RO/WO registers are skipped with a marker comment. Mismatches bump
the test's `errors` counter via `sim_log_line("FAIL", ...)`.

Currently shipped: walk-all over a top-level regblock binding
(`bitbash(regs)`). Per-instance filtering (`bitbash(chip.dma0)`)
and field-policy-aware patterns (W1C write-1-then-readback) are
proposed for later phases once the policy keyword set expands.

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

### Phase 1 — shipped across PRs #95 / #97 / #98 / #99 / #100

1. **`regblock` parse + AST** — width/reset/access on registers,
   `field` decls with bit position and width derived from type.
   Single-line and block-form registers coexist. (PR #95 / #97)
2. **Mirror struct codegen** — POD, struct-of-structs for
   addrmaps. Reset values populate via C++ field default
   initializers. (PR #95 / #100)
3. **Address tables** — `constexpr <Regblock>_AddrTable[]` per
   regblock. Currently used for documentation; future `bitbash`
   variants and overlap checks will consume them. (PR #95)
4. **Frontdoor R/W lowering** — via the `via <Helper>` clause and
   the helper's `write(addr, data)` / `read(addr) -> data`
   methods. The RFC's `bound to <Bus>` form is still on the
   roadmap; helper-routed shipped first because it composes with
   the existing transactor machinery without protocol-specific
   codegen. (PR #95)
5. **Read-side mirror predict** — every bus read updates the
   mirror via `(mirror = bus_read())` assignment expression form.
   (PR #98)
6. **Access policies `rw` / `ro` / `wo`** — `ro` drops bus writes;
   `wo` serves reads from the mirror without bus traffic. (PR #98)
7. **`bitbash(regs)` builtin** — compile-time-unrolled all-ones +
   zero pattern walk over RW registers; RO/WO skipped with marker
   comments. (PR #99)
8. **Flat `addrmap` composition** — multiple regblock instances at
   distinct base addresses; 3-level `chip.inst.REG` and 4-level
   `chip.inst.REG.FIELD` access. (PR #100)

### Deferred — testability concern

`w1c` / `w1s` / `wclr` / `wset` / `rc` / `rs` policies parse-error
today, pointing the user at this doc. Honest end-to-end tests
require a W1C-aware DUT; the existing `AxiLiteRegs` fixture DUT
acts RW from the bus side, so a `regs.IRQ = 1` write to a W1C
register would look indistinguishable from a `rw` write at the
bus. The codegen for these policies is small (mirror-update
predicate), but landing them without a credible end-to-end test
risks shipping silent-bug behavior. They land alongside a
W1C-aware DUT (or RDL-generated DUT pair).

### Phase 2 (proposed)

1. `mem` decl in `sparse` and `dense` modes (no `none` / reference yet).
2. `backdoor` accessors for registers and dense mems.
3. Multi-level `addrmap` nesting.
4. `alias of` instance aliasing.
5. Multiple-map support (`addrmap SocRegsDebug` parallel to `SocRegs`).
6. `extend regblock` override syntax.
7. `size` clause on addrmap instances + overlap checking.
8. Constraint integration: `randomize(regs.path.field)` plumbed
   into the existing Z3 lowering.

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
- **Shipped RAL fixtures** (use these as the canonical syntax reference
  for what works today):
  - [`tests/fixtures/regblock_basic_test.harc`](../tests/fixtures/regblock_basic_test.harc) — Phase 1a, register-level R/W via helper
  - [`tests/fixtures/regblock_fields_test.harc`](../tests/fixtures/regblock_fields_test.harc) — Phase 1b, field-level decomposition
  - [`tests/fixtures/regblock_access_test.harc`](../tests/fixtures/regblock_access_test.harc) — Phase 1c, ro/wo policies + read predict
  - [`tests/fixtures/regblock_bitbash_test.harc`](../tests/fixtures/regblock_bitbash_test.harc) — Phase 1d, walk-all builtin
  - [`tests/fixtures/regblock_addrmap_test.harc`](../tests/fixtures/regblock_addrmap_test.harc) — Phase 1e, addrmap composition
- **PR history** for the shipped phases:
  - PR #95 (1a regblock), #97 (1b fields), #98 (1c read predict + ro/wo),
    #99 (1d bitbash), #100 (1e addrmap)
- Pre-RAL axilite test patterns (still pass, kept for protocol-level coverage):
  [`tests/fixtures/axilite_regs_full_test.harc`](../tests/fixtures/axilite_regs_full_test.harc),
  [`tests/fixtures/axilite_constraint_test.harc`](../tests/fixtures/axilite_constraint_test.harc)
- `rdl2arch` (sibling DUT-side generator): https://github.com/arch-hdl-lang/rdl2arch
- `rdl2arch-riscv` (RISC-V CSR specialization): https://github.com/arch-hdl-lang/rdl2arch-riscv
- SystemRDL 2.0 spec (Accellera): the source vocabulary `rdl2harc` will lower from
- `systemrdl-compiler` (MIT-licensed RDL frontend): the parser both
  `rdl2arch` and the future `rdl2harc` build on
