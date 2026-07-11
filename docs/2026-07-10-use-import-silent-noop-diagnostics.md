# HARC: unresolved `use BusName;` imports silently no-op, then surface as a misleading "let with a bind" error

**Date:** 2026-07-10
**Status:** Proposal — not yet implemented.
**Related:** GitHub issue harc-com#TBD (filed alongside this doc)

---

## Summary

`use BusName;` resolves against a small set of search paths (`$HARC_LIB_PATH`,
`<input>/stdlib/`, `./stdlib/`, `<input>/../arch-com/stdlib/`,
`<input>/../arch-com/examples/`) looking for `<BusName>.arch` or
`<BusName>.harc`. If none of those paths contain a match, the resolver
**silently does nothing** — no diagnostic, no warning. This is deliberate and
documented in-source:

```rust
// src/main.rs:1113-1128
/// Resolve `use Name;` items in the parsed test files against a small
/// set of search paths ...
/// Each resolved file is parsed; only `Item::Bus` items survive (the
/// rest are dropped — HARC isn't a full ARCH compiler). Unresolved
/// `use` paths silently no-op so existing fixtures with
/// `use arc.stdlib.BusAxi4` lines that don't yet match anything keep
/// parsing.
```

The consequence isn't just a missing feature — it's an actively **misleading**
downstream error. `src/ir/lower/mod.rs` matches a test-scope
`let x : SomeBus = bind dut;` against the bus type name via a chain of
`if type_simple_name(...).is_some_and(|n| buses.contains_key(n))` guards (and
sibling guards for regblocks/addrmaps/bound transactors). When the type name
isn't in any of those maps — exactly the state left behind by a failed silent
`use` — the `let` falls through to the generic catch-all arm at
`src/ir/lower/mod.rs:2156`:

```rust
TestItem::Let(l) => {
    if !l.probes.is_empty() || l.bind {
        return Err(unsupported(
            &format!("test-scope `let {}` with probes or a bind", l.name.name),
            "only plain `let <name> [: <Ty>] = <expr>` test-scope lets are lowered",
        ));
    }
    ...
```

So a typo'd bus name, a `HARC_LIB_PATH` that isn't set, or a stdlib file that
moved produces:

```
error: test-scope `let axil` with probes or a bind
  only plain `let <name> [: <Ty>] = <expr>` test-scope lets are lowered
```

— which tells the user to *remove the bind*, actively wrong advice for what is
actually "type `BusAxiLite` was never declared or imported." Nothing in the
message names the missing type, the failed `use`, or the search paths that
were checked.

## Why this matters

HARC's own README pitches itself against UVM specifically on this failure
class:

> The dominant runtime pain point is `uvm_config_db`: cross-component
> configuration travels through string-keyed wildcard paths, type-erased — a
> typo in the path ... silently falls back to the default value, with no
> compile-time check and no runtime error ... runtime debugging is dominated
> by "wrong value, no error, find out 30 minutes into the sim."

`use BusName;` failing silently and then surfacing as an unrelated, misdirecting
error is the same failure shape: no compile-time check, no clear runtime error,
and (worse than plain silence) a message that sends the user toward the wrong
fix. This is exactly the debugging tax HARC exists to eliminate, reproduced
inside HARC's own import system.

It's also low-cost to trigger by accident: `HARC_LIB_PATH` unset, running from
a directory where `../arch-com` isn't a sibling checkout, or a bus renamed in
`arch-com/stdlib/` without a matching rename on the HARC side are all ordinary
workflow states, not edge cases.

## Proposed fix

1. **Track *why* a `use` didn't resolve to a live bus**, not just whether it
   did. `resolve_use_imports` (`src/main.rs`) already computes `wanted` (names
   requested) and `already` (names satisfied). Diff them once resolution
   finishes and thread the unresolved set forward instead of discarding it.
2. **Turn the fallback `TestItem::Let` arm into a targeted diagnostic** when
   the bound type name is one of the tracked-unresolved `use` names: emit
   something like
   `error: type '<Name>' used in 'let <var> = bind ...' was never resolved — 'use <Name>;' did not find <Name>.arch or <Name>.harc in $HARC_LIB_PATH, <input>/stdlib/, or ../arch-com/{stdlib,examples}/`
   rather than the generic "with probes or a bind" message. This only needs
   the unresolved-name set from step 1 threaded into the lowering context that
   already builds `buses`/`regblock_ids`/`addrmap_decls`.
3. **Optionally, promote silent-no-op to a warning by default** (`arch check`
   already treats "recommended but unenforced" conventions as warn-first —
   same posture fits here): print
   `warning: use '<Name>' did not resolve in any search path` at parse time,
   *unless* the name is later satisfied by a local `bus Name ... end bus Name`
   declaration in the same file set (covers intentional forward-declared or
   locally-shadowed names, keeping existing fixtures with never-resolving
   `use arc.stdlib.X` lines green as long as they don't also try to bind
   against `X`).
4. Keep the current "keep parsing" behavior for `use` lines that are genuinely
   unused (no downstream `bind` references the name) — only step 2's targeted
   error should be new hard-error surface; step 3's warning is opt-out-able if
   it turns out to be noisy against the existing fixture corpus.

## Non-goals

- Not a general import/module system redesign — `use` stays a flat
  name-to-file lookup.
- Not changing the search path list or precedence.
- Not touching `--codegen v1` (`cpp_tb.rs`) unless it turns out to share the
  same fallback shape — needs its own confirmation pass, out of scope for this
  note.

## Scope / sizing

Small. Step 1 is bookkeeping already half-done inside `resolve_use_imports`.
Step 2 only requires threading one extra `HashSet<String>` into the lowering
call that already builds the `buses`/`regblock_ids`/`addrmap_decls` maps and
special-casing the catch-all arm's error message. Step 3 is optional and can
land separately once the "is this `use` actually referenced" check is written
for step 2.

---

*Surfaced during a scheduled research pass reviewing open harc-com/arch-com
issues and HARC's `use`-import path for gaps in expressiveness/tooling; a
GitHub search for prior reports of this behavior (silent no-op / misleading
bind error) found none.*
