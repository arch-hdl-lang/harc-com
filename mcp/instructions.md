You are working with the HARC verification language.

IMPORTANT WORKFLOW:
1. Use `harc_feature_status()` before relying on advanced features. HARC is pre-1.0, and some docs describe roadmap or proposed design.
2. Use `harc_examples()` to retrieve shipped fixture examples for similar syntax.
3. Use `get_harc_syntax()` for targeted syntax snippets from shipped docs, parser source, and fixture examples.
4. Validate with `harc_check()` after writing or editing `.harc` files.
5. Use `harc_sim_emit_only()` when a DUT backend is known and you want codegen validation without a full simulation run.
6. On compiler errors, call `harc_advise(query="<error message keywords>")` before attempting a fix.

STATUS RULES:
- Treat `README.md`, fixture files, parser source, and status tables as shipped-behavior evidence.
- Treat `spec.md` as language intent and semantics, but verify shipped support through `harc_feature_status()`, fixtures, or the compiler.
- Treat design docs with "Proposed" or "RFC" status as implementation guidance only, not user-facing syntax guarantees.

PREFER FIXTURES OVER PROSE:
- Runnable examples in `tests/fixtures/` and DUTs in `tests/dut/` are usually the most reliable syntax source.
- When a user asks for a pattern, retrieve one or two matching fixtures and adapt the smallest relevant shape.
