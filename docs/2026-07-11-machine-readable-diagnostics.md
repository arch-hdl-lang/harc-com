# Machine-readable diagnostics for `harc` (companion to arch-com proposal)

Status: proposal, not started. No branch/PR yet.

Full proposal and rationale live in
[`arch-com/ideas/2026-07-11-machine-readable-diagnostics.md`](../../arch-com/ideas/2026-07-11-machine-readable-diagnostics.md)
(both compilers share the exact same `miette` + `thiserror` diagnostics
architecture, so the design is one proposal covering both repos). This file
tracks the harc-specific half.

## harc-specific finding

harc-com's gap is sharper than arch-com's. In addition to `src/diagnostics.rs`'s
`CompileError` (parse-time, has spans, same `miette::Diagnostic` shape as
arch-com — just missing error codes and a JSON reporter, same fix), TB-IR
lowering has its own, separate `LowerError` enum
(`src/ir/lower/mod.rs:43`):

```rust
pub enum LowerError {
    Unsupported { construct: String, detail: String },
    Invalid(String),
}
```

This type does **not** implement `miette::Diagnostic` and carries **no
`Span`**. It reaches the user via `src/main.rs:1397`:

```rust
let prog = harc::ir::lower::lower_program(&merged).map_err(|e| miette::miette!("{}", e))?;
```

— a bare, unlabeled string wrapped in an ad-hoc `miette!`. Every TB-IR
rejection (the majority of harc-com's current open-issue backlog: #494,
#483, #484, #425 are all TB-IR lowering-gap reports) is reported with zero
file/line/column, even though the rejection is always raised while walking
a specific spanned AST node — the span is available at the raise site and
is just being dropped in `.detail`/`.message`. That absence of location is
plausibly *why* those issues are written as hand-pasted minimal repros
rather than "see line N": there's nothing else to point at.

Also directly relevant: harc-com#482 was a real shipped bug caused by *not*
having `Unsupported` vs `Invalid` typed as a proper category — an `Invalid`
case (structurally-impossible recursive record) was misclassified through
the `Unsupported` variant, which always appends "re-run with `--codegen
v1`," and `--codegen v1` then stack-overflowed on the same input (#483).
Giving `LowerError` a real `category` enum field (instead of two
constructor shapes that only differ by which `Display` arm fires) is the
type-level fix for that whole bug class, independent of whether the JSON
output ships.

## harc-specific implementation steps

1. Add `span: Span` to both `LowerError::Unsupported` and
   `LowerError::Invalid` — threaded from the originating AST node at every
   raise site (`unsupported()` helper at `mod.rs:67` and the ~89
   `unsupported()`/`Invalid(...)` call sites across `src/ir/lower/*.rs`,
   per the sweep already done for issue #355's "structural debt" audit).
2. Implement `miette::Diagnostic` for `LowerError` (mirrors
   `CompileError`'s existing `#[label]`/`#[diagnostic(help(...))]` pattern)
   instead of routing through `miette::miette!("{}", e)`.
3. Replace the two-variant `Unsupported`/`Invalid` `Display`-based
   distinction with an explicit typed category so it can populate a JSON
   `"category"` field directly (fixes #482's root cause as a side effect).
4. Everything else (`--error-format json` flag, `JSONReportHandler` wiring,
   golden-file tests) is identical in shape to the arch-com plan — see the
   full proposal doc linked above.

## Not a duplicate

Checked open harc-com issues/PRs (2026-07-11): #494/#483/#484/#425/#355 are
all about *what* TB-IR does or doesn't lower, not about *how* lowering
errors are reported. #463 (code graph index) is semantic-navigation
tooling, not diagnostics. #329 (shared references) and #316 (MT safety
audit) are unrelated language/runtime features. No open issue proposes
structured diagnostic output or spans on `LowerError`.
