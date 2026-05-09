# HARC v1 Specification

*A verification language paired with the ARCH HDL — the **harness** for stimulus, properties, coverage, and testbench architecture.*

> **HARC** = "harness of ARCh". File extension `.harc`, CLI `harc`. Pronounced as a single syllable.

---

## 0. Goals and Non-Goals

**In scope (v1):**
- Temporal properties, assertions, assumptions, cover points (replaces SVA + PSL)
- Compositional formal contracts (assume/guarantee at module boundaries)
- Constrained-random stimulus with relational constraints
- Functional coverage — type-derived and explicit
- UVM-equivalent testbench architecture as native language constructs (env, agent, driver, monitor, sequencer, tseq, scoreboard)
- Reference model embedding (ARCH functional modules, C functions, Sail-imported semantics)
- Native cycle-based simulator runtime, co-compiled with ARCH (Verilator-class throughput)
- **DUT backend abstraction** — HARC TB binds to ARCH-compiled DUTs (primary, fastest path) *or* SV-via-Verilator-compiled DUTs (interop path; raw signal access in v1)
- SIMD batch CRV (N-lane stimulus parallelism on a single compiled TB) — staged for v1.1
- SV + UVM + SVA transpile for sign-off and tool portability
- Direct BTOR2 / SMT-LIB2 formal export

**Out of scope (v1):**
- Wave / coverage GUI tooling — interoperate via standard formats (FSDB / VCD / UCDB)
- Power / UPF verification — defer to ARCH side
- Mixed-signal / AMS
- Emulation backend beyond a synthesizable assertion subset (full TB-on-emulation deferred to v2)
- Commercial-simulator DUT backends (VCS / Xcelium / Questa via DPI/VPI co-sim) — v1.1+; same DUT abstraction layer extended with vendor-specific eval shims
- VHDL DUTs — v1.1+ via GHDL or Verilator's experimental VHDL path; the DUT abstraction layer admits this without redesign
- Protocol-typed grouping of raw SV signals — v1 ships raw signal access; convention-based or explicit binding for `bus`-typed access deferred to v1.1 once real DUT integration experience informs the design

**Non-goals on principle:**
- Backward source compatibility with SV/UVM testbenches (transpile is *output* only)
- A class-based object model
- Phase-driven lifecycle as a first-class user concept

---

## 1. Design Principles

The language inherits five constraints from being a sister to ARCH; everything else follows:

1. **One frontend.** HARC and ARCH share lexer, parser, expression grammar, type system, and elaboration. A `.harc` file and a `.arch` file compile in one invocation; cross-references are typed.
2. **One IR.** HARC lowers to the same IR as ARCH. Properties, coverage, transactions, and TB components are IR-level constructs. The native simulator and the SV/UVM transpiler are both IR consumers.
3. **Co-elaboration.** A parameterized port and the assertions / coverage on it are emitted in the same elaboration pass, by the same type substitution. The April-15 verification-co-generated-with-ports problem becomes structural rather than aspirational.
4. **One coherent semantics.** Properties have one meaning across simulation, formal, and the synthesizable subset. The backend chooses *which* meaning is realized — never *what* is meant.
5. **Constructs over framework.** Anything UVM achieves through library convention (factory, config DB, phase macros, field macros, virtual interfaces) is either a first-class language construct or eliminated as unnecessary given a real type system.
6. **Ride on ARCH primitives, don't parallel them.** Where ARCH already provides a primitive, HARC reuses it as the lowering target rather than reinventing one. Sequences lower to ARCH `thread`s. Drivers and monitors bind to ARCH `bus` declarations and dispatch through the existing `handshake_channel` / `credit_channel` / `tlm_method` machinery. Tests lower to ARCH `testbench`. Properties extend ARCH's `assert` / `cover` / `assume` plus the planned temporal sugar (`a |=> b`, `past(e, N)`, `rose(a)`, `##N e`). HARC adds the missing verification-side abstractions (transactions, constraints, coverage, env/agent/scoreboard, aspects) on top — it does not duplicate the primitives ARCH already ships. See §16 for the lowering map.
7. **LL(1) grammar.** HARC inherits ARCH's LL(1) commitment (ARCH §2.4): every production is decidable from one token of lookahead, no backtracking. This is what makes both languages tractable for AI codegen — a parser can commit to a parse tree from the leading token of every construct, which is the same property that lets an LLM emit syntactically valid code without seeing the trailing context. Every HARC keyword has a distinct FIRST set in every position it can appear; ambiguous compound forms like `cover sequence` vs `cover property` vs `cover propname` resolve by single-token lookahead after the leading keyword. New language features must preserve LL(1); see §2 for the disambiguation rules at potentially-ambiguous sites.

---

## 2. Relationship to ARCH

**File layout.** `.arch` for design, `.harc` for verification. Mixed projects compile both in one pass.

**Module visibility.** A `.harc` file uses an ARCH module or package the same way ARCH files do (`use`, per ARCH §29) and gets typed access to its ports, parameters, internal signals (with `internal` access modifier), and protocol-typed interfaces.

```
use arc.dut.AxiSlave        // an ARCH module

module AxiSlaveTb
    let dut: AxiSlave#(AW=32, DW=64) = bind ...
    on dut.axi_s.aw.handshake
        ...
    end on
end module AxiSlaveTb
```

**DUT backend.** ARCH-compiled DUTs are the primary path — same IR, same compiler pass, no marshaling. HARC also supports SV DUTs compiled through Verilator (§10.5), so existing SV codebases can be driven by HARC TBs without a full HDL migration. The TB code is identical regardless of DUT backend; only the elaboration-time `bind` differs:

```
// ARCH DUT (default, fastest path):
use arc.dut.AxiSlave
let dut: AxiSlave#(AW=32, DW=64) = bind ...

// SV DUT via Verilator:
module my_axi_slave kind verilator
    src: "rtl/axi_slave.sv"
    top: my_axi_slave_top
    param ADDR_W: int = 32
    param DATA_W: int = 64
    clocks: { aclk: tb_clk }
    resets: { aresetn: tb_rst_n }
end module my_axi_slave
let dut: my_axi_slave = bind ...
on dut.s_axi_awvalid
    ...                          // raw signal access — protocol grouping is v1.1
end on
```

The cycle-based runtime (§7.1, §10.1) calls `dut.eval_domain(D)` on every cycle regardless of backend; the binding generates the right C++ glue at compile time. See §10.5 for the full DUT backend spec.

**Shared types.** Everything ARCH defines is available in HARC:
- Bitvectors `bits<N>`, `uint<N>`, `sint<N>` with width arithmetic
- Struct, ADT (sums-of-products), enum, vector
- Pipeline, FSM, FIFO, counter, regfile
- Clock-domain types
- Protocol-typed interfaces (AXI / AXIS / etc. with handshake sequencing in the type)

**Shared elaboration.** Generics, traits, type-level naturals — same machinery. A protocol type's handshake sequencing is what the HARC driver/monitor are *derived from*; you do not re-author the protocol.

**What HARC adds, lexically.** New keywords, reserved only in `.harc` files (so existing ARCH code is unaffected):

```
assert assume cover property pseq
solve_before solve_after dist
transaction agent env driver monitor
sequencer tseq scoreboard ref phase weight
on after fork join_any join_all join_none emit
scope setup run check teardown test
blocking comb across with default
keep extend when hookable pre post apply
parallel schedule select                  // tseq composition operators (§17.1)
buffer stream state                       // flow object types (§17.2)
```

The composition operators (`parallel`, `schedule`, `select`) are scoped to `tseq` bodies — borrowed from PSS activity composition (§17.1). The flow object types (`buffer<T>`, `stream<T>`, `state<T>`) are first-class types alongside `event<T>` (§17.2).

Attribute syntax `[name]` and `[name(args)]` annotates declarations: `[cyclic]`, `[unique]`, `[dist {...}]`, `[range(...)]`, etc. On field declarations attributes follow the type, introduced by the `with` keyword (`name : type [default <value>] [with [attr]+]`); on other declarations they prefix. Attributes are extensible without grammar changes.

**Keywords shared with ARCH** (same primitive, same meaning, used in HARC files where the construct applies): `module`, `bus`, `port`, `param`, `let`, `reg`, `comb`, `seq`, `assert`, `assume`, `cover`, `use`, `function`, `package`, `domain`, `Clock`, `Reset`, `thread`, `wait`, `lock`, `testbench`, `task`, `init`, `repeat`, `log`, plus all primitive types (`UInt`, `SInt`, `Bool`, `Vec`, `struct`, `enum`).

Note in particular that `seq` retains its ARCH meaning of *registered logic block* — and the bareword `sequence` is not a HARC type or stand-alone keyword. The temporal sequence type is `pseq` (§3.4); the test sequence construct is `tseq` (§8.4). The bareword `sequence` reappears as a keyword in exactly one compound form: `cover sequence` for behavioral sequence coverage (§17.3) — the linguistic fit there is strong enough to merit the carve-out. HARC has three kinds of sequences with deliberately symmetric names:

- **`tseq`** — *test sequence*. Stimulus generators that yield transactions to a sequencer (§8.4). Lower to ARCH `thread`s.
- **`pseq`** — *property sequence*. Temporal patterns used inside `assert property` / `cover property` / `assume property` blocks (§3.4, §5). Lower to ARCH temporal sugar (`a |=> b`, `##N e`, etc.) plus shadow registers for multi-cycle relations.
- **`cover sequence`** — *behavioral sequence*. Coverage patterns over orderings of events (§17.3). Lower to compile-time-constructed FSMs over event observations.

The `t` / `p` prefix on the first two tells you immediately which side of verification you're looking at: stimulus or properties. Behavioral coverage is its own thing and uses the explicit two-word form.

And `use` is the ARCH-style namespace import (§3.6), distinct from `apply` which activates an aspect within a scope.

`bus` and `dut` are conventional, not reserved.

**LL(1) disambiguation at potentially-ambiguous sites.** HARC is LL(1) (per §1, principle 7). The places where multiple constructs share a leading keyword resolve by single-token lookahead:

| Leading token | Next token decides production |
|---|---|
| `cover` | `sequence` → behavioral sequence (§17.3); `property` → property cover; IDENT → cover named property |
| `module Name` | `#(` → parameters first; `kind` → external backend variant (§10.5); newline → ARCH-source body opens directly |
| `agent Name` (and `driver`/`monitor`/`env`) | `#(` → parameters; `bound` → bound-to clause; newline → body opens |
| `name : type` (field decl) | `default` → default clause; `with` → attributes; newline / `,` → end of field |
| `on event-expr` | `pre` → pre-hook; `post` → post-hook; newline → main handler body |
| `assert`/`assume` | IDENT (followed by `;` or end-of-line) → named property; expression → inline boolean or temporal property (the temporal operators `|=>`, `##N`, etc. are part of the expression grammar, not separate productions) |
| `end` | `module` / `transaction` / `agent` / etc. → close that named declaration; `on` / `fork` / `when` / etc. → close anonymous compound block |

New language features must preserve this property. The check is mechanical: for every keyword that introduces a construct, FIRST sets of all possible continuations must be pairwise disjoint.

**Block syntax: end-construct style throughout.** HARC follows ARCH's `end <kind> [<name>]` convention for all block bodies — no curly braces for declaration bodies, statement-block bodies, or compound expressions. A named declaration closes with `end <kind> <name>` (e.g., `end module AxiSlaveTb`); an anonymous compound block closes with `end <kind>` (e.g., `end on`, `end fork`, `end when`). The named-end form is the parser-validating, AI-codegen-friendly shape that ARCH §2.4 commits to, and HARC mirrors it. Curly braces are reserved for *value literals* — set literals (`{READ, WRITE}`), distribution literals (`dist {[0..0xFF] :/ 80}`), record/struct literals — never for blocks.

**Discard pattern `_` in binding positions.** A lone underscore is a binding name that says "I need to introduce a binding here but don't intend to read it." It is not an identifier — it cannot be referenced from the body, and reusing the same `_` in nested binders does not collide. v1 admits `_` in two positions:

- The for-loop variable: `for _ in 0 .. N ... end for` — repeat a body N times without naming the index.
- The randomize-result discard (planned, Phase 1b): `randomize(_) with ...` — when the call is for its constraint side-effect rather than the produced value.

```
for _ in 0 .. 10
    wait 1 cycle             // common idiom: just spin N cycles
end for
```

A bare `wait N cycles` (§7.4) is the cleaner form when the loop body is *only* the wait. Use a real loop variable name (`for i in 0..N`) when the body actually consumes the index; use `_` when the body does not. Borrowed from Rust and Python.

**Ternary `cond ? then : else`.** A C/SystemVerilog-style conditional expression. Right-associative; precedence is just above implication (`|->`, `|=>`) and below every other operator, so `a + b ? x : y` parses as `(a + b) ? x : y` and `a ? b : c ? d : e` parses as `a ? b : (c ? d : e)`. There is no `if`-as-expression in HARC; for value selection inside an expression context, use ternary.

```
let h = dut.alloc_resp_valid ? dut.alloc_resp_handle : 255
let bound = i < hi ? i : hi - 1
```

Lowers to a parenthesized C++ ternary in the generated TB.

---

## 3. Type System Extensions

HARC adds a small set of types on top of ARCH's:

### 3.1 Default-rand fields with attributes

Fields in `transaction` and `struct` types are **random by default**. The `rand` keyword does not exist; instead, opt out with `!` for non-random fields, append `default <value>` after the type for a default value, and append `with [attr]+` for randomization modifiers.

```
transaction AxiWrite
    addr  : uint<64>                                                    // rand by default
    data  : Vec<bits<64>, 256>
    len   : uint<8>
    burst : BurstType

    id    : uint<4>  with [cyclic]                                      // cycles through values; randc semantics
    size  : uint<8>  with [dist {[0..0xFF] :/ 80, [0x100..] :/ 20}]     // distribution attribute
    tag   : uint<8>  with [unique within tseq]                          // distinct across this tseq's emissions

    !mode : AxiMode default NORMAL                                      // not random; default settable in code
end transaction AxiWrite
```

**Field syntax:** `[!] name : type [default <value>] [with [attr]+]`. The field name and type lead. Two optional clauses follow, each keyword-introduced: `default <value>` for the value when not randomized (or pre-randomize for random fields), and `with [attr]+` for randomization modifiers. Both clauses are independently optional; when both are present, `default` precedes `with`.

```
addr : uint<64>                                                  // random, no default, no attrs
size : uint<8> default 0                                         // random, with default
size : uint<8> with [cyclic]                                     // random, with attrs
size : uint<8> default 0 with [dist {[0..0xFF] :/ 80, [0x100..] :/ 20}]
!mode : AxiMode default NORMAL                                   // non-random with default
```

Multi-attribute under one `with`:

```
size : uint<8> default 0 with [dist {[0..0xFF] :/ 80, [0x100..] :/ 20}] [weighted(zero_bias)]

// or, when attributes are long:
size : uint<8> default 0 with
    [dist {[0..0xFF] :/ 80, [0x100..] :/ 20}]
    [weighted(zero_bias)]
```

**Why keyword-introduced clauses, not `=`.** The `=` token is overloaded across many contexts (type aliases, expression assignment, comparison-derived constraints). Keyword-introduced clauses make every field declaration self-documenting: see `default`, expect a value; see `with`, expect attributes; see neither, the field is plain random. Searchability is also direct — `grep "default "` finds every default-bearing field, `grep "with \["` finds every attributed field.

**The prefix modifier `!` (non-random):** storage-class marker, conceptually the inverse of randomization. Use for fields that are settable in code but never randomized — config knobs, mode bits with explicit values, sideband state.

**No `const` inside transactions.** A transaction is a randomizable value record; constants don't belong to its body. For test-bench-wide constants — protocol versions, magic numbers, table sizes — use package-level or module-level `const` declarations (per ARCH §29) and reference them by name from inside the transaction. This keeps the transaction body to its two purposes: fields and constraints. Anything that isn't either of those lives elsewhere.

```
// at package scope:
const AXI_VERSION : uint = 1
const MAX_BURST_LEN : uint = 256

transaction AxiTxn
    addr : uint<64>
    len  : uint<8>
    keep len <= MAX_BURST_LEN          // refers to package-level const
end transaction AxiTxn
```

The `with` keyword here is the same one used in `randomize(t) with { ... }` (§4.4) — both introduce additional randomization properties attached to the surrounding context. At field-declaration time the properties are baked into the type; at call time they're scoped to that randomize call. The shared keyword reflects shared semantics.

Rationale for default-rand: in real transactions the overwhelming majority of fields are random; opting *in* to randomness everywhere is noise. Borrowed from Specman e (`!` prefix for non-random). Attributes are extensible — `[range(...)]`, `[weighted(...)]`, etc. can be added without new keywords.

#### `[unique]` scoping

`[unique]` constrains a field's randomized value to be distinct from prior assignments. The scope of "prior" varies; HARC makes this explicit:

```
[unique]                       // unique within one randomize() call (default; SV-equivalent)
[unique within tseq]           // unique across all emissions from this tseq instance
[unique within sequencer]      // unique across all transactions a sequencer ever drives
[unique within test]           // globally unique within this test instance
```

The bareword `[unique]` matches SystemVerilog's per-call `unique` semantics — a single `randomize` against a list/array produces distinct values within that one call. The `within X` clauses extend the lifetime: the runtime maintains a used-value set tied to the named scope; new randomizations sample from the complement.

**Lowering:**

| Form | Runtime cost | Notes |
|---|---|---|
| `[unique]` | free — a solver-side constraint within one call | No persistent state |
| `[unique within tseq]` | per-tseq-instance bitset (or hash for wide types) | State tied to tseq lifetime |
| `[unique within sequencer]` | per-sequencer bitset | State tied to sequencer lifetime |
| `[unique within test]` | per-test bitset | State tied to test instance lifetime |

**Compile-time check.** For `[unique within X]`, the compiler verifies the field's value space is bounded enough that uniqueness is sustainable over the scope's expected emission count. A `uint<4>` field with `[unique within tseq]` has 16 possible values; if the static checker can prove the tseq emits more than 16 transactions, the program is rejected with an "unique exhaustion" error pointing at both the attribute site and the emission site. For dynamically-bounded emission counts, the compiler emits a runtime warning at exhaustion rather than failing compile.

### 3.2 Time

```
type time
let t: time = 100ns      // also: 5ps, 3us, 4cycles
```

`time` is a tagged type, not an integer. Mixed-unit arithmetic requires explicit conversion. `cycles` is resolved against the enclosing `clocking` scope.

### 3.3 Transactions

A `transaction` is a typed record with built-in conveniences for randomization, packing, and scoreboard comparison.

```
transaction AxiWrite
    addr  : uint<64>
    data  : Vec<bits<64>, 256>
    len   : uint<8>
    burst : BurstType
    id    : uint<4>
    strb  : Vec<bits<8>, 256>

    keep len in [1..256]
    keep addr % (1 << size) == 0
    keep len * (1 << size) <= 4096 - (addr % 4096)
end transaction AxiWrite
```

Transactions are **value types** (ADT under the hood). They have structural equality, deep-copy by default, and pack/unpack methods derived from the protocol type they're associated with.

There is no class hierarchy. Reuse and parameterization are via composition, ARCH's traits, and `extend` aspects (§3.6) — not inheritance.

#### Inline `keep` constraints

`keep` declares a constraint that always applies to instances of this transaction. It is inline because the constraint is intrinsic to the type. For external, parameterized, composable constraint sets, use a `relation` (§4); for cross-cutting test-specific constraints, use `extend` (§3.6).

#### `when` subtypes for conditional fields

Borrowed from Specman e. A discriminator field gates which fields exist in the transaction:

```
transaction AxiTxn
    op   : AxiOp
    addr : uint<64>
    len  : uint<8>

    when op == WRITE
        data : Vec<bits<64>, 256>
        strb : Vec<bits<8>, 256>
    end when
    when op == READ
        expected_data : Vec<bits<64>, 256>
    end when

    keep op in {READ, WRITE}    // discriminator value space — single-field, Phase 1a OK
    keep len in [1..256]
end transaction AxiTxn
```

The consumer accesses `t.addr` and `t.len` unconditionally; conditional fields require a refinement (pattern match, `if`, or guarded access) — the compiler enforces this at use-site.

`when` subtypes are the right model when you want a flat-record API with conditional presence. ADTs (sums-of-products from ARCH) are the right model for closed sums where each variant has fundamentally different shape (FSM states, protocol message types). Both are available; pick per use case.

The constraint solver reasons about `when`-conditional fields naturally — they participate in the SAT problem only when their discriminator selects them.

#### Encoding: tagged ADT with per-variant constraint subproblems

`when` subtypes lower to a tagged ADT: discriminator field + per-variant payload. The constraint solver sees the type via Z3's native algebraic-datatype theory (`(declare-datatypes ...)`); each `when` variant is a constructor, and field accesses inside a `when` clause become accessor functions guarded by the discriminator.

The constraint problem decomposes per-variant: at solve time, the discriminator's constraints are satisfied first, then only the active variant's fields and `keep`s enter the SAT/SMT problem. Inactive variants' fields are never allocated in the solver's representation for that solve. This gives:

- **Solver pruning** — unsat variants are eliminated as whole branches, not searched field-by-field
- **Clean composition with `extend`** (§3.6) — adding `when X { keep ... }` via aspect extends only that variant's subproblem; the ADT declaration is unchanged
- **Tractable lane divergence for v1.1 SIMD batch CRV** — lanes group by discriminator, each group solves its variant subproblem in parallel; scatter via AVX-512 lane masking

The cost is a one-time per-type elaboration setup (ADT declaration to the solver, per-variant constraint tree). This is amortized across all `randomize` calls of that type.

### 3.4 Properties as first-class types

```
type prop                    // a temporal property
type pseq                    // a temporal sequence (used inside properties)
type event<T>                // typed event channel — reactive, cycle-aligned
```

`prop` and `pseq` are first-class. You can store them, pass them to generics, build property combinators.

`event<T>` is HARC's reactive primitive. For other dataflow patterns — queued producer/consumer, continuous streams, persistent shared state — see the flow object types in §17.2 (`buffer<T>`, `stream<T>`, `state<T>`), borrowed from PSS.

### 3.5 Coverage carried by types

```
enum BurstType { FIXED, INCR, WRAP }
                 // implicit coverage bins for each variant
```

Any value of type `BurstType` automatically participates in coverage when sampled in a `cov` group. `bits<N>` with `range(...)` annotations get implicit bins. This eliminates the bin-restating problem and lets coverage hit-counts be type-checked against the value space.

### 3.6 `extend` aspects with two-stage `use` + `apply` activation

Borrowed from Specman e, with the AOP-too-magic critique addressed by separating two concerns: making an aspect *visible* and making it *active*. HARC inherits ARCH's `use` keyword (ARCH §29) for namespace import and adds `apply` for scope-local activation.

`extend` adds constraints, fields, or `when` subtypes to an existing type from a separate file, without modifying the original definition. Multiple extensions compose.

```
// file: tests/aspects/short_bursts.harc
package ShortBursts
    extend AxiTxn
        keep len < 16
        keep burst == INCR
    end extend AxiTxn
end package ShortBursts

// file: tests/aspects/aligned_writes.harc
package AlignedWrites
    extend AxiTxn
        keep addr % 64 == 0
    end extend AxiTxn
end package AlignedWrites

// file: tests/test_small_aligned.harc
use tests.aspects.short_bursts        // names visible; aspect NOT yet active
use tests.aspects.aligned_writes

test SmallAligned
    apply ShortBursts                 // active in this test only
    apply AlignedWrites
    ...
end test SmallAligned
```

**Two-stage rationale.**
e applies aspects globally based on file load order, which is the legitimate "where did this come from" critique. HARC splits the two questions:

- `use` — same as ARCH (§29): brings package names into scope. Pure name resolution; no compile-time effect on which extensions are active. Two `use` lines never conflict.
- `apply` — activates the named aspect's `extend` blocks within the enclosing scope (`test`, `env`, or `scope`). The activation is textually visible at the activation site; `grep -n "apply ShortBursts"` finds every site where the aspect takes effect.

This is a strict win over e: the `use` line tells you the aspect *exists*; the `apply` line tells you where it *takes effect*. Aspects defined in a `use`d package but not `apply`d in any scope have no observable effect.

**Composition rules.**
- An `apply` activates the named package's `extend` blocks for the lexical extent of the enclosing scope.
- Multiple `apply` lines in the same scope compose by intersection of constraints (all `keep`s must hold) and union of fields / `when` clauses.
- Conflicts at the field level (two extensions adding the same field name with different types) are compile errors. Conflicts at the constraint level (unsatisfiable composed constraints) are runtime solver failures with both sources reported.
- Nested scopes inherit applies from their enclosing scope; nested `apply` adds to the inherited set, never removes.

**Depth-1 rule: `extend` targets base type declarations only.**

This is the single most important constraint on the aspect system, and it's what addresses the "where does this come from" critique of unrestricted AOP. An `extend` block must name a type declared with `transaction`, `struct`, `agent`, `env`, etc. — never another `extend` or aspect package.

```
transaction AxiTxn ... end transaction AxiTxn   // base declaration

package ShortBursts
    extend AxiTxn                                // OK — extends the base
        keep len < 16
    end extend AxiTxn
end package ShortBursts

package StrictBursts
    extend ShortBursts.AxiTxn                    // ERROR — would be depth 2
        ...
    end extend ShortBursts.AxiTxn
end package StrictBursts
```

What this rules out: extension chains. What it allows (and is sufficient): multiple flat extensions of the same base. Composition is realized at `apply` sites by stacking activations, not by chaining extension definitions:

```
extend AxiTxn keep A end extend AxiTxn          // package A, depth 1
extend AxiTxn keep B end extend AxiTxn          // package B, depth 1
extend AxiTxn keep C end extend AxiTxn          // package C, depth 1

test T
    apply A; apply B; apply C                   // effective type: AxiTxn ∧ A ∧ B ∧ C
end test T
```

The "chain" lives at activation sites. `grep -n "apply"` recovers the entire composition for any test; `grep -n "extend AxiTxn"` enumerates every contributor to the type. Both queries are linear, both are textually visible, neither requires walking a graph.

**Why depth 1 is sufficient.** Specman e allows arbitrary extension depth, and real e codebases routinely have transactions whose effective constraint set is assembled from chains across many files. Reading any single file in such a chain reveals only a partial picture, and the composition order matters subtly. Depth-1 forfeits no expressivity: every multi-level chain in e can be rewritten as multiple depth-1 extensions composed via `apply` stacking. What it forfeits is the ability to *partially* extend an extension, which is precisely the source of the auditability problem.

**Edge case: extension B references a field that extension A adds.** If `apply B` is activated without `apply A`, B's reference to A's field is a runtime error at the activation site, with both source locations reported. Restructuring is the escape hatch — merge A and B into one package, or split the field-adding extension from the field-constraining one such that the constraint extension is conditional on the same `apply` set. v1 ships this as the rule; v1.1 may add an explicit `requires` clause on packages for declarative dependency capture (§13).

**What can be extended.**
- `transaction` — add fields, `keep` constraints, `when` subtypes
- `struct` — same
- `agent` / `env` / `driver` / `monitor` / `scoreboard` — add fields, `connect` clauses, `on` handlers
- `ref module` — not extendable (refs are spec-derived; extension would defeat the purpose)
- ARCH design modules — not extendable from HARC (design-side extensions require ARCH-side changes)
- Other aspect packages — not extendable (the depth-1 rule above)

**What cannot be extended.**
Free-form method body replacement is explicitly out (see §12). Extensions add to types and components; they do not rewrite existing methods. To intercept method calls, use `pre`/`post` hooks (§7.3).

---

## 4. Stimulus and Constraints — Three Forms

The April-13 principle holds: **constraints are relations, not directives**. HARC provides three syntactic forms for declaring them, all sharing the same underlying semantics:

| Form         | Where it lives                | Use for                                              |
|--------------|-------------------------------|------------------------------------------------------|
| `keep` block | Inline in `transaction`/`struct` (§3.3) | Constraints intrinsic to the type            |
| `relation`   | Free-standing, named, composable | External, parameterized, reusable constraint sets |
| `extend`     | Imported aspect (§3.6)        | Test-specific, cross-cutting constraints             |

A free-standing `relation`:

```
relation AxiBurstLegal(t: AxiWrite)
    t.burst != WRAP || (t.len in {2, 4, 8, 16})
    t.addr % (1 << t.size) == 0
    t.len * (1 << t.size) <= 4096 - (t.addr % 4096)
end relation AxiBurstLegal
```

is used in three contexts, regardless of which form declared it:

1. **Stimulus generation** — `randomize(t) with AxiBurstLegal(t)` — the solver finds satisfying values.
2. **Monitor checking** — `assume AxiBurstLegal(t_obs)` checks the DUT honored the protocol; `assert AxiBurstLegal(t_gen)` checks legality of generated stimulus.
3. **Formal** — exported to SMT-LIB2 directly; participates in compositional contracts.

No driver/monitor duplication. No inheritance ladder to add a constraint.

### 4.1 Solve hints, distributions, and auto-lowering

```
randomize(t) with
    solve_before(t.burst, t.len)
    t.addr dist { [0..0xFFFF] :/ 80, [0x1_0000..] :/ 20 }
end randomize
```

#### Auto-lowering simple `keep` to attribute-equivalent code

Phase 1a does not link the SMT solver. Single-field constraints whose right-hand side is a constant expression auto-lower to attribute-equivalent code at elaboration — they generate the same PRNG sampling code that `[range(...)]`, `[weighted(...)]`, `[dist {...}]`, or `[cyclic]` would. The user can write whichever form is more natural:

| `keep` form | Auto-lowers to | Phase |
|---|---|---|
| `keep f in [a..b]` (constants) | `[range(a, b)]` — uniform PRNG sampling | 1a |
| `keep f == c` (constant) | constant assignment — no PRNG call | 1a |
| `keep f in {a, b, c}` (constants) | `[weighted(a, b, c)]` — equal-weight PRNG choice | 1a |
| `keep f != c` (single hole over wide range) | runtime rejection-sample | 1a (with warning) |
| `keep <multi-field constraint>` | requires solver — Phase 1b error | 1b |
| `keep f != g` (cross-field) | requires solver — Phase 1b error | 1b |
| `keep f * g <= K` (arithmetic, multi-field) | requires solver — Phase 1b error | 1b |

The static checker rejects multi-field constraints in Phase 1a with a clear error pointing at the offending term and citing the field count. Single-field constants-only forms compile without the solver linked.

This makes `keep` and attributes equivalent for simple cases, and the attribute form a *concrete syntactic alternative* rather than a separate Phase 1a-vs-1b feature gate. Style guidance: use attributes when the constraint is intrinsic to the field (always applies, part of the type's contract); use `keep` when the constraint is composed with others or expresses a relationship even if currently constant. Attributes are declaration-style, `keep` is constraint-style — same lowering for the simple cases.

#### `[dist]` attribute vs. `dist` directive — when to use which

Two ways to specify a weighted distribution. They differ in scope:

**Field-level `[dist]` attribute** — distribution is intrinsic to the type. Every randomization of any instance of this transaction uses it.

```
transaction AxiTxn
    size : uint<3> with [dist {6 :/ 90, [0..5] :/ 10}]   // 90% size=6 (cache-line aligned)
end transaction AxiTxn
```

**`dist` directive inside `randomize ... with`** — distribution is ad-hoc, scoped to one call. Doesn't affect the type.

```
let t: AxiTxn
randomize(t) with
    t.size dist { [0..2] :/ 100 }    // this call wants only small sizes
end randomize
```

When both are present, the directive wins for that specific call. A worked example showing both — typical AXI verification mix where most stimulus is cache-line aligned but targeted regressions hit unaligned cases:

```
transaction AxiTxn
    size : uint<3>
    addr : uint<64>

    // Intrinsic: 90% of generated traffic is cache-line-aligned
    size with [dist {6 :/ 90, [0..5] :/ 10}]
end transaction AxiTxn

// Normal regression: uses the intrinsic distribution
tseq Regression(n: int) -> TSeq<AxiTxn>
    for _ in 0 .. n
        let t: AxiTxn
        randomize(t)                                // 90% size=6, 10% spread across 0..5
        yield t
    end for
end tseq Regression

// Targeted: hit unaligned cases harder
tseq UnalignedStress(n: int) -> TSeq<AxiTxn>
    for _ in 0 .. n
        let t: AxiTxn
        randomize(t) with
            t.size dist { [0..2] :/ 100 }            // override: small sizes only
        end randomize
        yield t
    end for
end tseq UnalignedStress
```

**Style guidance.** Use the attribute when the distribution reflects the field's natural skew everywhere it occurs (protocol-level realism, traffic shaping). Use the directive when the test wants to override or focus on a sub-range (targeted regression, edge-case stress). The attribute is the default; the directive is the override.

### 4.2 Composability

```
relation AxiAlignedBurst(t: AxiWrite) = AxiBurstLegal(t) && t.addr % 64 == 0
```

Relations are values. They can be passed, intersected (`&&`), unioned (`||`), and parameterized.

### 4.3 Solver

Z3 by default; Bitwuzla available for pure-bitvector workloads (per April-13). Solution diversity is via blocking clauses + stratified sampling — implementation detail, not user-facing.

### 4.4 Execution model — queued by default, `blocking` opt-in

`randomize` does not stall the simulator cycle by default. The compiler enqueues the request to an off-cycle solver pool; the result is delivered through an implicit single-shot channel that the consuming `tseq` awaits at use-site.

```
tseq RandomWrites(n: int) -> TSeq<AxiWrite>
    for _ in 0 .. n
        let t: AxiWrite
        randomize(t) with AxiBurstLegal(t)   // queued; solver runs off-cycle
        yield t                              // implicit await on the result
    end for
end tseq RandomWrites
```

Queue depth is a per-tseq parameter (default 16). Steady-state, the solver pool stays ahead of consumption; sim cycles do not block on Z3.

For closed-loop stimulus where the next transaction depends on observed runtime state, use `blocking randomize`:

```
on dut.error_irq
    let t: ErrorRecoveryTxn
    blocking randomize(t) with t.error_id == dut.last_error_id
    drv.send(t)
end on
```

`blocking` stalls the cycle while the solver returns. The keyword is mandatory whenever the constraint references runtime DUT state — the compiler detects this dependency and rejects un-annotated calls with a compile-time error pointing at the offending term. (Conservative dataflow analysis; false positives can be silenced by binding the runtime expression to a snapshot variable before the call.)

The default-queued model is what makes the cycle-based simulator (§10.1) keep its perf in the presence of a heavyweight SMT solver. Z3 calls in tight loops would otherwise dominate.

### 4.5 List operations and quantifiers

Borrowed from Specman e. Lists (and ARCH `Vec`s) carry rich functional operators that compose with constraint expressions. ARCH already provides `map` / `fold` / `zipWith` / `scan` (per April-17 thread); HARC adds verification idioms:

```
let total_bytes = all_writes.sum_of(.len)
let pending     = all_writes.count_of(.op == WRITE && !.completed)
let oldest      = all_writes.first_of(.timestamp < t_threshold)
let any_failed  = all_writes.any(.error)
let all_aligned = all_writes.all(.addr % 64 == 0)
```

The `.field` shorthand inside the predicate refers to the iterating element. These compose into constraints:

```
keep all_writes.all(.addr % (1 << .size) == 0)        // every burst aligned
keep all_writes.count_of(.burst == FIXED) <= 8        // bound on FIXED bursts
```

The constraint solver lowers quantifiers over bounded list types into expanded constraint sets at elaboration. Unbounded lists are not constrainable; the compiler errors with a pointer to the unbounded type.

---

## 5. Properties — One Syntax, Three Roles

Temporal property syntax mirrors SVA but tightened:

```
clocking dut.axi_s.aclk

property aw_valid_stable
    dut.axi_s.aw.valid && !dut.axi_s.aw.ready |=>
        stable(dut.axi_s.aw.payload) && dut.axi_s.aw.valid
end property aw_valid_stable
```

Operators: `|->`, `|=>`, `##N`, `##[m:n]`, `[*N]`, `[*m:n]`, `throughout`, `within`, `intersect`, `and`, `or`, `not`, plus the temporal helpers `rose(e)`, `fell(e)`, `stable(e)`, and `past(e[, N])`.

The temporal helpers are spelled **without** the SVA `$` prefix — same as ARCH (which already supports `past(e, N)` etc.). This keeps source-level syntax consistent across the two languages. The SV+UVM transpiler (§10.2) lowers `past`/`rose`/`fell`/`stable` to their `$`-prefixed SVA equivalents at emission time; the source language (both ARCH and HARC) uses the bare names. The only `$`-prefixed identifier that survives at source level is `$clog2`, which is a compile-time function rather than a temporal helper and matches ARCH's spelling.

A property is used in one of three roles by attaching a verb:

```
assert aw_valid_stable          // DUT obligation
assume aw_valid_stable          // env constraint (formal)
cover  aw_valid_stable          // witness for sim/formal
```

### 5.1 Module contracts (compositional formal)

```
module AxiSlave
    contract
        assume    input_valid_after_reset
        guarantee response_within_n_cycles(N=16)
    end contract
    ...
end module AxiSlave
```

Contracts elaborate at module instantiation: the *consumer* gets `assume guarantee_*`; the module body proves `guarantee` under `assume`. This is the only way to make formal scale — built into the language, not bolted on as methodology.

### 5.2 Backends per role

| Role        | Native sim                    | SVA transpile     | BTOR2 / SMT export       |
|-------------|-------------------------------|-------------------|--------------------------|
| `assert`    | runtime check                 | `assert property` | proof obligation         |
| `assume`    | runtime check (warn-on-fail)  | `assume property` | constraint               |
| `cover`     | sim coverage                  | `cover property`  | witness query            |
| `contract`  | runtime (consumer assume, body assert) | bind + SVA | compositional decomposition |

---

## 6. Coverage

### 6.1 Type-derived

`enum`, ranged `uint<N>`, struct fields with `cov` modifier — all participate. No restating bin sets that already exist in the type definition.

### 6.2 Explicit cover groups

```
covergroup AxiOps @(posedge dut.axi_s.aclk)
    cp_burst : cover dut.axi_s.aw.payload.burst    // type-derived bins
    cp_len   : cover dut.axi_s.aw.payload.len
        bins
            single = {1}
            short  = {[2:8]}
            long   = {[9:256]}
        end bins
    cross cp_burst, cp_len
end covergroup AxiOps
```

### 6.3 Coverage as data

A coverage group is a typed value. You can:
- Merge groups across runs: `g1.merge(g2)`
- Query bin hit counts programmatically
- Export to UCDB, JSON, or HARC's native format

This unblocks ML-guided coverage closure (the April-13 article use case) without scraping vendor tools.

---

## 7. Execution Model

HARC replaces UVM phases with **scopes + events**, and replaces the SV process model with **cycle-aligned coroutines over a static schedule**. The latter is what makes the cycle-based simulator (§10.1) viable; the language semantics are designed around it, not bolted on.

### 7.1 Cycle-based foundation

There is no runtime event queue. Each clock domain has one statically scheduled C++ loop:

```
for each cycle of domain D:
    solver_dispatch(D)        // pull completed randomize() results into ready channels
    tb_step(D)                // resume coroutines, fire on-blocks, advance sequencers
    dut.eval_domain(D)        // DUT for domain D (ARCH-compiled or Verilator-compiled — see §10.5)
    sample_coverage(D)
    check_assertions(D)
```

`tb_step(D)` is a flat dispatch over coroutine resume points and `on`-handler triggers in domain D, all known at compile time. No dynamic process spawning, no event queue insertion, no NBA region.

This is the same execution shape as `arch sim`, extended with TB state. Co-compilation with the DUT means a single binary, single cache footprint, single optimizer pass.

### 7.2 Lifecycle scope

```
scope sim
    setup
        ...                     // build connections; runs once before clocks start
    end setup
    run
        ...                     // simulation main; clocks tick here
    end run
    check
        ...                     // post-run scoreboard / coverage queries
    end check
    teardown
        ...                     // close handles
    end teardown
end scope sim
```

`setup` / `run` / `check` / `teardown` are *blocks*, not virtual methods. No `super.build_phase()` ceremony, no objection counting, no end-of-test deadlock. `run` ends when its body completes (or when a `stop` is signalled); `check` runs after.

### 7.3 Events as cycle-aligned channels

```
event<AxiWrite> txn_observed                 // delivered next cycle (default)
event comb<AxiWrite> txn_observed_comb       // delivered same cycle (combinational)

on txn_observed(t)
    sb.expect(t)                              // reactive subscription
end on
emit txn_observed(t)                          // publish
```

Default delivery is **next-cycle** in the publishing domain — gives the compiler one cycle of slack to schedule subscribers, mirrors how RTL pipeline registers behave, and avoids combinational loops by construction. `event comb<T>` opts into same-cycle delivery; the compiler enforces that comb chains terminate (no `comb` cycles in the dependency graph).

`on` handlers become resume points in their domain's tb_step. Multiple subscribers fan out at compile time — no runtime dispatch.

#### `pre` / `post` hooks on declared methods

Borrowed from Specman e (constrained form). Components can declare hookable methods; subscribers attach `pre` and `post` blocks via `on`:

```
agent AxiAgent#(P)
    hookable send(t: AxiWrite)
        ...                                       // method declared as hookable
    end send
end agent AxiAgent

// elsewhere, possibly via extend (§3.6):
on agent.send pre
    log(info, "dispatching ${t}")
end on
on agent.send post
    stats.txn_count += 1
end on
```

`pre` fires before the method body, `post` after. Both run in the same cycle as the call. Hooks cannot replace the body — only observe and instrument. This is the controlled subset of e's AOP method-extension; it covers the legitimate use cases (instrumentation, debug, coverage glue) without the method-rewriting hazards (see §12).

The `hookable` marker is mandatory on the method declaration — only methods that opt in can be hooked. This makes the surface area for hooks visible at the type definition, not implicit.

### 7.4 Concurrency: fork/join over cycles

`fork / join_any / join_all` lowers to **cycle-aligned coroutines**. Each branch is compiled to a state machine indexed by resume label; `join_*` is a barrier checked once per cycle in tb_step.

```
fork
    branch
        drv.send(t1); drv.send(t2)        // sequence A
    end branch
    branch
        mon.expect(t1); mon.expect(t2)    // sequence B
    end branch
join_all
```

Compiles to one coroutine per branch with explicit suspend points at each cycle boundary (every implicit clock-edge wait inside `drv.send`, `mon.expect`, etc.). No OS thread, no fiber, no scheduler — a switch/case per coroutine.

`after N cycles ... end after` is the suspend primitive; wall-clock units (`100ns`) lower against the bound clock domain. `fork ... join_none` exists for fire-and-forget patterns and lowers to a coroutine without a join barrier.

**Loops.** Four loop forms cover the common cases:

- `for i in lo .. hi ... end for` — bounded count; `i` available in the body, or `_` when unused.
- `repeat <expr> ... end repeat` — fixed iteration count; no induction variable.
- `while <cond> ... end while` — pre-tested loop; the body re-checks `cond` on each iteration.
- `loop ... end loop` — infinite; exit via `break` (typically inside an `if`).

`break` exits the innermost enclosing loop; `continue` skips to its next iteration. Both are statements in their own right and lower to C++ `break` / `continue`, so they also work inside `for` / `repeat` / `loop` bodies.

```
let _w = 0
while !dut.ready && _w < 16
    wait 1 cycle
    _w = _w + 1
end while
if _w == 16
    fail("ready never asserted")
end if
```

The single-statement form `wait N cycles` is the common shorthand. Under multi-clock it advances by N rising edges of the **primary clock** (the first-declared clock in the test). To advance relative to a non-primary clock, use the optional `on <clock>` clause:

```
clock fast_clk = FastDomain      // 200 MHz, primary
clock slow_clk = SlowDomain      //  50 MHz

wait 1 cycle                     // 1 fast_clk rising edge
wait 2 cycles on slow_clk        // 2 slow_clk rising edges; fast_clk
                                 // continues to tick at its natural rate
                                 // (8 fast edges elapse in this span).
```

Cycle counts in `on <clock>` are real-time–correct: every other clock keeps ticking at its declared frequency. Use this form when an assertion is naturally phrased in the destination domain ("after 2 dst cycles, X holds"); use the bare form when reasoning is in the primary domain or when there's only one clock.
### 7.5 Multi-clock domain spanning

Each `on` block has a primary clock domain inferred from its trigger. Reads of signals or events from another domain are allowed but **not silent**: the compiler synthesizes a synchronizer (default 2-FF) and the read evaluates to a typed value with explicit latency.

```
clocking dut.fast_clk

on dut.fast_signal
    let v = across dut.slow_regfile.x        // 2-FF synced into fast domain
    assert v == expected
end on
```

The `across` keyword is required for cross-domain reads — implicit cross-domain access is a compile error. This forces the user to acknowledge metastability latency at the read site, the same discipline ARCH already enforces in design.

For TB cross-domain *writes*, the only mechanism is a typed channel:

```
event<Cmd> fast_to_slow across (fast_clk -> slow_clk) depth=4

// in fast-domain on-block:
emit fast_to_slow(cmd)

// in slow-domain on-block:
on fast_to_slow(cmd)
    ...                                       // fires in slow domain; FIFO underneath
end on
```

Cross-domain channels are async-FIFO-typed; the compiler sizes the FIFO from the `depth` annotation and synthesizes the gray-code pointer crossing. Properties spanning domains (e.g., "request in fast → response in slow within N slow cycles") are translated by the compiler against the synced view, with a documented latency budget added to the bound.

### 7.6 No global config DB

Connection topology is declared statically inside `env`. `uvm_config_db` does not exist. If a TB component needs a parameter, it's a generic parameter on the component.

### 7.7 Logging

`log` is an ARCH primitive HARC layers verification semantics on top of. The base ARCH primitive is `log("text ${expr}")` — printf-style with string interpolation, written to stderr by default, lowers to SV `$display` on transpile. HARC adds severity, verbosity, and component IDs while keeping a single call shape.

**Surface syntax (severity-first, SV-style):**

```
log(info,  "dispatching ${t}")                       // routine progress
log(warn,  "scoreboard depth ${sb.depth} > 1024")    // notable but not failing
log(error, "mismatch: expected ${t_exp} got ${t_obs}")  // counted toward test result
log(fatal, "configuration invalid")                   // aborts this test instance
log(debug, "handshake completed in ${cycles} cycles") // hidden by default verbosity
```

**Severity is an enum**, not a keyword set. The variants (`debug`, `info`, `warn`, `error`, `fatal`) are values of type `Severity`, in scope wherever `log` is callable:

```
enum Severity { debug, info, warn, error, fatal }
```

This means severity composes — you can pass it through generics, store it in a config struct, or thread it through a wrapper function. A `log_per_severity(s: Severity, msg: String) ... log(s, msg) ... end function` helper is just a normal function.

**Optional named arguments** for verbosity and component ID:

```
log(info, "details", verbosity=HIGH)         // override default verbosity
log(info, "...", id="AXI_DRV")               // override implicit component ID
log(info, "...", id="AXI_DRV", verbosity=HIGH)
```

**Default verbosity per severity:**

| Severity | Default verbosity | Prints by default? |
|---|---|---|
| `fatal` | LOW | always |
| `error` | LOW | always |
| `warn` | MEDIUM | yes |
| `info` | MEDIUM | yes |
| `debug` | HIGH | no — must opt in via `--verbosity HIGH` or higher |

The runtime exposes verbosity as a flag (`harc sim --verbosity HIGH`) and per-component overrides (`--verbosity-of env.agent.driver=DEBUG`). Verbosity levels are LOW / MEDIUM / HIGH / DEBUG / FULL — only messages whose verbosity ≤ the runtime threshold print. `error` and `fatal` always print regardless of threshold (they're test-result-bearing).

**Component IDs are implicit** from the enclosing TB component context. A `log(info, ...)` call inside `env.agent.driver` gets `id="env.agent.driver"` automatically. The explicit `id=` override is for cases where the call is in a free function or shared utility.

**Behavioral semantics by severity:**

| Severity | Test result | Simulation behavior |
|---|---|---|
| `debug` | no effect | suppressed by default; informational only |
| `info` | no effect | informational |
| `warn` | no effect | logged, no further action |
| `error` | failure counter incremented | logged; test fails at end of run if any error logged |
| `fatal` | failure | logged; this test instance aborts immediately at end of current cycle. Other instances in the same regression continue. |

This means `log(error, ...)` is the test-failure signal that pairs with `assert` — they share the same runtime path and the same end-of-run failure reporting. `log(fatal, ...)` is the equivalent of an unrecoverable test condition — it terminates *this test instance*, not the whole simulation runtime. On a CPU regression of 10K seeds this means one bad seed doesn't kill the other 9,999; on a SIMD or GPU batch (§10.1, Phase 7), one fatal lane retires from the grid while sibling lanes continue. Per-instance fatal is the right semantic at every scale.

**Output format:**

```
[ 1247 ns | tb_clk:412 | env.agent.driver | INFO ] dispatching AxiTxn { addr=0x..., len=8, ... }
```

Timestamp + cycle in the relevant clock domain + component ID + severity + interpolated message. Structured for grep / awk consumption; a `--log-format json` flag emits one JSON object per line for tooling.

**Determinism.** `log` calls fire at thread cycle boundaries (when emitted from `tseq` / `thread` lowering) or at handler dispatch order within a cycle (when emitted from `on` blocks). Output ordering is fully deterministic per seed.

**Component-scoped instance**, when needed:

```
component AxiDriver
    let log = Logger("AXI_DRV")             // pre-bind ID to this scope
    on req(t)
        log.info("dispatching ${t}")        // method-style; equivalent to log(info, "...", id="AXI_DRV")
    end on
end component AxiDriver
```

`Logger(id)` returns a value whose `.info(msg)` / `.warn(msg)` / etc. methods are sugar for `log(severity, msg, id=id)`. Use when the same component logs frequently and the component-name boilerplate matters.

**Lowering — see §16** for the full ARCH and SV+UVM mapping.

---

## 8. Testbench Architecture — Native Constructs

This is where the "wide" scope decision pays off. Each UVM role becomes a language construct with a typed contract.

**v0 lowering status.** The component constructs below all parse and round-trip through `harc fmt`. The cpp_tb backend lowers a useful subset:

- `tseq T -> TSeq<X>` → `[&]`-lambda returning `std::vector<X>`. `yield e` pushes to the implicit `_result` accumulator. Iterate the result with `for x in seq`.
- `driver` / `agent` / `env` / `sequencer` → plain C++ struct of fields. DUT-typed fields lower to Verilator pointers (`V<Name>*`); sub-component fields are by-value structs.
- `hookable name(args) -> T ... end name` on any of the above → free `[&]`-capturing lambda named `<Type>_<name>`. Inside the body, bare references to component fields rewrite to `self.<field>`. `dut.<port>` keeps the arrow-access form.
- `obj.method(args)` and `env.sub.method(args)` rewrite to `<Type>_<method>(<self>, args)` (the call-site dispatcher resolves up to two levels of field-access chain).
- `let drv : MyDriver` default-constructs the struct; the user assigns DUT pointers and other field values explicitly afterward (`drv.dut = dut`).
- `on event_field(arg) ... end on` inside a driver / agent / sequencer body → registers a `[&]`-capturing closure into the corresponding event vector at `let drv : T` time. Event payloads typed `event<MyTxn>` round-trip as the `MyTxn` C++ struct (transactions and enums get their bare name; integer-typed payloads still widen). `emit drv.req(t)` fires every registered subscriber synchronously — the on-handler body runs inside the test's tick scope (so `wait`, `dut.x = ...`, etc. all work).
- `on dut.signal ... end on` inside a monitor body → per-cycle bool checker (existing behavior, unchanged).
- `connect a -> b ... end connect` inside an `env` body → at `let env : E` time, installs a generic-lambda bridge subscriber on `<env>.<a>` that fans out to every subscriber of `<env>.<b>`. Lets a sequencer's `out event` drive a driver's `in event` without the test scope manually re-emitting. Edge endpoints are field-access chains (`sub.event_name`); the bridge uses `auto` for the payload so the connect site doesn't have to look up the event's type.
- `TSeq<T>` as a hookable parameter type → `const std::vector<T>&` (pass-by-reference, so iterating a tseq result inside a sequencer's `dispatch` method doesn't copy each transaction).
- `on obj.method pre/post ... end on` (or `on env.sub.method pre/post`) → registers a `[&]`-capturing closure into a per-`(Type, method)` hook vector. Each hookable method's body is wrapped with `for (auto& _h : <Type>_<method>_<side>) _h(args);` before/after the body. Pre and post hooks see the same arg list as the method; both can read and mutate test-scope locals via the lambda capture (e.g. counters, scoreboards). Hooks cannot replace the body — only observe and instrument.
- `bus Name { ... } end bus Name` (mirrors arch-com §19) → protocol-typed bundle of DUT signals. v0 surface: plain signals (`name: in|out Type`) and `handshake_channel ch: send|receive kind: valid_ready { payload signals } end handshake_channel ch`. `param`, `credit_channel`, and `tlm_method` blocks parse but don't yet contribute to typed access — those follow.
- `let var : BusName = bind <dut-expr>` → bus binding. `var` is a virtual binding (no C++ instance is emitted); subsequent `var.signal` and `var.channel.signal` accesses lower to flat DUT-pointer paths matching arch's port-flattening convention: `<dut>-><var>_<signal>` and `<dut>-><var>_<channel>_<signal>`. Unknown signal/channel names produce a clear HARC-level error before C++ codegen.
- `use BusName;` (or `use foo.bar.BusName;`) → extern import. `harc sim` walks search paths (`$HARC_LIB_PATH` colon-separated; then `<input>/stdlib/`, `./stdlib/`, `<input>/../arch-com/stdlib/`, `<input>/../arch-com/examples/`) for `<BusName>.arch` (or `.harc`) and parses any `bus` items it contains. Unresolved imports silently no-op — the same `use arc.stdlib.X` lines already in pre-bus-typing fixtures keep parsing without behavioral change.
- `driver Foo bound to BusType` (and the matching `agent` / `monitor` form) declares the component's protocol-typed binding. Instantiation pairs with `let drv : Foo = bind <bus_binding>` where `<bus_binding>` is a previously-declared `let X : BusType = bind dut`-style variable. The `bind` clause is type-checked at codegen: passing a `BusBar` binding to a driver `bound to BusFoo` produces a clear HARC-level error. Inside the driver's `on T t` handlers, the bare identifier `bus` resolves to the bound binding so `bus.<ch>.send(t.addr, …)` and `bus.<ch>.<sig>` lower through the same paths as test-scope bus access — flat names use the original binding's prefix, not `"bus"` (e.g. `dut->axil_aw_addr`, not `dut->bus_aw_addr`).
- A `bound to BusType` driver/agent with a single `in event<T>` field plus a matching `on <event_name>(t)` handler additionally lowers as an **independent coroutine actor**: the driver gets its own `harc_rt::ThreadSlot` registered with the test's scheduler plus a per-instance `std::deque<T>` transaction queue. The actor coroutine loops `co_await wait_until(!queue.empty())` → pop t → run the on-handler body in coroutine context (so internal `wait N cycles` and `bus.<ch>.send/recv` lower to `co_await`) → repeat. `emit drv.req(t)` from the run coroutine just enqueues the transaction (non-blocking); the driver coroutine processes it in parallel with the run coroutine. The main loop terminates when the *run* coroutine finishes — driver coroutines parked in `WaitUntil { queue.empty() }` are abandoned at process exit (intentional: the test is over). Drivers without `bound to`, drivers with no input event, or drivers with multiple matching handlers fall back to the synchronous subscriber-callback model (existing fixtures unchanged).
- A `bound to BusType` monitor with `on bus.<ch>.handshake(arg) ... end on` handlers lowers each handler as a **per-channel coroutine actor**: own `ThreadSlot`, registered with the scheduler, and a coroutine that loops `co_await wait_until(<chan>_valid && <chan>_ready)` → captures the channel's first payload signal into `arg` → runs the body in coroutine context → `co_await wait_cycles(1)` to skip past this handshake before re-arming. Multiple handlers on different channels become independent actors that run concurrently. Non-handshake handlers in the same monitor (event subscribers, cycle triggers on bool expressions) fall through to the existing sync `_checkers`-based path.
- `bus.<ch>.send(p1, …, pN)` → auto valid/ready handshake. Lowers to: drive each payload signal from the matching positional arg, raise `valid`, spin on `ready` (bounded budget of 16 cycles, each cycle = `co_await harc_rt::wait_cycles(_slot, 1)` in run-coroutine context, plain `tick()` in sync method/handler context), final cycle wait, drop `valid`. Arg arity must match the channel's payload signal count; mismatch is a clear HARC-level error.
- `let v = bus.<ch>.recv()` (or bare `bus.<ch>.recv()`) → auto valid/ready handshake. Lowers to: raise `ready`, spin on `valid` (16-cycle budget, same coroutine/sync split as send), capture the first payload signal into `v` (when used as a let-rhs), final cycle wait, drop `ready`. Multi-payload channels still expose every signal via the manual `bus.<ch>.<sig>` path; `recv()` returning only the head signal is a v0 simplification, not a permanent constraint.

**Coroutine runtime (Phase 1, single-actor).** The test's `run` block lowers to a C++20 coroutine driven by `harc_rt::ThreadScheduler` (slim sister of arch-com's `arch_thread_rt.h`). `wait N cycles` and the bus.send/recv spin loops emit `co_await harc_rt::wait_cycles(_slot, N)`; the main loop drives one primary-clock posedge per iteration, calls `_checkers`, then resumes any coroutine whose wait condition is satisfied. Hookable methods, `on`-event-handler closures, tseq lambdas, and free functions stay synchronous — they only execute while the run coroutine is "running" between `co_await`s, so a sync `tick()` from inside a method does not race the scheduler. Multi-clock `wait N cycles on <named-clock>` keeps its sync `eval_clocks_until` path even in coroutine context: the main loop's full-primary-period granularity is too coarse for sub-primary-cycle waits when the named clock runs faster than primary. **Phase 2** (driver/agent/monitor as independent coroutines, `bound to BusType` codegen) and **Phase 3** (multi-thread scheduler for performance scaling) sit on top of the same runtime — surface stays the same, scheduler gets richer.

Out of v0 scope: `tlm_method` lowering, structured multi-payload returns from `recv()` and from `bus.<ch>.handshake(arg)` (the captured `arg` is currently the first payload signal — multi-payload structs follow), DUT-side introspection to flag bus signals that the actual SV doesn't expose, env-composed `bound` sub-components (only top-level `let drv/mon : T = bind axil` is supported; bound components nested inside an `env` follow), and OS-thread parallelism (Phase 3).

### 8.1 `agent`

```
agent AxiAgent#(P: AxiParams) bound to AxiBus#(P)
    driver    : AxiDriver#(P)
    monitor   : AxiMonitor#(P)
    sequencer : Sequencer<AxiWrite>

    connect
        sequencer.req -> driver.req
    end connect
end agent AxiAgent
```

`bound to T` ties the agent to a protocol-typed interface. The agent cannot be instantiated without a matching interface — checked at elaboration. Inside the agent body, `bus` refers to the bound interface.

### 8.2 `driver`

```
driver AxiDriver#(P: AxiParams) bound to AxiBus#(P)
    req: in event<AxiWrite>

    on req(t)
        // protocol contract supplies handshake sequencing automatically
        bus.aw.send(t.addr, t.len, t.burst, t.id)
        for beat in 0 .. t.len
            bus.w.send(t.data[beat], t.strb[beat], beat == t.len - 1)
        end for
        let resp = bus.b.recv()
        assert resp.id == t.id
    end on
end driver AxiDriver
```

`bus.aw.send(...)` is *derived* from the protocol type's handshake spec — the driver does not hand-code the valid/ready dance. This is the same skip-the-middle-layer move ARCH already makes for `arch formal`.

### 8.3 `monitor`

```
monitor AxiMonitor#(P: AxiParams) bound to AxiBus#(P)
    txn: out event<AxiWrite>

    on bus.aw.handshake(aw)
        let t = AxiWrite { addr: aw.addr, len: aw.len, burst: aw.burst, id: aw.id, ... }
        for beat in 0 .. aw.len
            let w = bus.w.handshake.next()
            t.data[beat] = w.data
            t.strb[beat] = w.strb
        end for
        emit txn(t)
    end on
end monitor AxiMonitor
```

Monitors are passive *by type* — `bound to T` does not include any output-driving permission. The compiler rejects a monitor that tries to drive.

### 8.4 `sequencer` and `tseq`

```
tseq RandomWrites(n: int) -> TSeq<AxiWrite>
    for _ in 0 .. n
        let t: AxiWrite
        randomize(t) with AxiBurstLegal(t)
        yield t
    end for
end tseq RandomWrites

tseq BackToBackBursts -> TSeq<AxiWrite>
    for burst in [FIXED, INCR, WRAP]
        let t: AxiWrite
        randomize(t) with
            t.burst == burst
            t.len == 16
        end randomize
        yield t
    end for
end tseq BackToBackBursts
```

`tseq` is a generator. Test sequences compose:

```
tseq Mixed = RandomWrites(100) >> BackToBackBursts >> RandomWrites(100)
```

The sequencer is a generic component; users do not normally subclass one.

### 8.5 `scoreboard`

```
scoreboard AxiSb
    expected: queue<AxiWrite>

    on env.agent.sequencer.dispatched(t)
        expected.push(t)
    end on
    on env.agent.monitor.txn(t_obs)
        let t_exp = expected.pop()
        assert t_obs == t_exp
            else fail("mismatch: expected ${t_exp} got ${t_obs}")
    end on
end scoreboard AxiSb
```

Equality on transactions is structural and free; `==` does the right thing without `do_compare` boilerplate.

### 8.6 `env`

```
env AxiTbEnv#(P: AxiParams)
    agent : AxiAgent#(P)
    sb    : AxiSb
    cov   : AxiOps

    connect
        agent.monitor.txn -> sb.observed
    end connect
end env AxiTbEnv
```

`env` is the static composition root. No factory, no `uvm_config_db.set(this, "*", "agent", ...)`.

---

## 9. Reference Models and Co-simulation

ARCH already supports C function bodies behind fixed-latency pipes (per April-15). HARC inherits this for reference models:

```
ref module AxiRefMem#(SIZE: int)
    in  cmd  : AxiWrite
    out resp : AxiResp
    body c
        // C function — receives typed AxiWrite, returns AxiResp
    end body
end ref module AxiRefMem
```

A `ref module` is a module whose body is functional (C function or pure ARCH). The scoreboard compares DUT output against `ref` output without DPI ceremony — the typed channel does the marshaling.

ISA-spec embedding: a Sail model compiles to a `ref module` via the C-emulator path (per April-21 thread), giving spec-driven reference models for free.

---

## 10. Backends

### 10.1 Native simulator — `harc sim`

**Cycle-based, statically scheduled, co-compiled with ARCH.** No event-driven kernel.

Compilation produces a single C++ binary linking:
- ARCH design → C++ via the existing ARCH backend (Verilator-class)
- HARC testbench → C++ via the HARC backend; the test `run` block is a C++20 coroutine driven by a cooperative `harc_rt::ThreadScheduler` (sister to arch-com's `arch_thread_rt.h`). v0 ships single-actor (only `run` is a coroutine; methods and `on`-handlers stay synchronous between yields); v1 adds independent coroutines per driver/monitor, then OS-thread parallelism for performance scaling. Notably **not** lowered to FSMs — coroutine-direct simulation preserves source-level coverage legibility and keeps the door open for true multi-actor parallelism.
- Z3 / Bitwuzla — linked as the off-cycle solver pool serving queued `randomize` requests (§4.4)
- Coverage / wave runtime — emits UCDB / FSDB / VCD via standard formats

Per-cycle dispatch shape (one per clock domain, see §7.1):

```
solver_dispatch(D) → tb_step(D) → dut.eval(D) → sample_coverage(D) → check_assertions(D)
```

Multi-clock simulations run one such loop per domain, advanced by the global cycle scheduler in lockstep with their period; cross-domain channels (§7.5) decouple the domains' tb_step ordering.

**Performance targets, v1:**
- Elaboration: < 5 s for a 100k-line TB+DUT
- Throughput: within 2× of pure-Verilator DUT-only simulation for a fully-loaded TB
- Solver pool: queued `randomize` should never be the bottleneck at default queue depth (16) for typical CRV workloads; tunable per `tseq`

**v1.1+ — SIMD batch CRV (Phase 7a, then 7b):**

Because the TB is statically scheduled cycle-based C++, the same SIMD-pack-N-stimuli backend that ARCH targets for `arch sim --batch N` applies to constrained-random regression. The progression is two phases:

- **Phase 7a (CPU SIMD).** The TB compiler emits per-lane RNG state and per-lane transaction registers in `__m512i` (AVX-512) or SVE equivalents; `dut.eval(D)` runs all N lanes per cycle. A 10K-seed nightly becomes 156 batches of 64 seeds with N-wide SIMD on top of the cycle-based base speedup. Lane-divergence handling on `blocking randomize` is the load-bearing design point — `when`-subtype lane grouping at elaboration (per-variant kernel emission, lane regrouping mid-simulation) handles the common divergence cases.
- **Phase 7b (GPU).** Same architecture, CUDA kernels, 10K+ lanes per grid. Per-cycle `tb_step_kernel<<<grid, block>>>()` per domain. Per-lane `state<T>` / `buffer<T>` lives in device memory; cross-lane communication is forbidden by construction (each lane is an independent test). Solver pool stays on host with pinned-memory channels to GPU — Z3 doesn't go to GPU, and queued `randomize` (§4.4) is already off the cycle path so the architecture aligns. Coverage merge is a reduction kernel; assertion checks aggregate into a host-visible per-lane failure mask. `blocking randomize` is strongly discouraged on GPU (host-device round-trip per call kills throughput). Per-test-instance `fatal` (§7.7) means one bad seed retires from the grid while siblings continue — the right semantic at every scale, and the reason the §7.7 fatal definition is written that way from v1.

GPU is a **width parameter, not a separate backend** — same statically scheduled cycle loop, same per-lane state isolation, same solver hand-off, lane count goes from 1 (Phase 1a) to 64 (Phase 7a) to 10K+ (Phase 7b). The discipline of Phase 7a validates the architecture before paying GPU debugging cost.

**What this gives up vs. event-driven kernels:**
- Sub-cycle timing (no `#10`-style arbitrary delays in TB; all timing is clock-relative)
- Dynamic process spawning at sim-time (replaced by static coroutine compilation)
- A handful of SVA operators that span unclocked time (`#-#`, `#=#` — unsupported on native; available on SV+UVM transpile target)

These are deliberate. None are needed for pure-RTL TB; all of them are footguns that event-driven kernels accommodate at large perf cost.

### 10.2 SV+UVM transpile — `harc -emit sv-uvm`

Lossy in known places, all documented:

| HARC                 | SV+UVM emission                                  |
|-----------------------|--------------------------------------------------|
| `agent`/`env`/etc.    | UVM class hierarchy with factory boilerplate     |
| `transaction` (flat)  | `uvm_sequence_item` + field automation           |
| `transaction` (with `when`) | discriminator + tagged union; constraints emitted as `if (disc == ...)` blocks |
| `relation`            | `constraint` blocks                              |
| `property/assert/...` | SVA                                              |
| `event<T>` (default, next-cycle) | `uvm_analysis_port` + 1-cycle pipeline reg |
| `event comb<T>`       | `uvm_analysis_port` (immediate)                  |
| `scope`               | phase mapping (setup→build, run→run, check→check, teardown→final) |
| `fork/join_*`         | SV `fork/join_*` (event scheduler does the work) |
| `randomize` (queued)  | SV `randomize()` — queue is dropped; per-call blocking on host sim |
| `blocking randomize`  | SV `randomize()` — same target, just no semantic change to flag |
| `across` cross-domain read | explicit 2-FF synchronizer module + `bind` |
| Cross-domain channel  | async-FIFO module + `bind`                       |
| `ref module` (C body) | DPI-C import                                     |
| Module `contract`     | `bind` + SVA (solver behavior tool-dependent)    |

What does **not** survive the transpile cleanly:
- Native cycle-based execution speed — transpile output runs at vendor-sim throughput, not Verilator-class
- Pre-computed `randomize` queues — collapse to per-call blocking on the host sim's randomize implementation
- Coverage-as-data programmatic queries (only what UCDB exposes)
- Compositional formal decomposition (best-effort only — formal-tool-dependent)
- SIMD batch CRV (v1.1) — single-lane only on transpile target

### 10.3 Formal exporter — `harc -emit btor2` / `-emit smt2`
- Properties + module contracts + ARCH design → BTOR2 (for AVR / Pono / Avy) or SMT-LIB2 (for direct Z3)
- Same skip-the-middle-layer pattern `arch formal` already uses
- `when` subtypes export directly to SMT-LIB2 algebraic datatypes (`(declare-datatypes ...)`); per-variant constraint subproblems map 1:1 to the solver's native theory
- v1: BMC and k-induction; PDR/IC3 via Pono backend

### 10.4 Emulation
v1: assertion-synthesizable subset (no temporal-unbounded operators; bounded `[*]` only). Emit synthesizable RTL checkers binding to DUT signals. Full TB on emulation deferred to v2.

### 10.5 DUT backends

The HARC TB compiler is backend-agnostic at the cycle-loop level — `tb_step(D)` and `dut.eval_domain(D)` are an interface, not a specific implementation. Two DUT backends ship in v1:

**ARCH-compiled DUT (default, fastest path).** ARCH source compiles to C++ via the existing ARCH backend (Verilator-class). HARC and ARCH share the same IR, the same compiler invocation, and the same C++ output object — single binary, single cache footprint, single optimizer pass. Typed cross-references (`dut.axi_s.aw.payload`) resolve directly against ARCH IR. This is the only path that gives co-elaboration (HARC TB and ARCH design parameters elaborate in the same pass) and protocol-typed interface binding (HARC drivers bind to ARCH `bus` declarations and dispatch through `handshake_channel` / `credit_channel` / `tlm_method`).

**Verilator-compiled SV DUT (interop path).** Existing SystemVerilog DUTs are linked through Verilator's standard C++ output. The HARC compiler:

1. Invokes `verilator --xml-only` on the SV source to extract a structured port and parameter description.
2. Generates C++ glue mapping HARC's typed signal access to Verilator's `Vmodel` accessor methods.
3. Links the generated `Vmodel.cpp` + glue + HARC TB into the same single binary the ARCH backend produces.

Per-cycle, `dut.eval_domain(D)` calls `Vmodel->eval()`; signal reads and writes go through Verilator's port accessors. Throughput is whatever Verilator delivers for the DUT (typically Verilator-class regardless of TB language).

Surface syntax for binding (parallels ARCH's `module Foo kind <variant>` pattern — see ARCH §11 for `ram kind sram`, ARCH §16 for `fifo kind lifo`, etc.):

```
module my_axi_slave kind verilator
    src: "rtl/axi_slave.sv"
    top: my_axi_slave_top
    param ADDR_W: int = 32
    param DATA_W: int = 64
    clocks: { aclk: tb_clk, ... }
    resets: { aresetn: tb_rst_n }
end module my_axi_slave

let dut: my_axi_slave = bind ...
on dut.s_axi_awvalid && dut.s_axi_awready
    let addr = dut.s_axi_awaddr        // raw signal read
    dut.s_axi_awready <- 1             // raw signal write
end on
```

The implicit default is `kind arch` — an ordinary `module Foo ... end module Foo` declaration with HARC/ARCH source body uses the ARCH backend. `kind verilator` selects the Verilator compilation backend; v1.1+ adds `kind vcs`, `kind xcelium`, `kind ghdl` along the same pattern.

**v1 limitations of the Verilator path:**

- **Raw signal access only.** No automatic protocol grouping in v1 — `dut.s_axi_awvalid` is a raw signal, not part of a typed `bus BusAxi4`. This means HARC drivers/monitors that are written against ARCH `bus` types cannot be reused directly against SV DUTs without adapter code. v1.1 will add convention-based grouping (`<prefix>_<channel>_<signal>` patterns) and explicit binding stubs for protocol-typed access.
- **No `internal` access** to SV module internals beyond what Verilator's public accessors expose. Verilator can be coerced into exposing more via `/* verilator public */` annotations, but HARC v1 doesn't depend on this.
- **No co-elaboration.** SV parameters are baked at Verilator compile time; HARC parameters can't be propagated into the SV DUT. Mixed-parameter designs need the ARCH-DUT path.
- **No SVA on internal SV signals.** HARC `assert` / `cover` / `assume` work fine on the DUT boundary signals; reaching internal SV signals for property checking requires Verilator hierarchical access (currently limited).

**Why ship Verilator support in v1.** The ARCH-only path gates HARC adoption on ARCH adoption. Verilator-linked SV DUT support means existing SV codebases can be driven, observed, scoreboarded, and asserted on by HARC TBs without an HDL migration — the realistic adoption path. ARCH remains the primary, fastest, most expressive path; Verilator is the on-ramp.

**v1.1+ DUT backends:**
- **Commercial-simulator co-sim (VCS / Xcelium / Questa).** HARC TB process talks to the vendor sim through DPI-C (HARC TB compiled as a shared library that the vendor sim loads). Slower than co-compiled Verilator (one cycle = one DPI roundtrip) but covers proprietary HDL flows and unmodifiable encrypted IP.
- **VHDL DUTs.** Via GHDL co-sim or Verilator's experimental VHDL frontend. Same DUT abstraction layer; just a different eval shim.
- **Protocol-typed grouping for raw SV signals.** Convention-based default (`<prefix>_<channel>_<signal>` auto-groups into protocol types) with explicit binding stubs as override. Lets HARC drivers/monitors written against `bus BusAxi4` work against SV DUTs without adapter code.

The DUT backend abstraction makes all three v1.1+ paths straightforward additions, not architectural rewrites.

---

## 11. Worked Example: AXI Read+Write Agent

End-to-end. ~80 lines of HARC replaces ~700 lines of UVM. Uses ARCH stdlib `BusAxi4` (per ARCH §18e) — driver and monitor bind to the bus and dispatch through its handshake channels.

```
use arc.stdlib.BusAxi4         // ARCH stdlib bus definition
use arc.dut.AxiSlave           // ARCH design module under test

// --- Transaction with when subtypes (READ vs WRITE share addr/len/id; data only on WRITE)
transaction AxiTxn
    op    : AxiOp
    addr  : uint<64>
    len   : uint<8>
    size  : uint<3>
    burst : BurstType
    id    : uint<4>

    when op == WRITE
        data : Vec<bits<64>, 256>
        strb : Vec<bits<8>, 256>
    end when
    when op == READ
        expected_data : Vec<bits<64>, 256>      // for scoreboard comparison
    end when

    keep op in {READ, WRITE}                    // discriminator value space
    keep len in [1..256]
    keep burst != WRAP || (len in {2, 4, 8, 16})
    keep addr % (1 << size) == 0
    keep len * (1 << size) <= 4096 - (addr % 4096)
end transaction AxiTxn

// --- Test sequence (every field random by default; constraint inherited from AxiTxn)
tseq RandomTxns(n: int) -> TSeq<AxiTxn>
    for _ in 0 .. n
        let t: AxiTxn
        randomize(t)                    // queued; off-cycle solver
        yield t
    end for
end tseq RandomTxns

// --- Test-specific aspect: tighten constraints for short-burst regression
package ShortBursts
    extend AxiTxn
        keep len < 16
        keep burst == INCR
    end extend AxiTxn
end package ShortBursts

// --- Driver, Monitor, Sequencer (per §8) reused as-is.
// Driver lowers to an ARCH `thread` that drives the bus's handshake_channel methods;
// Monitor lowers to a passive `thread` that observes them.

// --- Scoreboard
scoreboard AxiSb
    expected: queue<AxiTxn>
    on env.agent.sequencer.dispatched(t)
        expected.push(t)
    end on
    on env.agent.monitor.txn(t_obs)
        let t_exp = expected.pop()
        assert t_obs == t_exp else fail("mismatch")
    end on
end scoreboard AxiSb

// --- Coverage
covergroup AxiOps @(posedge dut.s_axi.aclk)
    cp_op    : cover dut.s_axi.aw.payload.op       // type-derived bins
    cp_burst : cover dut.s_axi.aw.payload.burst
    cp_len   : cover dut.s_axi.aw.payload.len
        bins
            single = {1}
            short  = {[2:8]}
            long   = {[9:256]}
        end bins
    cross cp_op, cp_burst, cp_len
end covergroup AxiOps

// --- Env (static composition root)
env AxiTbEnv
    agent : AxiAgent bound to BusAxi4<ADDR_W=32, DATA_W=64, ID_W=4>
    sb    : AxiSb
    cov   : AxiOps
end env AxiTbEnv

// --- Test (full mix — no aspects applied)
test SmokeTest
    let dut: AxiSlave#(AW=32, DW=64, IDW=4)
    let env: AxiTbEnv = bind dut.s_axi

    scope sim
        run
            env.agent.sequencer.run(RandomTxns(1000))
        end run
        check
            assert env.cov.cp_op.coverage > 95.0
            assert env.sb.errors == 0
        end check
    end scope sim
end test SmokeTest

// --- Test (short-burst regression — uses + applies the ShortBursts aspect)
use tests.aspects.short_bursts        // makes the package visible

test ShortBurstSmoke
    apply ShortBursts                 // activates the extend in this scope only

    let dut: AxiSlave#(AW=32, DW=64, IDW=4)
    let env: AxiTbEnv = bind dut.s_axi

    scope sim
        run
            env.agent.sequencer.run(RandomTxns(500))   // ShortBursts constraints active
        end run
        check
            assert env.sb.errors == 0
        end check
    end scope sim
end test ShortBurstSmoke
```

This compiles to:
- A native simulation: `harc sim smoke.harc` — ~3 s startup, runs to completion. The HARC compiler co-elaborates with ARCH; the resulting binary links the cycle-based ARCH-compiled DUT with HARC-compiled testbench coroutines into a single C++ executable.
- A UVM testbench: `harc -emit sv-uvm smoke.harc` — ~600 lines of UVM that drops into a Xcelium / VCS / Questa flow.
- A formal proof artifact: `harc -emit btor2 smoke.harc` — for the property subset.

---

## 12. Explicitly Rejected

Carrying forward the April-13 list, with rationale specific to the sister-language framing:

- **Class hierarchies for transactions.** ADTs with structural equality, `when` subtypes (§3.3), and `extend` aspects (§3.6) cover every `extends` use case more cleanly.
- **`uvm_config_db`.** Static composition + generic parameters cover every legitimate use; the rest were UVM workarounds for SV's missing module-system features ARCH already has.
- **Virtual interfaces.** `bound to ProtocolType` with typed cross-module references replaces the entire `vif` indirection.
- **Phase macros (`build_phase`, `connect_phase`, etc.).** Replaced by `scope sim` with `setup` / `run` / `check` / `teardown` blocks.
- **Factory registration.** Generic parameters and explicit instantiation. No `uvm_object_utils`.
- **Field automation (`uvm_field_*`).** ADT-derived deep equality, pack/unpack, and pretty-printing.
- **TLM as a separate type hierarchy.** TLM is `event<T>` and `TSeq<T>` over typed values.
- **Macros for TB structure.** Macros are framework cope; first-class constructs make them unnecessary.
- **Two-state vs four-state ambiguity.** ARCH is two-state; X-propagation is a backend concern (formal handles it; sim warnings are tooling, not language).
- **Implicit `super` chains.** No inheritance; composition is explicit.
- **Explicit `rand` keyword on every field.** Default-rand with `!` opt-out (§3.1) is the e-style cure.

**Rejected from Specman e** (despite the broader e study being highly productive):

- **Free-form AOP method-body extension.** Letting any code in any file replace any method body is the legitimate "where does this come from" critique of e. HARC allows `extend` of types and components (§3.6), and `pre`/`post` hooks on `hookable` methods (§7.3) — but never wholesale method body replacement. Composition stays auditable.
- **Globally-applied aspects via load order.** e applies extensions globally based on file load. HARC requires explicit `use` of the package and explicit `apply` at the test scope; the two-stage trail (`grep -n "use"` for visibility, `grep -n "apply"` for activation) is the audit trail.
- **Multi-level extension chains.** e allows `extend` of an `extend`, with arbitrary depth. HARC restricts `extend` to depth 1 — extensions always target base type declarations, never other extensions (§3.6). Composition across multiple extensions is realized at `apply` sites, not by chaining extension definitions. This eliminates the depth-N file-archaeology problem at the cost of zero expressivity (every chain rewrites as flat extensions + stacked `apply`s).
- **`like` inheritance.** ARCH's generics + composition cover every legitimate case; `like` is redundant.
- **e's syntax quirks.** `<' ... '>` file blocks, the apostrophe-typed-subtype syntax (`when WRITE'op base_u`), magic globals like `sys`. HARC uses ARCH's lexer; none of these come along.
- **Runtime-modifiable struct hierarchy.** e lets you `gen` and modify objects at runtime. HARC is statically scheduled (§7); this is the price of cycle-based perf and we're keeping it.

---

## 13. Open Issues for v1.1

Honest about what is not pinned down:

- **Coverage-in-formal vs coverage-in-sim.** Cover-as-witness in formal vs coverage-as-percentage in sim are different beasts; the surface syntax pretends otherwise. A sharper unification is needed.
- **Transaction recording for waves.** UCDB is fine for coverage; transaction-level wave annotation needs a format choice (FSDB has it natively; VCD doesn't).
- **Cross-domain read latency in property surface syntax.** §7.5 makes `across` reads explicit, but a property like `req |=> across resp ##[1:8] done` has a synthesized 2-FF latency that should be reflected in the bound. Implicit (good ergonomics, surprising) vs explicit (verbose, predictable) is unsettled.
- **Pre-computed `randomize` queues with constraint dependence on captured state.** When `with` references a `let`-bound snapshot of runtime state, the queue is still valid — but only until the snapshot is invalidated. The dataflow analysis is sound but the user-facing rule needs to be teachable in two sentences; not there yet.
- **Solver pool sizing.** Default queue depth (16) and pool worker count are both knobs. Auto-tuning from observed tseq consumption rate vs. per-call solver latency is the right answer; v1 ships with manual knobs.
- **Sub-cycle async events on native.** Rare in pure-RTL TB but real (e.g., gate-level back-annotation, async reset deassertion alignment). v1 punts; they only work via SV+UVM transpile target. v2 candidate: a small "async event" subset that cycle-aligns to the next active edge with bounded-skew assertions.
- **Lane divergence for SIMD batch CRV (v1.1).** `when`-subtype lane divergence has a clear path: group lanes by discriminator value, solve each group's variant subproblem independently, scatter results via AVX-512 lane masking. The remaining open question is `blocking randomize` divergence — when one lane stalls on a runtime-dependent solve and others don't, mask-and-stall is the natural answer but coverage attribution gets weird (does the stalled lane miss its sample window?). Needs prototyping.
- **Replay determinism.** Native runtime should be deterministic per seed; transpile target depends on vendor sim. Multi-domain ordering across domain boundaries is the subtle case.
- **Sail-import ergonomics.** The compile-to-`ref module` path works in principle; mapping Sail's effect tracking to ARCH's clock-domain types needs prototyping.
- **Aspect dependency declarations (`requires`).** v1's depth-1 rule (§3.6) handles cross-extension dependencies via runtime errors when one extension references fields added by another that isn't active in scope. This works but is an error-on-misuse story rather than a dependency-on-declaration story. v1.1 candidate: a `requires` clause on aspect packages (`package B { requires A; extend T { ... } }`) so `apply B` errors at compile time if A isn't already in the active set. Captures the dependency declaratively without reintroducing extension chains.
- **GPU batch CRV backend (Phase 7b — §14).** The cycle-based + statically scheduled + per-lane-isolated architecture makes GPU a *width parameter* rather than a separate backend (§10.1). Phase 7a CPU SIMD validates the lane-divergence handling and per-variant grouping at AVX-512 width before paying GPU debugging cost; 7b extends the same architecture to CUDA at 10K+ lanes. Scoped as v1.1+ explicitly because (a) it depends on Phase 7a landing first, and (b) the host-device hand-off design for queued randomize and per-lane coverage merge needs prototyping at small scale before committing the full kernel-dispatch design.
- **Emulation TB.** v2.

---

## 14. Implementation Phasing

Build order matches the data path of a working testbench: stimulus generates traffic, monitor observes it, checker verifies it. Properties and coverage are *additions* to a working TB, not the foundation — they're useful only once stimulus exists to exercise the DUT. Each phase delivers user-visible value standalone — no big-bang.

- **Phase 1a — Per-field stimulus, no constraint solver.** Transactions with default-rand fields and per-field attributes: `[range(...)]`, `[dist {...}]` (per-field weighted distribution), `[cyclic]`, `[unique]`, `[weighted(...)]`. `when` subtypes — discriminator-based variant selection plus per-field randomization within a variant. `tseq` with composition operators (`parallel`, `schedule`, `select`, `repeat` — §17.1), `sequencer`, `driver` bound to ARCH `bus` and dispatching through `handshake_channel` / `credit_channel` / `tlm_method`, `buffer<T>` flow object (§17.2), basic `test` and `scope sim` with `run` block, **logging with severity / verbosity / component IDs** (§7.7 — rides on the ARCH `log` primitive), **DUT backend abstraction with both ARCH co-compiled and Verilator-linked SV paths** (§10.5 — raw signal access on the SV path; protocol-typed binding deferred to v1.1). Static checker rejects any `keep` or `relation` referencing more than one field, with a clear error pointing to Phase 1b. **No SMT solver linked** — runtime is a standard PRNG library (xoshiro / PCG / Mersenne) with weighted-sample and cyclic-enumeration support. **Demo:** random valid AXI traffic drives a slave DUT through a HARC-compiled binary, against either an ARCH-native AXI slave or an existing SV AXI slave linked via Verilator; expressivity equivalent to SystemVerilog `$urandom_range` plus distributions and cyclic enumeration, with HARC's clean type system on top.

- **Phase 1b — Constraint solver, queued randomize, full CRV.** Z3 integration (linked as off-cycle solver pool — §4.4), cross-field `keep` constraints in transactions, free-standing `relation` declarations (§4), `solve_before` / `solve_after` hints, the `dist` directive inside `randomize ... with { ... }` for cross-field weighted distributions, queued `randomize` with implicit single-shot result channel, `blocking randomize` semantics with compile-time enforcement when constraint references runtime DUT state, tagged-ADT encoding of `when` subtypes for solver pruning (§3.3 — `(declare-datatypes)` per-variant subproblems). Phase 1a code keeps working unchanged — Phase 1b lifts the cross-field restriction on the static checker and enables the solver path. **Demo:** classic AXI burst-legal generation with relational constraints (`len * size <= 4096 - addr % 4096`); solver pool sustains throughput against cycle-based simulation.

- **Phase 2 — Monitor.** `monitor` bound to ARCH `bus` (passive — type system enforces no-driving), transaction reconstruction from observed bus signals, `agent` as the driver+monitor+sequencer composition. Multi-clock domain spanning (`across`, cross-domain channels — §7.5) lands here, lowering to ARCH `synchronizer` and async `fifo`. **Demo:** observe and reconstruct the transactions the DUT actually emitted; agent groups everything per protocol.

- **Phase 3 — Checker.** `scoreboard` construct with structural equality on transactions, `env` as the static composition root, `state<T>` flow object (§17.2) for shared scoreboard slots. **Demo:** closed-loop functional verification — random stim → DUT → monitor → scoreboard catches mismatches end-to-end. This is the milestone that makes HARC a working testbench language.

- **Phase 4 — Properties, coverage, formal export.** `assert` / `assume` / `cover property`, `pseq` (§3.4, §5), module `contract` blocks for compositional formal (§5.1), `covergroup` (§6), `cover sequence` for behavioral coverage (§17.3), BTOR2 / SMT-LIB2 export (§10.3). **Demo:** SVA-equivalent property checking layered onto the working TB; formal proof export for the property subset; coverage closure on existing stimulus.

- **Phase 5 — SV+UVM transpiler.** `harc -emit sv-uvm` (§10.2). Lossy in known places, all documented. This phase doubles as a completeness check on the language surface: anything that cannot transpile is a UVM gap, not a HARC gap. **Demo:** full HARC TB → ~10× line count of UVM that drops into Xcelium / VCS / Questa.

- **Phase 6 — Reference model embedding.** ARCH `ref module` integration from HARC, C function bodies via DPI, Sail import via the C-emulator path (§9), `stream<T>` flow object (§17.2 — main use case is ref-model continuous output). **Demo:** scoreboard compares DUT output against a Sail-derived golden model without DPI ceremony in user code.

- **Phase 7a — CPU SIMD batch CRV.** N-lane stimulus parallelism on the cycle-based backend (§10.1), AVX-512 lane masking, `when`-subtype lane grouping with per-variant constraint subproblems (§3.3), `blocking randomize` divergence handling, queued-randomize solver pool with pinned-memory hand-off design (forward-compatible with 7b). **Demo:** 64-wide regression nightlies — 10K seeds in 156 batches.

- **Phase 7b — GPU batch CRV backend.** Same architecture as 7a, CUDA kernels, 10K+ lanes per grid. Per-cycle `tb_step_kernel<<<grid, block>>>()` dispatch per clock domain; coverage merge via reduction kernels; per-lane `state<T>` / `buffer<T>` in device memory (cross-lane communication forbidden by construction); solver pool stays on host with pinned-memory channels to GPU. `blocking randomize` is strongly discouraged (host-device round-trip per call kills throughput); queued randomize is the canonical GPU path. The work in 7a — lane divergence handling, per-variant grouping, lane-masked execution, queue-based solver dispatch — maps directly to 7b. Skipping 7a and going straight to 7b skips the validation step where the architecture is confirmed at smaller scale before paying GPU debugging cost. **Demo:** 10K-seed nightly in a single GPU launch; coverage closure in seconds rather than minutes; per-test-instance `fatal` (§7.7) means one bad seed retires from the grid while siblings continue.

- **Phase 8 — Emulation subset.** Synthesizable assertion checkers binding to DUT signals (§10.4); full TB on emulation deferred to v2.

**Why split Phase 1.** Phase 1a is meaningful without an SMT dependency — most real CRV stimulus is per-field random within a range, and SystemVerilog projects routinely ship valuable testbenches built on `$urandom_range` alone. Pulling Z3 integration, queued/blocking randomize, and tagged-ADT encoding into Phase 1b lets the early demo land months sooner: a working stimulus → DUT path with no constraint solver to integrate, no solver-pool tuning, no compile-time runtime-state-dependence analysis. Phase 1a code transparently upgrades to Phase 1b — the static checker simply lifts the cross-field restriction. The split also gives a clean static-vs-dynamic boundary for the implementation: Phase 1a is pure runtime PRNG; Phase 1b adds the static-elaboration / dynamic-solver pipeline.

**Why this order, not "properties first":** an earlier draft put properties + coverage at Phase 1, on the reasoning that they form "the smallest viable language" and could ship as an SVA replacement on day one. The reordering above rejects that framing. Properties without stimulus assert against silence; coverage without stimulus measures empty space. The data path stim → monitor → checker is what makes a testbench *work*; properties are a refinement layered on top of working stimulus, not a substitute for it. Building the foundation first means each phase delivers something usable, and the property machinery (Phase 4) rides on the same event/sample plumbing the TB already has from Phases 1-3.

---

## 15. Naming

**HARC** — *Harness of ARCh*. Verification harness is a long-established term in the discipline (test harness, verification harness, wiring harness — same root); the language is the harness around an ARCH design. Four letters parallel ARCH's four; the shared "ARC" middle is visible at a glance; pronounced "hark" as a single syllable.

Alternatives considered and not picked:

| Name      | Why not                                                  |
|-----------|----------------------------------------------------------|
| VARCH     | Wordy. Working name during spec drafting.                |
| ARCV      | Easily mistyped as ARCH.                                 |
| ARC       | Collides with Synopsys ARC processor.                    |
| VEX       | Mild VEX-RISC-V collision; loses the ARCH connection.    |
| PROVE     | Lofty; overpromises the formal-first stance.             |
| AXIOM     | Conflicts with the computer-algebra system.              |

**One adjacency to keep in mind:** HAV (hardware-assisted verification) is a current Siemens/Cadence/Synopsys category term for emulation + FPGA prototyping platforms. Different category (HAV is a hardware-platform class, HARC is a language) and the audible C/V difference makes confusion in writing unlikely, but worth knowing.

---

## 16. ARCH Lowering Map

Every HARC construct has a direct lowering to an ARCH primitive — HARC adds the verification-side abstraction layer, ARCH provides the execution mechanism. This map is the contract between the two languages and the basis for the "ride on ARCH primitives" principle (§1, item 6).

| HARC construct                  | Lowering target in ARCH                              | Notes                                                   |
|---------------------------------|------------------------------------------------------|---------------------------------------------------------|
| `tseq Foo(args) -> TSeq<T>`     | `thread Foo on tb_clk rising` with state-machine body  | Coroutine resume points become thread states           |
| `fork / join_all / join_any / join_none` | Multiple `thread`s + `lock` for synchronization | Cycle-aligned by ARCH thread scheduler                 |
| `event<T>` (next-cycle)         | Module-scope `pipe_reg<T, 1>`                        | Default delivery is +1 cycle (§7.3)                    |
| `event comb<T>` (same-cycle)    | `let` binding + `comb` block                         | Comb-cycle detection at elaboration                    |
| `on event(t) ... end on`        | `seq on clk rising` block guarded by event valid     | One handler per subscriber; resolved at compile time   |
| `agent` / `env`                 | ARCH `module` with composed children                 | Static composition root                                |
| `driver`                        | ARCH `thread` driving a `bus` port's send-side methods (handshake_channel send, credit_channel send, tlm_method initiator) | One driver = one thread per protocol channel |
| `monitor`                       | ARCH `thread` reading bus port's receive-side state (passive: cannot drive) | Type system enforces passivity            |
| `sequencer`                     | `fifo<T>` of transactions + a thread that pops and emits to the driver event | Standard producer/consumer pattern |
| `scoreboard`                    | ARCH `module` with `queue` + `seq` block + `assert`  | Comparison logic compiles to ARCH assertions           |
| `transaction` (flat)            | ARCH `struct`                                        | Structural equality from ARCH's struct support         |
| `transaction` (with `when`)     | ARCH `enum` discriminator + `struct` per variant     | Tagged ADT, lowered to SMT datatype for solver         |
| `relation` / `keep`             | Solver constraints (Z3/Bitwuzla); not lowered to ARCH | Solver runs in HARC runtime, not ARCH simulation       |
| `assert` / `cover` / `assume`   | ARCH `assert` / `cover` / `assume` directly           | Same primitive, same backend                           |
| `property` / `prop` (full temporal) | ARCH temporal sugar: `a |=> b`, `past(e, N)`, `rose(a)`, `##N e`, plus shadow regs for multi-cycle | Per ARCH §25.4 |
| `pseq` (temporal sequence)      | Inlined into the consuming property; same ARCH sugar | First-class only for composition / parameterization; no separate emission |
| Module `contract` (assume/guarantee) | ARCH `bind` + assertions at boundaries           | Compositional formal scales via this                   |
| `covergroup`                    | Generated coverage tracking module + ARCH `cover` properties | Coverage data is a typed value queryable from HARC |
| `test`                          | ARCH `testbench` block                               | `scope sim` maps to testbench `init` / main / final    |
| `scope sim` (with `setup`/`run`/`check`/`teardown` blocks) | testbench `init` + `sequence main` + post-run check task + cleanup task | Phase-block mapping |
| `ref module`                    | ARCH module with C function body via DPI             | Same as ARCH §22 reference modules                     |
| `bus` port type                 | ARCH `bus` with `target` perspective                 | Per ARCH §24                                           |
| ARCH DUT bind (`let dut: ArchModule = bind ...`) | Direct typed reference into ARCH IR; co-elaborated, single binary | Default fastest path                  |
| Verilator DUT bind (`module Name kind verilator { ... }`) | `verilator --xml-only` consumed by HARC frontend; generated C++ glue maps typed signal access to `Vmodel` accessors; linked into the HARC binary alongside `Vmodel.cpp` | Raw signal access only in v1; protocol-typed binding deferred to v1.1 |
| Cycle-loop `dut.eval_domain(D)` | ARCH backend: direct C++ call into co-compiled module | Verilator backend: `Vmodel->eval()` |
| `use Foo`                       | ARCH `use Foo` (per §29)                             | Same keyword, same semantic                            |
| `apply Aspect`                  | Compile-time activation of `extend` blocks; no runtime lowering | Aspect resolution happens in HARC frontend |
| `across` cross-domain read      | ARCH `synchronizer` instance (kind ff or gray as appropriate) | Per ARCH §8.3 / §5.2                              |
| Cross-domain typed channel      | ARCH `fifo` with distinct `wr_clk` / `rd_clk` ports  | Per ARCH §8.2 (async FIFO with gray-code CDC)          |
| `[cyclic]` attribute            | Solver hint: cyclic enumeration over value space     | Runtime-only; no ARCH lowering                         |
| `[dist {...}]` attribute        | Solver hint: weighted distribution                   | Runtime-only; no ARCH lowering                         |
| `pre` / `post` hooks            | `seq` block before / after the hooked method's body  | Wraps the call site, not the method definition         |
| Queued `randomize`              | Off-cycle solver pool (HARC runtime); ARCH thread waits on result channel | Default; no per-cycle stall                |
| `log(info, "...")` / `log(warn, "...")` | ARCH `log("[id] INFO ...")` / `log("[id] WARN ...")` formatted at HARC frontend | SV `uvm_info(id, msg, UVM_MEDIUM)` / `uvm_warning(id, msg)` |
| `log(error, "...")`             | ARCH `log` + HARC failure-counter increment | SV `uvm_error(id, msg)` |
| `log(fatal, "...")`             | ARCH `log` + abort this test instance (lane retires; siblings continue) | SV `uvm_fatal(id, msg)` |
| `log(debug, "...")`             | ARCH `log` gated on runtime verbosity flag | SV `uvm_info(id, msg, UVM_HIGH)` |
| `Logger(id).info(msg)`          | Sugar over `log(info, msg, id=id)` — same lowering | Same UVM mapping with pre-bound ID |
| `blocking randomize`            | Per-cycle solver call inside the calling thread; thread stalls until result | Compile-time forced when constraint references runtime DUT state |
| `parallel { A; B }` (in tseq)   | `fork { A } { B } join_all` over ARCH threads               | Cycle-aligned; one thread per branch                    |
| `select { ... }` (in tseq)      | Guarded race: synthesized event combinator on each branch's first await; first-firing branch wins | Other branches discarded at the race point |
| `schedule { A; B; C }` (in tseq) | Solver-chosen permutation per seed, materialized at elaboration time as a fixed serial order | No backward inferencing; constraints via `solve_before`/shared events |
| `repeat N { A }` (in tseq)      | Constant `N`: unrolled at elaboration; runtime `N`: thread-scoped loop counter | Same as ARCH `repeat`             |
| `buffer<T, depth=N>`            | ARCH `fifo<T, depth=N>` with single producer / single consumer enforced at compile time | SPSC FIFO semantics                  |
| `stream<T>`                     | ARCH `comb` signal or `pipe_reg<T,1>` (always-driven, always-current)              | One writer, many readers                                |
| `state<T>`                      | ARCH `module` with a register storage cell, write port, read getter               | One writer per cycle, many readers; R-after-W tracked   |
| `cover sequence name = a -> b -> c` | Compile-time-constructed event-FSM evaluated in `tb_step`; ARCH `cover` on the FSM-accepts state | Behavioral coverage; complements `covergroup` |

**Four primitives HARC adds that have no direct ARCH equivalent:**
- The constraint solver pool — Z3/Bitwuzla integration, queued randomize, lane divergence handling for SIMD batch CRV.
- The aspect resolver — compile-time `extend` composition, `use`/`apply` audit trail, conflict detection.
- The transaction-equivalence runtime — structural comparison, scoreboard merge, coverage-as-data queries.
- The DUT backend abstraction — uniform `dut.eval_domain(D)` interface over ARCH co-compiled DUTs and Verilator-linked SV DUTs (v1), extending to commercial-sim co-sim and VHDL in v1.1+.

These four are HARC's value-add. Everything else is ARCH primitives in verification clothing.

---

## 17. Borrowed from the Portable Stimulus Standard

PSS (Accellera, v3.0 August 2024; v3.1 in progress) is the verification industry's existing standard for declarative scenario specification. Its primary use case sits at SoC level and above — vertical reuse from block to chip to post-silicon — which is *not* HARC's target scope. HARC focuses on cycle-accurate RTL block and subsystem verification, where UVM dominates and PSS has historically struggled to land.

That said, three PSS abstractions are well-engineered and slot cleanly into HARC's existing model. This section consolidates them, with PSS-aligned naming scoped under the constructs they belong to.

### 17.1 Activity composition operators on `tseq`

PSS distinguishes serial composition (default), parallel composition (`parallel`), unordered composition (`schedule`), choice (`select`), and iteration (`repeat`). HARC adopts the same vocabulary, scoped to `tseq` bodies, with semantics matched to the cycle-based execution model. Inside a `tseq` body these are bare keywords; in prose they're referred to as `tseq.parallel`, `tseq.schedule`, etc., to make the scoping explicit.

```
tseq DmaScenario -> TSeq<DmaTxn>
    setup_descriptor()                     // serial — default sequencing
    parallel                               // both branches start the same cycle
        fill_source_buffer()
        arm_dma()
    end parallel
    fire_dma()
    select                                 // race; first to fire wins
        wait_completion() => check_data()
        wait_error()      => check_recovery()
    end select
    repeat 4
        drain_one_burst()
    end repeat
    schedule                               // any valid order; solver-chosen per seed
        sample_perf_counters()
        clear_status()
        ack_interrupt()
    end schedule
end tseq DmaScenario
```

**Lowering, per operator:**

| Operator                    | Lowering                                                                                              |
|-----------------------------|-------------------------------------------------------------------------------------------------------|
| serial (default)            | linear thread state machine, one suspend point per await                                              |
| `parallel ... end parallel` | `fork branch A end branch branch B end branch join_all` — two threads, joined cycle-aligned          |
| `select ... end select` (`e1 => ...; e2 => ...`) | guarded race: synthesize an event combinator from each branch's first await; first-firing branch wins, others discarded |
| `repeat N ... end repeat`   | constant `N`: unrolled at elaboration; runtime `N`: loop counter inside the thread                    |
| `schedule ... end schedule` | solver picks a valid permutation per seed; permutation materializes deterministically at elaboration time |

`schedule` is the interesting one. PSS uses it for "any valid order satisfying flow/resource constraints" with the planner picking the order. HARC's version is lighter: per seed, the solver picks a permutation, with constraints expressed via `solve_before` / `solve_after` between scheduled actions or via shared events that establish ordering. **No backward inferencing** — see §17.4.

**Compositionality.** Operators nest:

```
parallel
    repeat 4
        drv_a.send_random()
    end repeat
    select
        wait_intr_a()      => recover()
        wait_timeout(100)  => fail("no interrupt")
    end select
end parallel
```

### 17.2 Flow objects — typed dataflow primitives

HARC's `event<T>` (§3.4, §7.3) handles reactive cycle-aligned delivery — one-shot, multi-subscriber, fires on the publishing cycle (or `comb` for same-cycle). PSS adds three flavors of typed dataflow that fill complementary niches:

```
type buffer<T, depth=N>      // SPSC FIFO, ordered, finite
type stream<T>               // continuous typed signal (always-current value)
type state<T>                // persistent shared state with R/W sequencing
```

**Comparison:**

| Type           | Producers     | Consumers       | Persistence              | Lowering target                        |
|----------------|---------------|-----------------|--------------------------|----------------------------------------|
| `event<T>`     | many          | many (fan-out)  | one cycle (or `comb`)    | `pipe_reg<T, 1>` or comb signal        |
| `buffer<T,N>`  | one           | one             | until consumed (FIFO)    | ARCH `fifo<T, depth=N>`                |
| `stream<T>`    | one           | many            | always-current value     | ARCH `comb` signal or `pipe_reg<T, 1>` |
| `state<T>`     | one writer    | many readers    | until next write         | ARCH module with reg + read getter     |

**When to use which:**

- **`event<T>`** for transient notifications that may have many listeners. Example: "transaction observed by monitor."
- **`buffer<T>`** for queued producer/consumer pipelines where ordering and finite depth matter. Example: "transactions waiting to be driven into the bus."
- **`stream<T>`** for continuous reference output. Example: "ref model emits one expected sample per cycle, scoreboard reads on every cycle."
- **`state<T>`** for shared scoreboard slots — the most common new use case. Example: "expected status register value after the last write."

**`state<T>` example:**

```
state<uint<32>> expected_status

on env.agent.monitor.write(t)
    if t.addr == STATUS_REG
        expected_status <- t.data           // sequenced write
    end if
end on

on env.agent.monitor.read(t)
    if t.addr == STATUS_REG
        assert t.data == expected_status    // read after write, same cycle or later
    end if
end on
```

`state<T>` semantics:
- Exactly one writer per cycle (compile-time enforced; multiple writers in the same cycle is a compile error)
- Many readers; reads see the most-recent committed write
- Read-after-write across cycles is tracked via shadow registers in the lowering
- Default-initialized to `T`'s default; explicit init via `state<T> = expr`

**`buffer<T>` example:**

```
buffer<AxiTxn, depth=16> pending_writes

tseq Producer
    repeat 100
        let t: AxiTxn
        randomize(t)
        pending_writes <- t                 // enqueue; blocks if full
    end repeat
end tseq Producer

tseq Consumer
    loop
        let t = pending_writes.recv()       // dequeue; blocks if empty
        drv.send(t)
    end loop
end tseq Consumer
```

The compiler enforces SPSC at elaboration: exactly one `<-` site, exactly one `.recv()` site (or `on buffer(t) ... end on` reactive form). Multi-producer or multi-consumer requires a different primitive.

### 17.3 Behavioral sequence coverage

`covergroup` (§6.2) covers data values. PSS 3.0's behavioral coverage covers *orderings of events*. HARC adopts this as `cover sequence`, the one place the bareword `sequence` appears as a HARC keyword:

```
cover sequence dma_full_lifecycle =
    setup -> fire -> complete

cover sequence dma_aborted =
    setup -> fire -> abort

cover sequence parallel_drain =
    fire -> { drain_a, drain_b in any_order } -> complete

cover sequence retry_after_error =
    setup -> fire -> error -> setup -> fire -> complete
```

**Operators inside a `cover sequence` pattern:**

| Operator                                | Meaning                                                              |
|-----------------------------------------|----------------------------------------------------------------------|
| `a -> b`                                | event `a` followed (any number of cycles later) by event `b`         |
| `a ->[N] b`                             | event `a` followed exactly `N` cycles later by `b`                   |
| `a ->[m:n] b`                           | event `a` followed `m` to `n` cycles later by `b`                    |
| `{ a, b in any_order }`                 | both events occur, in either order                                   |
| `a | b`                                 | either `a` or `b` (matches whichever fires first)                    |
| `repeat[N] a`                           | event `a` fires `N` times                                            |

Each `cover sequence` declares an ordered (or partially-ordered) pattern over named events. The runtime maintains a per-sequence FSM; when the pattern reaches its accept state, the sequence is marked covered. Coverage reports list which named scenarios were exercised, alongside data covergroup percentages.

**Implementation rides on existing plumbing.** The sequence FSM is a compile-time-constructed state machine over event observations, evaluated once per cycle in `tb_step`. The coverage hit is exposed via ARCH `cover` on the FSM's accept state, so behavioral coverage data flows through the same UCDB/JSON export path as covergroups (§6.3).

This complements (does not replace) data covergroups. Together they answer two questions: "did we exercise the value space" (`covergroup`) and "did we exercise the scenario space" (`cover sequence`).

### 17.4 What HARC does NOT borrow from PSS

The cuts are deliberate and follow from HARC's scope (RTL block-level cycle-accurate verification):

- **Goal-directed (backward) inferencing planner.** PSS's signature feature: declare a goal action, the SAT-based planner finds an action sequence that reaches it via flow / resource / scheduling constraints. Heavy machinery; the value crystallizes at SoC integration level (DMA paths, memory regions, interrupt routing) — not at the cycle-accurate block level HARC targets. HARC's CRV solver pool plus seed-driven `schedule` ordering covers the block-level case without the planner.

- **Class-based action model.** PSS uses inheritance for actions and components. HARC has explicitly rejected class hierarchies (§12); the `extend` aspect machinery (§3.6) covers the legitimate composition cases without inheritance. PSS's `extend` syntax does not require inheritance; we adopt the part that's class-independent.

- **Resource pools at testbench scope.** ARCH's thread-level `lock` handles mutual exclusion at the level HARC operates; a parallel resource-pool abstraction at TB scope would duplicate ARCH primitives without meaningful added expressivity for block-level work.

- **Goal annotations without the planner.** Without backward inferencing, `goal` declarations on tseqs are equivalent to comments. Comments work fine.

- **`exec` blocks for embedded target codegen.** PSS embeds C/SV directly inside source for backend codegen. HARC's transpile is compiler-driven from the IR — the codegen path is invisible at the language level, and this is a feature, not a gap.

- **Vertical reuse to bare-metal C / post-silicon.** The strongest PSS value proposition, but it presumes HARC moves up the stack into SoC scope. v1 targets RTL block and subsystem verification; the vertical-reuse story is at most a v2+ transpile target.

### 17.5 Phasing

The borrowed features distribute across HARC's implementation phases (§14):

- **Activity composition operators** (`parallel`, `schedule`, `select`, `repeat`) — Phase 1a, alongside `tseq` itself; they're the composition vocabulary `tseq` needs to be useful for non-trivial scenarios. None of these operators require the SMT solver; `schedule` uses topological-sort over partial-order constraints, not SAT.
- **`buffer<T>`** — Phase 1a, with stimulus plumbing; thin wrapper over ARCH `fifo`, no solver needed.
- **`state<T>`** — Phase 3, with the checker; main use case is shared scoreboard slots.
- **`stream<T>`** — Phase 6, with reference model embedding; main use case is continuous ref-model output.
- **`cover sequence`** — Phase 4, with the rest of coverage.

No phase needs to slip; all three borrowings fit inside the existing phase budget.

---
