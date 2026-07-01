# Note: language server (LSP) proposal, tracked in arch-com

Status: research note — no code changes proposed here. Full proposal
lives in arch-com's `doc/proposal_language_server.md` (this file is a
pointer + the HARC-specific angle, following the pattern of
arch-com's `doc/proposal_arch_harc_tlm_consistency.md` for cross-repo
topics).

## Summary

Neither `arch-com` nor `harc-com` has any semantic editor tooling today —
`editors/vim` and `editors/vscode` in both repos are hand-maintained
*static* TextMate/vim syntax grammars only (see each repo's
`editors/README.md`), kept in sync with `src/lexer.rs`'s keyword list by
hand. There is no language server: no inline diagnostics, hover,
go-to-definition, or completion in either language. A grep across both
repos' issues/PRs for "language server" / "LSP" / "lsp" turns up nothing
beyond the existing grammar work, and arch-com's
`doc/plan_arch_doc_comments.md` §7 lists "IDE / LSP hover support" as
deferred, ownerless future work.

## Why this matters for HARC specifically

HARC's TB-IR migration (docs/tb-ir-plan.md) and the v1-vs-tbir gap
inventory (harc-com#425) are exactly the kind of "silent until you run
it" surface an LSP would help with: dozens of latent-but-real construct
rejections that today only show up as a CLI error the moment someone's
fixture happens to touch the gap. Live diagnostics while editing a `.harc`
testbench would surface a TB-IR lowering rejection (e.g. #476's nested
struct fields) the moment it's typed, not after a `harc sim` run.

## Proposed shared core

Since `arch-com` and `harc-com` maintain parallel AST/resolver shapes
(the same discipline that keeps operator parity tracked in harc-com#473
and ARCH↔HARC TLM semantics tracked in arch-com's
`proposal_arch_harc_tlm_consistency.md`), the LSP's transport layer
(span→`Range` conversion, debounce, `publishDiagnostics` plumbing, the
VSCode client shim) should be near-identical between an `arch-lsp` and
an `harc-lsp` binary — see the full proposal for the suggested v1 scope
(diagnostics-only, wrapping the existing lex/parse/resolve/typecheck
pipeline in-memory) and v2 scope (hover, go-to-definition, reusing
whatever lands from the open harc-com#463 code-graph-index issue as the
semantic backend).

No implementation is proposed in this note — see arch-com's
`doc/proposal_language_server.md` for the concrete plan, risks, and
suggested first PR.
