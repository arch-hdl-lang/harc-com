//! Lifecycle and testbench-scope source normalization shared by code generators.

use crate::ast::*;
use crate::codegen::source_transform::{is_concurrent_assertion, SharedLifecycleBodies};
use crate::lexer::Span;

/// Desugar `impl <name> for <TbType>` tests into the classic
/// `test <name>` form (docs/test-ergonomics.md §3.3). For each test
/// with `for_testbench: Some(tb)`:
///
/// 1. Find the bound testbench's declaration (in `file.items`).
/// 2. Classify its fields:
///    - First named-type field whose type isn't a known HARC
///      construct → the DUT (e.g. `dut : Top`). Synthesize a test-
///      scope `let dut : <SVType>` so the existing Verilator-init
///      path picks up the allocation. (Other DUT-typed fields are
///      currently out of scope — multi-DUT testbenches will need a
///      separate plumbing pass.)
///    - All fields participate in the bare-name-rewrite set.
/// 3. Synthesize a test-scope `let _tb : <TbType>` so the existing
///    component-instantiation path constructs the testbench struct.
/// 4. Synthesize `_tb.dut = dut` at the start of the run block so
///    the testbench's DUT pointer is wired to the test-scope let.
/// 5. Walk every Stmt / Expr in the test items and rewrite bare
///    `Ident(name)` references where `name` matches a testbench
///    field (other than `dut`) or a testbench helper method to
///    `_tb.<name>` field-access or `_tb.<name>(...)` call,
///    respectively. `dut` keeps its bare form — it refers to the
///    synthesized test-scope let, which is the same underlying
///    VTop instance as `_tb.dut` (one allocation, two pointers).
///
/// Tests with `for_testbench: None` (classic form) pass through
/// unchanged.
/// Returns the file unchanged (borrowed) when no test carries an
/// `impl <Test> for <Tb>` binding.
///
/// It used to clone the entire `SourceFile` up front regardless, which on
/// a large classic-form suite is the biggest single allocation in the
/// frontend for an edit that never happens.
///
/// NOT a no-op for impl-for sources, which is most of this repo's corpus.
/// `lower_program` desugars into a local binding that dies when it
/// returns, and `cmd_sim` hands the ORIGINAL merged file to every
/// consumer, so such a suite still pays this clone once per caller —
/// `dut_probes`, lowering, and the randomize emitter. Desugaring once up
/// front in `cmd_sim` would collapse all three to a borrow; see harc#546.
pub(crate) fn desugar_impl_for_test_in_file(file: &SourceFile) -> std::borrow::Cow<'_, SourceFile> {
    desugar_impl_for_test_in_file_inner(file, false, None)
}

/// Captured reusable-testbench lifecycle phase bodies, keyed by
/// `(source-testbench name, phase)`. Each `Block` is the phase body AFTER
/// the `_tb.<field>` rewrite the desugar applies — the exact shape the
/// historical per-test inlining would have produced — so TB-IR lowering
/// can lower it ONCE into a `TestbenchLifecycle` function whose re-emitted
/// output is trace-identical (same field references and statement order;
/// local names differ). First test to bind a given testbench wins (later
/// binds produce an identical rewrite for the corpus). See #619 M4a.
/// Like [`desugar_impl_for_test_in_file`], but for the TB-IR native
/// lifecycle-ownership path (issue #619 M4a). Instead of splicing each
/// bound testbench's `setup`/`check`/`teardown` bodies into every test,
/// it leaves a single `__harc_tb_lifecycle_<phase>()` marker call in
/// their place (the wire and the impl-owned lifecycle statements stay
/// exactly where they were). TB-IR lowering lowers each testbench phase
/// body ONCE into a `FunctionKind::TestbenchLifecycle` function and
/// lowers the marker to a call to it, so the reusable lifecycle exists
/// once rather than copied per test.
///
/// v1 never calls this — it keeps the historical inlining via
/// [`desugar_impl_for_test_in_file`]. See docs/619-m4a-ir-ownership.md.
pub(crate) fn desugar_impl_for_test_sharing_lifecycle(
    file: &SourceFile,
) -> (std::borrow::Cow<'_, SourceFile>, SharedLifecycleBodies) {
    let mut captured = SharedLifecycleBodies::new();
    let file = desugar_impl_for_test_in_file_inner(file, true, Some(&mut captured));
    (file, captured)
}

/// The reserved marker-call name TB-IR lowering recognizes for a shared
/// testbench lifecycle phase, under `share_lifecycle` desugaring.
pub(crate) fn tb_lifecycle_marker_name(phase: LifecyclePhase) -> &'static str {
    match phase {
        LifecyclePhase::Setup => "__harc_tb_lifecycle_setup",
        LifecyclePhase::Check => "__harc_tb_lifecycle_check",
        LifecyclePhase::Teardown => "__harc_tb_lifecycle_teardown",
    }
}

/// A one-statement block holding the reserved lifecycle marker call for
/// `phase`. Substituted for a testbench lifecycle body when desugaring
/// in `share_lifecycle` mode (#619 M4a).
fn tb_lifecycle_marker_block(phase: LifecyclePhase, span: Span) -> Block {
    let callee = Expr {
        kind: Box::new(ExprKind::Ident(Ident {
            name: tb_lifecycle_marker_name(phase).into(),
            span,
        })),
        span,
    };
    let call = Expr {
        kind: Box::new(ExprKind::Call {
            callee,
            args: Vec::new(),
        }),
        span,
    };
    Block::new(
        vec![Stmt {
            kind: StmtKind::Expr(call),
            span,
        }],
        span,
        SourceId::default(),
    )
}

// ---------------------------------------------------------------------------
// #619 M4a sub-step 4: conservative shareability decision.
//
// A reusable testbench's lifecycle may be SHARED (lowered once, marker →
// call edge) only when doing so cannot change the trace vs the historical
// per-test inlining. Two hazards force a per-testbench fallback to the
// historical inlining (byte-identical to switch-OFF):
//
//   1. Side-table state. A phase body — or ANY of the testbench's own
//      methods, scanned as a conservative union — that mints per-test
//      side-table state (a `randomize` site, a concurrent `cover`, or a
//      concurrent `property` check) must not be shared: lowering it once
//      attributes that state to the FIRST binding test only.
//   2. Field shadowing. If any binding test declares a test-scope `let`
//      whose name collides with a testbench field, that test's `_tb.<field>`
//      rewrite differs from the shared (first-bind) body.
//
// When in doubt we classify UNSAFE. The union method scan and the
// any-field shadow rule are deliberately coarse (per-testbench, not
// per-phase / per-referenced-field); a false "unsafe" only forgoes the
// sharing optimization and falls back to the always-correct inlining.
// ---------------------------------------------------------------------------

/// Recursive scanner for share-unsafe constructs. Carries the file's
/// declared-`property` set so it can apply the AUTHORITATIVE
/// concurrent-assertion predicate (`is_concurrent_assertion`) that lowering
/// itself uses — a purely syntactic `named`/`property_kw` shortcut misses
/// temporal-operator asserts (`|->`, `$rose`, `##N`, …) and bare-ident
/// property references, which mint per-test `PropertyCheckId`s.
///
/// The statement/expression traversals mirror the exhaustive matches of
/// `rewrite_stmt_for_impl` / `rewrite_expr_for_impl`, so a new AST variant
/// forces this to be revisited (arms are compiler-checked for
/// exhaustiveness).
struct ShareScan<'a> {
    properties: &'a std::collections::HashMap<String, Expr>,
    found: bool,
}

impl<'a> ShareScan<'a> {
    fn new(properties: &'a std::collections::HashMap<String, Expr>) -> Self {
        ShareScan {
            properties,
            found: false,
        }
    }

    /// True if `kind` DIRECTLY mints per-test side-table state or a
    /// per-test-context-dependent body (not recursing).
    fn stmt_directly_unsafe(&self, kind: &StmtKind) -> bool {
        match kind {
            // A randomize site mints a per-test constraint problem.
            StmtKind::Randomize { .. } => true,
            // Every `cover` (named or anonymous) mints a per-test
            // CoverCheckId — `lower_cover` registers unconditionally.
            StmtKind::Cover(_) => true,
            // A CONCURRENT assert/assume mints a per-test PropertyCheckId.
            // Use the SAME predicate lowering uses (`lower_assert` /
            // `lower_assume` → `is_concurrent_assertion`): a bare ident
            // naming a declared property OR any temporal operator
            // (`|->`/`|=>`/`throughout`/`within`/`intersect`/`##`/`[*]`/
            // `$past`/`$rose`/`$fell`/`$stable`). An immediate boolean
            // assert/assume is safe.
            StmtKind::Assert(v) | StmtKind::Assume(v) => v
                .expr
                .as_ref()
                .is_some_and(|e| is_concurrent_assertion(e, self.properties)),
            // A nested `on ...` handler (a per-register `on regs.REG`
            // callback, a method hook, or a cycle trigger) is rewritten
            // against PER-TEST context — regblock bindings, method-hook
            // param names — so a copy folded into a shared phase would
            // differ across binding tests. Conservatively unshareable.
            StmtKind::On(_) => true,
            _ => false,
        }
    }

    fn block(&mut self, b: &Block) {
        for s in &b.stmts {
            if self.found {
                return;
            }
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        if self.found {
            return;
        }
        if self.stmt_directly_unsafe(&s.kind) {
            self.found = true;
            return;
        }
        match &s.kind {
            StmtKind::Let(l) => {
                if let Some(v) = l.value.as_ref() {
                    self.expr(v);
                }
            }
            StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::For(f) => {
                self.expr(&f.iter);
                self.block(&f.body);
            }
            StmtKind::Repeat(r) => {
                self.expr(&r.count);
                self.block(&r.body);
            }
            StmtKind::Loop(b) => self.block(b),
            StmtKind::While { cond, body, .. } => {
                self.expr(cond);
                self.block(body);
            }
            StmtKind::If(ifs) => {
                self.expr(&ifs.cond);
                self.block(&ifs.then_block);
                for (c, b) in ifs.elsifs.iter() {
                    self.expr(c);
                    self.block(b);
                }
                if let Some(b) = ifs.else_block.as_ref() {
                    self.block(b);
                }
            }
            StmtKind::Fork(fk) => {
                for b in fk.branches.iter() {
                    self.block(b);
                }
            }
            StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => {
                for b in blocks.iter() {
                    self.block(b);
                }
            }
            StmtKind::Select(arms) => {
                for a in arms.iter() {
                    self.expr(&a.event);
                    self.block(&a.action);
                }
            }
            // `On` is flagged directly-unsafe above; the arm is kept for
            // exhaustiveness (its body would be scanned if that changed).
            StmtKind::On(h) => {
                self.expr(&h.event);
                self.block(&h.body);
            }
            StmtKind::After { duration, body, .. } => {
                self.expr(duration);
                self.block(body);
            }
            StmtKind::Wait { duration, .. } => self.expr(duration),
            StmtKind::WaitUntil {
                conditions,
                timeout,
                ..
            } => {
                for c in conditions.iter() {
                    self.expr(c);
                }
                if let Some(t) = timeout.as_ref() {
                    self.expr(&t.cycles);
                    if let Some(m) = t.message.as_ref() {
                        self.expr(m);
                    }
                }
            }
            StmtKind::Yield(e) | StmtKind::Release(e) => self.expr(e),
            StmtKind::Return(opt) => {
                if let Some(e) = opt.as_ref() {
                    self.expr(e);
                }
            }
            // Immediate assert/assume reach here (the concurrent forms were
            // caught by `stmt_directly_unsafe`); scan operands for a nested
            // `randomize` expression. `Cover`/`Randomize` never reach here
            // (already flagged), but are listed for exhaustiveness.
            StmtKind::Assert(v) | StmtKind::Assume(v) | StmtKind::Cover(v) => {
                if let Some(e) = v.expr.as_ref() {
                    self.expr(e);
                }
                if let Some(e) = v.else_fail.as_ref() {
                    self.expr(e);
                }
            }
            StmtKind::Randomize {
                target, with_body, ..
            } => {
                self.expr(target);
                for e in with_body.iter() {
                    self.expr(e);
                }
            }
            StmtKind::Log { args, .. }
            | StmtKind::LogF { args, .. }
            | StmtKind::Emit { args, .. } => {
                for a in args.iter() {
                    self.arg(a);
                }
            }
            StmtKind::Fail { msg, .. } => self.expr(msg),
            StmtKind::JoinAll { .. }
            | StmtKind::Apply(_)
            | StmtKind::Break { .. }
            | StmtKind::Continue { .. } => {}
        }
    }

    fn arg(&mut self, a: &CallArg) {
        match a {
            CallArg::Expr(e) => self.expr(e),
            CallArg::Named { value, .. } => self.expr(value),
        }
    }

    fn expr(&mut self, e: &Expr) {
        if self.found {
            return;
        }
        match e.kind.as_ref() {
            // `randomize(t)` in expression position (`let x = randomize(t)`).
            ExprKind::Randomize { .. } => {
                self.found = true;
            }
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                for a in args.iter() {
                    self.arg(a);
                }
            }
            ExprKind::Field { target, .. } => self.expr(target),
            ExprKind::Index { target, index } => {
                self.expr(target);
                self.expr(index);
            }
            ExprKind::BitSlice { target, hi, lo } => {
                self.expr(target);
                self.expr(hi);
                self.expr(lo);
            }
            ExprKind::Cast { expr, .. } => self.expr(expr),
            ExprKind::Send { target, value } => {
                self.expr(target);
                self.expr(value);
            }
            ExprKind::Unary { expr, .. } => self.expr(expr),
            ExprKind::ForkCall { call } => self.expr(call),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond);
                self.expr(then_branch);
                self.expr(else_branch);
            }
            ExprKind::HashHash { expr, .. } => self.expr(expr),
            ExprKind::SeqRepeat { expr, .. } => self.expr(expr),
            ExprKind::RangeLit { lo, hi } => {
                if let Some(lo) = lo.as_ref() {
                    self.expr(lo);
                }
                if let Some(hi) = hi.as_ref() {
                    self.expr(hi);
                }
            }
            ExprKind::SetLit(es)
            | ExprKind::SystemCall { args: es, .. }
            | ExprKind::SolveOrder { args: es } => {
                for x in es.iter() {
                    self.expr(x);
                }
            }
            ExprKind::ForEachConstraint { iter, body, .. } => {
                self.expr(iter);
                for x in body.iter() {
                    self.expr(x);
                }
            }
            ExprKind::SoftConstraint(sc) => {
                self.expr(&sc.expr);
                if let Some(weight) = sc.weight.as_ref() {
                    self.expr(weight);
                }
            }
            ExprKind::DistDirective { target, .. } => self.expr(target),
            ExprKind::Paren(x) => self.expr(x),
            ExprKind::NamedArg { value, .. } => self.expr(value),
            ExprKind::StructLit { fields: nfs, .. } => {
                for nf in nfs.iter() {
                    self.expr(&nf.value);
                }
            }
            ExprKind::CoverArrow { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Membership { expr, set } => {
                self.expr(expr);
                self.expr(set);
            }
            ExprKind::DistLit(_)
            | ExprKind::String(_)
            | ExprKind::Ident(_)
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Time(_)
            | ExprKind::Bool(_)
            | ExprKind::ImplicitSelf => {}
        }
    }
}

/// Conservative UNION scan (#619 M4a sub-step 4): a testbench's lifecycle
/// is share-unsafe if ANY body that could be FOLDED INTO the shared
/// lifecycle function contains a share-unsafe construct. Only two item
/// kinds contribute such a body — the lifecycle phase itself, and any
/// helper method a phase CFG-inlines (`hookable`/`function`). Scanning all
/// methods (rather than only those a phase calls) is a deliberate
/// over-approximation; a false "unsafe" only forgoes the sharing
/// optimization. The match is EXHAUSTIVE (no wildcard) so a new
/// `ComponentItem` kind forces an explicit safe/unsafe decision here.
fn testbench_lifecycle_share_unsafe(
    tb: &ComponentDecl,
    properties: &std::collections::HashMap<String, Expr>,
) -> bool {
    tb.items.iter().any(|ci| {
        let mut scan = ShareScan::new(properties);
        match ci {
            // The shared lifecycle function is lowered from the phase body,
            // plus any helper method the phase CFG-inlines at a call site.
            ComponentItem::Lifecycle(_, body)
            | ComponentItem::Hookable(HookableMethod { body, .. }) => scan.block(body),
            // The remaining item kinds never contribute a body to the
            // SHARED lifecycle function, so their contents cannot change a
            // shared phase's trace:
            //   * Field / Connect — declarations / wiring, no procedural body.
            //   * OnHandler — a testbench-scope `on` service lowered to its
            //     OWN per-test `TestHook` function (periodic/cycle service),
            //     never folded into a lifecycle phase.
            //   * TargetTlmThread — a bound-target responder thread, separate.
            //   * Watchdog — desugars to a periodic-service checker, separate.
            //   * Apply — aspect activation; inert (no backend applies it).
            ComponentItem::Field(_)
            | ComponentItem::Connect(_)
            | ComponentItem::OnHandler(_)
            | ComponentItem::TargetTlmThread(_)
            | ComponentItem::Watchdog(_)
            | ComponentItem::Apply(_) => {}
        }
        scan.found
    })
}

fn desugar_impl_for_test_in_file_inner<'a>(
    file: &'a SourceFile,
    share_lifecycle: bool,
    mut captured: Option<&mut SharedLifecycleBodies>,
) -> std::borrow::Cow<'a, SourceFile> {
    if !file
        .items
        .iter()
        .any(|it| matches!(it, Item::Test(t) if t.for_testbench.is_some()))
    {
        return std::borrow::Cow::Borrowed(file);
    }
    // Index components by name so the desugarer can resolve the
    // bound testbench's field list without re-walking the file.
    let mut components: std::collections::HashMap<String, ComponentDecl> =
        std::collections::HashMap::new();
    let mut component_sources: std::collections::HashMap<String, SourceId> =
        std::collections::HashMap::new();
    for (index, it) in file.items.iter().enumerate() {
        match it {
            Item::Env(c) | Item::Agent(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                components.insert(c.name.name.clone(), c.clone());
                component_sources.insert(c.name.name.clone(), file.item_source(index));
            }
            _ => {}
        }
    }
    let transactors: std::collections::HashMap<String, TransactorDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transactor(t) => Some((t.name.name.clone(), t.clone())),
            _ => None,
        })
        .collect();
    let scoreboards: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Scoreboard(c) => Some(c.name.name.clone()),
            _ => None,
        })
        .collect();
    let buses: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Bus(b) => Some(b.name.name.clone()),
            _ => None,
        })
        .collect();
    let covergroups: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Covergroup(g) => Some(g.name.name.clone()),
            _ => None,
        })
        .collect();
    let regblocks: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Regblock(r) => Some(r.name.name.clone()),
            _ => None,
        })
        .collect();
    let addrmaps: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Addrmap(a) => Some(a.name.name.clone()),
            _ => None,
        })
        .collect();

    // #619 M4a sub-step 4: under `share_lifecycle`, decide PER TESTBENCH
    // whether its lifecycle may be shared. A testbench falls back to the
    // historical per-test inlining (byte-identical to switch-OFF) if any
    // phase/method body is share-unsafe (side-table state) OR any binding
    // test shadows one of its fields. Only shareable testbenches get marker
    // substitution + body capture below; the rest inline verbatim. Empty
    // (nothing shared) when `share_lifecycle` is false.
    let shareable_testbenches: std::collections::HashSet<String> = if share_lifecycle {
        // File-scope `property NAME ... end property` bodies — the exact
        // table lowering's `is_concurrent_assertion` resolves against (see
        // `lower_program`), so the share scan classifies concurrent asserts
        // identically to lowering.
        let properties: std::collections::HashMap<String, Expr> = file
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Property(pd) => Some((pd.name.name.clone(), pd.body.clone())),
                _ => None,
            })
            .fold(std::collections::HashMap::new(), |mut m, (k, v)| {
                m.entry(k).or_insert(v); // first declaration wins, like lowering
                m
            });
        // Testbenches actually bound by an impl-for test.
        let bound: std::collections::HashSet<&str> = file
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Test(t) => t.for_testbench.as_ref().map(|i| i.name.as_str()),
                _ => None,
            })
            .collect();
        bound
            .iter()
            .filter_map(|&name| {
                let tb = components.get(name)?;
                if testbench_lifecycle_share_unsafe(tb, &properties) {
                    return None;
                }
                // Shadow hazard: `rewrite_expr_for_impl` suppresses BOTH
                // field-ref and bare-method-call rewriting via one flat
                // `shadow` set built from every test-scope `let` name. So a
                // binding test whose `let` collides with a testbench FIELD
                // *or* METHOD name gets a different inlined body than the
                // shared (first-bind) one — force fallback in either case.
                let shadow_targets: std::collections::HashSet<&str> = tb
                    .items
                    .iter()
                    .filter_map(|ci| match ci {
                        ComponentItem::Field(f) => Some(f.name.name.as_str()),
                        ComponentItem::Hookable(h) => Some(h.name.name.as_str()),
                        _ => None,
                    })
                    .collect();
                let shadowed = file.items.iter().any(|it| {
                    let Item::Test(t) = it else { return false };
                    if t.for_testbench.as_ref().map(|i| i.name.as_str()) != Some(name) {
                        return false;
                    }
                    t.items.iter().any(|ti| {
                        matches!(ti, TestItem::Let(l) if shadow_targets.contains(l.name.name.as_str()))
                    })
                });
                (!shadowed).then(|| name.to_string())
            })
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let mut out = file.clone();
    let top_item_sources = out.item_sources.clone();
    for (item_index, it) in out.items.iter_mut().enumerate() {
        let Item::Test(t) = it else { continue };
        let test_source = top_item_sources[item_index];
        let Some(tb_ident) = t.for_testbench.clone() else {
            continue;
        };
        let Some(tb) = components.get(&tb_ident.name) else {
            // Bound testbench not found — leave as is; the main
            // pipeline will surface a sensible error when nothing
            // resolves `_tb`'s type.
            continue;
        };
        // #619 M4a sub-step 4: share THIS testbench's lifecycle only if the
        // per-testbench decision above cleared it. Otherwise behave exactly
        // like the historical inlining (no markers, no capture) — the
        // conservative fallback.
        let share_this_tb = share_lifecycle && shareable_testbenches.contains(&tb_ident.name);
        let tb_source = component_sources
            .get(&tb_ident.name)
            .copied()
            .unwrap_or_default();

        // Classify testbench fields.
        let mut dut_field: Option<ComponentField> = None;
        let mut field_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut field_types: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut field_is_pointer: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for ci in &tb.items {
            if let ComponentItem::Field(f) = ci {
                field_names.insert(f.name.name.clone());
                if let TypeExpr::Named { name, .. } = &f.ty {
                    let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                    field_types.insert(f.name.name.clone(), simple.to_string());
                    let is_harc = components.contains_key(simple)
                        || transactors.contains_key(simple)
                        || scoreboards.contains(simple)
                        || buses.contains(simple)
                        || covergroups.contains(simple)
                        || regblocks.contains(simple)
                        || addrmaps.contains(simple);
                    if !is_harc {
                        // Treat as a DUT pointer (Verilator-named SV
                        // module type). First wins for the synthesized
                        // `let dut : ...`.
                        if dut_field.is_none() {
                            dut_field = Some(f.clone());
                        }
                        field_is_pointer.insert(f.name.name.clone());
                    }
                }
            }
        }

        // Method names — both `hookable` and `function` items.
        let mut method_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for ci in &tb.items {
            if let ComponentItem::Hookable(h) = ci {
                method_names.insert(h.name.name.clone());
            }
        }

        let mut tb_lifecycle = ScopeDecl {
            name: Ident {
                name: "sim".into(),
                span: tb_ident.span,
            },
            setup: None,
            run: None,
            check: None,
            teardown: None,
            span: tb_ident.span,
        };
        // Aggregate phase bodies into the synthetic tb_lifecycle
        // ScopeDecl the rest of cpp_tb's lifecycle machinery
        // consumes. With the §7 cleanup each `Lifecycle(phase, body)`
        // node carries exactly one phase, so this loop just routes by
        // phase tag — no field-of-ScopeDecl inspection needed.
        for ci in &tb.items {
            if let ComponentItem::Lifecycle(phase, body) = ci {
                match phase {
                    LifecyclePhase::Setup => tb_lifecycle.setup = Some(body.clone()),
                    LifecyclePhase::Check => tb_lifecycle.check = Some(body.clone()),
                    LifecyclePhase::Teardown => tb_lifecycle.teardown = Some(body.clone()),
                }
                tb_lifecycle.span = body.span;
            }
        }

        // Build the rewriter's "skip set" — names that must NOT be
        // rewritten because they shadow testbench fields at test
        // scope. `dut` always shadows (we synthesize `let dut : ...`
        // for it). Any user-declared `let X` at test scope also
        // shadows — capture them up-front.
        let mut shadow: std::collections::HashSet<String> = std::collections::HashSet::new();
        shadow.insert("dut".into());
        shadow.insert("_tb".into());
        let mut regblock_bindings: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for ti in &t.items {
            if let TestItem::Let(l) = ti {
                shadow.insert(l.name.name.clone());
                if l.bind {
                    if let Some(TypeExpr::Named { name, .. }) = l.ty.as_ref() {
                        if let Some(simple) = name.segments.last().map(|s| s.name.as_str()) {
                            if regblocks.contains(simple) {
                                regblock_bindings.insert(l.name.name.clone());
                            }
                        }
                    }
                }
            }
        }

        // Rewrite every Stmt / Expr in the test body. Bare-statement
        // items (the `impl ... for Tb` form with no `setup`/`run`/
        // `check`/`teardown` scopes) are routed through the same
        // `rewrite_stmts_for_impl` helper as scoped blocks below, so
        // any future rewrite behavior applies to both paths by
        // construction (PR address: code-review finding B on PR
        // arch-hdl-lang/harc-com#306).
        for ti in t.items.iter_mut() {
            match ti {
                TestItem::Stmt(s) => rewrite_stmts_for_impl(
                    std::slice::from_mut(s),
                    &field_names,
                    &method_names,
                    &field_is_pointer,
                    &shadow,
                    &field_types,
                    &transactors,
                    &regblock_bindings,
                ),
                TestItem::Scope(sc) => {
                    if let Some(b) = sc.run.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                            &field_types,
                            &transactors,
                            &regblock_bindings,
                        );
                    }
                    if let Some(b) = sc.setup.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                            &field_types,
                            &transactors,
                            &regblock_bindings,
                        );
                    }
                    if let Some(b) = sc.check.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                            &field_types,
                            &transactors,
                            &regblock_bindings,
                        );
                    }
                    if let Some(b) = sc.teardown.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                            &field_types,
                            &transactors,
                            &regblock_bindings,
                        );
                    }
                }
                TestItem::Phase(_, b) => {
                    rewrite_block_for_impl(
                        b,
                        &field_names,
                        &method_names,
                        &field_is_pointer,
                        &shadow,
                        &field_types,
                        &transactors,
                        &regblock_bindings,
                    );
                }
                _ => {}
            }
        }
        if let Some(b) = tb_lifecycle.setup.as_mut() {
            rewrite_block_for_impl(
                b,
                &field_names,
                &method_names,
                &field_is_pointer,
                &shadow,
                &field_types,
                &transactors,
                &regblock_bindings,
            );
        }
        if let Some(b) = tb_lifecycle.check.as_mut() {
            rewrite_block_for_impl(
                b,
                &field_names,
                &method_names,
                &field_is_pointer,
                &shadow,
                &field_types,
                &transactors,
                &regblock_bindings,
            );
        }
        if let Some(b) = tb_lifecycle.teardown.as_mut() {
            rewrite_block_for_impl(
                b,
                &field_names,
                &method_names,
                &field_is_pointer,
                &shadow,
                &field_types,
                &transactors,
                &regblock_bindings,
            );
        }

        // #619 M4a: under `share_lifecycle`, stash each SHAREABLE bound
        // testbench's now-rewritten (`_tb.<field>`) phase body so TB-IR
        // lowering can lower it ONCE into a `TestbenchLifecycle` function.
        // Keyed by source-testbench name + phase; the FIRST test to bind a
        // given testbench wins (subsequent binds rewrite identically for a
        // shareable testbench — shadowing tests force fallback, so they
        // never reach here). Captured BEFORE the marker substitution below
        // discards the bodies. Only the phases that get a marker are
        // stashed. Skipped entirely for a non-shareable testbench.
        if let Some(cap) = captured.as_deref_mut().filter(|_| share_this_tb) {
            for (phase, body) in [
                (LifecyclePhase::Setup, &tb_lifecycle.setup),
                (LifecyclePhase::Check, &tb_lifecycle.check),
                (LifecyclePhase::Teardown, &tb_lifecycle.teardown),
            ] {
                if let Some(b) = body {
                    cap.entry((tb_ident.name.clone(), phase))
                        .or_insert_with(|| b.clone());
                }
            }
        }

        // Prepend synthesized lets. Order: `let dut : Top` (so the
        // Verilator-init path sees it as a top-level DUT pointer),
        // then `let _tb : TopTb`. Inserted at the head of items so
        // they win over any user-declared lets that happen to shadow
        // (a defensive choice; today shadowing is also a HARC error
        // via duplicate-let detection).
        let mut prefix: Vec<(TestItem, SourceId)> = Vec::new();
        if let Some(dut) = &dut_field {
            // Synthesize: `let dut : <SVType>`
            prefix.push((
                TestItem::Let(LetStmt {
                    name: Ident {
                        name: "dut".into(),
                        span: dut.name.span,
                    },
                    ty: Some(dut.ty.clone()),
                    value: None,
                    bind: false,
                    probes: dut.probes.clone(),
                    bind_remap: dut.bind_remap.clone(),
                    span: dut.span,
                }),
                tb_source,
            ));
        }
        // Synthesize: `let _tb : <TbType>` — default-constructed
        // through the existing component-let path.
        prefix.push((
            TestItem::Let(LetStmt {
                name: Ident {
                    name: "_tb".into(),
                    span: tb_ident.span,
                },
                ty: Some(TypeExpr::Named {
                    name: Path {
                        segments: vec![tb_ident.clone()],
                        span: tb_ident.span,
                    },
                    generics: Vec::new(),
                    mode: None,
                    span: tb_ident.span,
                }),
                value: None,
                bind: false,
                probes: Vec::new(),
                bind_remap: Vec::new(),
                span: tb_ident.span,
            }),
            test_source,
        ));

        // Splice synthesized lets at the head, preserving the rest
        // of the items in original order.
        let original: Vec<TestItem> = std::mem::take(&mut t.items);
        let original_sources = std::mem::take(&mut t.item_sources);
        assert_eq!(original.len(), original_sources.len());
        let has_scope = original.iter().any(|ti| matches!(ti, TestItem::Scope(_)));
        let mut bare_run_stmts: Vec<Stmt> = Vec::new();
        let mut bare_run_sources: Vec<SourceId> = Vec::new();
        (t.items, t.item_sources) = prefix.into_iter().unzip();
        for (ti, item_source) in original.into_iter().zip(original_sources) {
            if !has_scope {
                if let TestItem::Stmt(s) = &ti {
                    bare_run_stmts.push(s.clone());
                    bare_run_sources.push(item_source);
                    continue;
                }
            }
            if let TestItem::Scope(sc) = &ti {
                let mut sc = sc.clone();
                // INVARIANT (load-bearing): the synthesized
                // `_tb.dut = dut` wire MUST precede the first read of
                // `_tb.dut.*` anywhere in the lowered test (any
                // lifecycle block or `run`). We achieve this by
                // always materializing a `setup` block to host the
                // wire as its very first statement — even if neither
                // the testbench nor the impl declared a setup. Setup
                // is by spec the earliest user-visible phase, so the
                // wire is guaranteed to run before any other phase
                // body that could dereference `_tb.dut.*`.
                //
                // If a new phase is later added that runs BEFORE
                // setup, the wire site MUST move with it (or the
                // new phase must be threaded through this same
                // merge). Conditional emission ("only inject if
                // setup already exists") was the previous shape;
                // that made the invariant implicit and order-
                // sensitive — silent zero-reads on regression.
                let wire_stmt = dut_field
                    .as_ref()
                    .map(|_| (make_wire_dut_stmt(tb_ident.span), test_source));

                // Under `share_lifecycle` (#619 M4a) each testbench
                // lifecycle body is replaced by a single marker call,
                // leaving the wire and impl-owned statements in place;
                // otherwise the historical inlining substitutes the
                // body verbatim.
                let tb_phase = |body: &Option<Block>, phase: LifecyclePhase| -> Option<Block> {
                    if share_this_tb {
                        body.as_ref()
                            .map(|_| tb_lifecycle_marker_block(phase, tb_ident.span))
                    } else {
                        body.clone()
                    }
                };
                sc.setup = merge_lifecycle_blocks(
                    tb_phase(&tb_lifecycle.setup, LifecyclePhase::Setup),
                    sc.setup.clone(),
                    wire_stmt,
                    tb_ident.span,
                );
                sc.check = merge_lifecycle_blocks(
                    tb_phase(&tb_lifecycle.check, LifecyclePhase::Check),
                    sc.check.clone(),
                    None,
                    tb_ident.span,
                );
                sc.teardown = merge_lifecycle_blocks(
                    sc.teardown.clone(),
                    tb_phase(&tb_lifecycle.teardown, LifecyclePhase::Teardown),
                    None,
                    tb_ident.span,
                );
                t.items.push(TestItem::Scope(sc));
                t.item_sources.push(item_source);
                continue;
            }
            t.items.push(ti);
            t.item_sources.push(item_source);
        }
        if !bare_run_stmts.is_empty() {
            // Same invariant as the scoped branch: always materialize
            // a setup block to host the `_tb.dut = dut` wire as its
            // first statement. See the scoped-branch comment above
            // for the load-bearing rationale.
            let wire_stmt = dut_field
                .as_ref()
                .map(|_| (make_wire_dut_stmt(tb_ident.span), test_source));
            // Under `share_lifecycle` (#619 M4a) each testbench lifecycle
            // body becomes a marker call; see the scoped-branch comment.
            let tb_phase = |body: &Option<Block>, phase: LifecyclePhase| -> Option<Block> {
                if share_this_tb {
                    body.as_ref()
                        .map(|_| tb_lifecycle_marker_block(phase, tb_ident.span))
                } else {
                    body.clone()
                }
            };
            let setup = merge_lifecycle_blocks(
                tb_phase(&tb_lifecycle.setup, LifecyclePhase::Setup),
                None,
                wire_stmt,
                tb_ident.span,
            );
            let run_stmts = bare_run_stmts;
            let run_source = bare_run_sources.first().copied().unwrap_or_default();
            t.items.push(TestItem::Scope(ScopeDecl {
                name: Ident {
                    name: "sim".into(),
                    span: tb_ident.span,
                },
                setup,
                run: Some(Block::with_sources(
                    run_stmts,
                    bare_run_sources,
                    tb_ident.span,
                )),
                check: tb_phase(&tb_lifecycle.check, LifecyclePhase::Check),
                teardown: tb_phase(&tb_lifecycle.teardown, LifecyclePhase::Teardown),
                span: tb_ident.span,
            }));
            t.item_sources.push(run_source);
        }

        assert_eq!(t.items.len(), t.item_sources.len());

        // Mark as desugared so any downstream consumer (pretty-
        // printer for diagnostics, etc.) sees the classic shape.
        t.for_testbench = None;
    }
    std::borrow::Cow::Owned(out)
}

fn merge_lifecycle_blocks(
    first: Option<Block>,
    second: Option<Block>,
    prefix_stmt: Option<(Stmt, SourceId)>,
    fallback_span: Span,
) -> Option<Block> {
    if first.is_none() && second.is_none() && prefix_stmt.is_none() {
        return None;
    }
    let mut stmts = Vec::new();
    let mut stmt_sources = Vec::new();
    if let Some((s, source)) = prefix_stmt {
        stmts.push(s);
        stmt_sources.push(source);
    }
    let mut span = fallback_span;
    if let Some(b) = first {
        span = b.span;
        stmt_sources.extend(b.stmt_sources.iter().copied());
        stmts.extend(b.stmts);
    }
    if let Some(b) = second {
        span = b.span;
        stmt_sources.extend(b.stmt_sources.iter().copied());
        stmts.extend(b.stmts);
    }
    Some(Block::with_sources(stmts, stmt_sources, span))
}

/// Build the synthetic `_tb.dut = dut` statement that wires the
/// testbench's DUT pointer to the test-scope `let dut : ...`. Same
/// shape as a user-written assignment, so it threads through
/// `emit_stmt` unchanged.
fn make_wire_dut_stmt(span: Span) -> Stmt {
    let _tb = Expr {
        kind: Box::new(ExprKind::Ident(Ident {
            name: "_tb".into(),
            span,
        })),
        span,
    };
    let _tb_dut = Expr {
        kind: Box::new(ExprKind::Field {
            target: _tb,
            name: Ident {
                name: "dut".into(),
                span,
            },
        }),
        span,
    };
    let dut = Expr {
        kind: Box::new(ExprKind::Ident(Ident {
            name: "dut".into(),
            span,
        })),
        span,
    };
    Stmt {
        kind: StmtKind::Assign {
            target: _tb_dut,
            value: dut,
        },
        span,
    }
}

#[cfg(test)]
mod impl_desugar_source_tests {
    use super::*;

    #[test]
    fn synthesized_items_pair_each_span_with_its_own_source() {
        let tb_file = crate::parser::parse_source_named(
            "probe_tb.harc",
            r#"testbench ProbeTb
    let dut : Top
        probe value : uint<8> at core.value
    end let dut
end testbench ProbeTb"#,
        )
        .unwrap();
        let impl_file = crate::parser::parse_source_named(
            "probe_test.harc",
            r#"impl ProbeTest for ProbeTb
    run
        wait 1 cycle
    end run
end impl ProbeTest"#,
        )
        .unwrap();
        let tb_source = tb_file.item_source(0);
        let impl_source = impl_file.item_source(0);
        let dut_span = match &tb_file.items[0] {
            Item::Env(tb) if tb.kind == ComponentKind::Testbench => match &tb.items[0] {
                ComponentItem::Field(field) => field.span,
                other => panic!("expected DUT field, got {other:?}"),
            },
            other => panic!("expected testbench, got {other:?}"),
        };
        let impl_span = match &impl_file.items[0] {
            Item::Test(test) => test.for_testbench.as_ref().unwrap().span,
            other => panic!("expected impl test, got {other:?}"),
        };
        let merged = crate::codegen::merge::merge_for_sim(vec![impl_file, tb_file], None).unwrap();
        let desugared = desugar_impl_for_test_in_file(&merged);
        let test = desugared
            .items
            .iter()
            .find_map(|item| match item {
                Item::Test(test) => Some(test),
                _ => None,
            })
            .unwrap();

        let TestItem::Let(dut) = &test.items[0] else {
            panic!("first synthetic item is not the DUT binding")
        };
        assert_eq!(test.item_source(0), tb_source);
        assert_eq!(dut.span, dut_span);

        let TestItem::Let(tb) = &test.items[1] else {
            panic!("second synthetic item is not the testbench binding")
        };
        assert_eq!(test.item_source(1), impl_source);
        assert_eq!(tb.span, impl_span);

        let scope = test.items.iter().find_map(|item| match item {
            TestItem::Scope(scope) => Some(scope),
            _ => None,
        });
        let setup = scope.unwrap().setup.as_ref().unwrap();
        assert_eq!(setup.stmt_source(0), impl_source);
        assert_eq!(setup.stmts[0].span, impl_span);
    }
}

/// Walk a block and rewrite bare-ident references that match a
/// testbench field or method to `_tb.<name>`. See
/// `desugar_impl_for_test_in_file` for the rewrite rules.
fn rewrite_block_for_impl(
    b: &mut Block,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
    field_types: &std::collections::HashMap<String, String>,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    regblock_bindings: &std::collections::HashSet<String>,
) {
    rewrite_stmts_for_impl(
        &mut b.stmts,
        fields,
        methods,
        pointers,
        shadow,
        field_types,
        transactors,
        regblock_bindings,
    );
}

/// Rewrite a testbench-scoped body (an `on ... end on` handler body or a
/// testbench helper-method body) so bare references to the testbench's
/// own fields/methods become `_tb.<name>`, matching the impl-for
/// desugaring already applied to the bound test body. Used by the TB-IR
/// lowering (issue #485): unlike v1 it has no testbench component to
/// resolve bare names against, so it reuses this AST rewrite before
/// lowering the body with the ordinary test-scope context. `extra_shadow`
/// names (e.g. a helper method's own parameters, or the handler's own
/// locals) are never rewritten; `dut` and `_tb` are always shadowed.
pub(crate) fn rewrite_testbench_scope_body(
    body: &mut Block,
    tb: &ComponentDecl,
    extra_shadow: &std::collections::HashSet<String>,
) {
    let mut fields: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut methods: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ci in &tb.items {
        match ci {
            ComponentItem::Field(f) => {
                fields.insert(f.name.name.clone());
            }
            ComponentItem::Hookable(h) => {
                methods.insert(h.name.name.clone());
            }
            _ => {}
        }
    }
    let mut shadow: std::collections::HashSet<String> =
        ["dut".to_string(), "_tb".to_string()].into_iter().collect();
    shadow.extend(extra_shadow.iter().cloned());
    // The `pointers` set only affected a legacy DUT-deref path that the
    // expr rewriter no longer consults; pass an empty set.
    let pointers = std::collections::HashSet::new();
    let field_types = std::collections::HashMap::new();
    let transactors = std::collections::HashMap::new();
    let regblock_bindings = std::collections::HashSet::new();
    rewrite_block_for_impl(
        body,
        &fields,
        &methods,
        &pointers,
        &shadow,
        &field_types,
        &transactors,
        &regblock_bindings,
    );
}

/// Statement-list variant of `rewrite_block_for_impl`. Both the
/// scoped form (per-`Block` walk) and the bare-statement form
/// (`impl ... for Tb` with a flat `Stmt` list, no `setup`/`run`/
/// `check`/`teardown` scopes) share this helper so that bare bodies
/// can never structurally diverge from scoped bodies. New rewrite
/// behavior added here applies to both paths by construction.
fn rewrite_stmts_for_impl(
    stmts: &mut [Stmt],
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
    field_types: &std::collections::HashMap<String, String>,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    regblock_bindings: &std::collections::HashSet<String>,
) {
    for s in stmts.iter_mut() {
        rewrite_stmt_for_impl(
            s,
            fields,
            methods,
            pointers,
            shadow,
            field_types,
            transactors,
            regblock_bindings,
        );
    }
}

fn rewrite_stmt_for_impl(
    s: &mut Stmt,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
    field_types: &std::collections::HashMap<String, String>,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    regblock_bindings: &std::collections::HashSet<String>,
) {
    match &mut s.kind {
        StmtKind::Let(l) => {
            if let Some(v) = l.value.as_mut() {
                rewrite_expr_for_impl(v, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
            rewrite_expr_for_impl(target, fields, methods, pointers, shadow);
            rewrite_expr_for_impl(value, fields, methods, pointers, shadow);
        }
        StmtKind::Expr(e) => rewrite_expr_for_impl(e, fields, methods, pointers, shadow),
        StmtKind::For(f) => {
            rewrite_expr_for_impl(&mut f.iter, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                &mut f.body,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::Repeat(r) => {
            rewrite_expr_for_impl(&mut r.count, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                &mut r.body,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::Loop(b) => rewrite_block_for_impl(
            b,
            fields,
            methods,
            pointers,
            shadow,
            field_types,
            transactors,
            regblock_bindings,
        ),
        StmtKind::While { cond, body, .. } => {
            rewrite_expr_for_impl(cond, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                body,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::If(ifs) => {
            rewrite_expr_for_impl(&mut ifs.cond, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                &mut ifs.then_block,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
            for (c, b) in ifs.elsifs.iter_mut() {
                rewrite_expr_for_impl(c, fields, methods, pointers, shadow);
                rewrite_block_for_impl(
                    b,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
            if let Some(b) = ifs.else_block.as_mut() {
                rewrite_block_for_impl(
                    b,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
        }
        StmtKind::Fork(fk) => {
            for b in fk.branches.iter_mut() {
                rewrite_block_for_impl(
                    b,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
        }
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => {
            for b in blocks.iter_mut() {
                rewrite_block_for_impl(
                    b,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
        }
        StmtKind::Select(arms) => {
            for a in arms.iter_mut() {
                rewrite_expr_for_impl(&mut a.event, fields, methods, pointers, shadow);
                rewrite_block_for_impl(
                    &mut a.action,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
        }
        StmtKind::On(h) => {
            let mut body_shadow = shadow.clone();
            if let Some(params) =
                impl_on_handler_param_names(h, field_types, transactors, regblock_bindings)
            {
                body_shadow.extend(params);
            }
            rewrite_expr_for_impl(&mut h.event, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                &mut h.body,
                fields,
                methods,
                pointers,
                &body_shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::After { duration, body, .. } => {
            rewrite_expr_for_impl(duration, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                body,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::Wait { duration, .. } => {
            rewrite_expr_for_impl(duration, fields, methods, pointers, shadow);
        }
        StmtKind::WaitUntil {
            conditions,
            timeout,
            ..
        } => {
            for c in conditions.iter_mut() {
                rewrite_expr_for_impl(c, fields, methods, pointers, shadow);
            }
            if let Some(t) = timeout.as_mut() {
                rewrite_expr_for_impl(&mut t.cycles, fields, methods, pointers, shadow);
                if let Some(m) = t.message.as_mut() {
                    rewrite_expr_for_impl(m, fields, methods, pointers, shadow);
                }
            }
        }
        StmtKind::Yield(e) | StmtKind::Release(e) => {
            rewrite_expr_for_impl(e, fields, methods, pointers, shadow);
        }
        StmtKind::Return(opt) => {
            if let Some(e) = opt.as_mut() {
                rewrite_expr_for_impl(e, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Assert(v) | StmtKind::Assume(v) | StmtKind::Cover(v) => {
            if let Some(ex) = v.expr.as_mut() {
                rewrite_expr_for_impl(ex, fields, methods, pointers, shadow);
            }
            if let Some(else_fail) = v.else_fail.as_mut() {
                rewrite_expr_for_impl(else_fail, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Randomize {
            target, with_body, ..
        } => {
            rewrite_expr_for_impl(target, fields, methods, pointers, shadow);
            for e in with_body.iter_mut() {
                rewrite_expr_for_impl(e, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Log { args, .. } | StmtKind::LogF { args, .. } => {
            for a in args.iter_mut() {
                match a {
                    CallArg::Expr(e) => rewrite_expr_for_impl(e, fields, methods, pointers, shadow),
                    CallArg::Named { value, .. } => {
                        rewrite_expr_for_impl(value, fields, methods, pointers, shadow);
                    }
                }
            }
        }
        StmtKind::Emit { name, args, .. } => {
            // Multi-segment `emit X.Y(t)` lowers verbatim as
            // `X.Y(...)` in the codegen at StmtKind::Emit handling
            // (see lower_emit). If the head segment is a testbench
            // field, prepend `_tb` so the lowered C++ resolves
            // through the testbench struct.
            if let Some(head) = name.segments.first() {
                if fields.contains(&head.name) && !shadow.contains(&head.name) {
                    name.segments.insert(
                        0,
                        Ident {
                            name: "_tb".into(),
                            span: head.span,
                        },
                    );
                }
            }
            for a in args.iter_mut() {
                match a {
                    CallArg::Expr(e) => rewrite_expr_for_impl(e, fields, methods, pointers, shadow),
                    CallArg::Named { value, .. } => {
                        rewrite_expr_for_impl(value, fields, methods, pointers, shadow);
                    }
                }
            }
        }
        StmtKind::Fail { msg, .. } => {
            rewrite_expr_for_impl(msg, fields, methods, pointers, shadow);
        }
        StmtKind::JoinAll { .. }
        | StmtKind::Apply(_)
        | StmtKind::Break { .. }
        | StmtKind::Continue { .. } => {}
    }
}

fn impl_on_handler_param_names(
    h: &OnHandler,
    field_types: &std::collections::HashMap<String, String>,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    regblock_bindings: &std::collections::HashSet<String>,
) -> Option<Vec<String>> {
    if h.hook.is_some() {
        let ExprKind::Field {
            target,
            name: method,
        } = &*h.event.kind
        else {
            return None;
        };
        let field = match &*target.kind {
            ExprKind::Ident(id) => id.name.as_str(),
            ExprKind::Field { target, name } => {
                let ExprKind::Ident(root) = &*target.kind else {
                    return None;
                };
                if root.name != "_tb" {
                    return None;
                }
                name.name.as_str()
            }
            _ => return None,
        };
        let transactor_ty = field_types.get(field)?;
        let transactor = transactors.get(transactor_ty)?;
        let method_decl = transactor
            .items
            .iter()
            .chain(transactor.when_active.iter().flatten())
            .find_map(|item| match item {
                ComponentItem::Hookable(m) if m.name.name == method.name => Some(m),
                _ => None,
            })?;
        return Some(
            method_decl
                .params
                .iter()
                .map(|p| p.name.name.clone())
                .collect(),
        );
    }

    // Per-register RAL callbacks (`on regs.REG`) see an implicit scalar
    // parameter named `data`. Match the lowering collector's resolved
    // subset by requiring the root ident to be a known test-scope
    // regblock binding; ordinary cycle triggers like `on dut.ready`
    // must not suppress testbench-field rewriting for a field named
    // `data`.
    if !h.periodic {
        if let ExprKind::Field { target, .. } = &*h.event.kind {
            if let ExprKind::Ident(binding) = &*target.kind {
                if !regblock_bindings.contains(&binding.name) {
                    return None;
                }
                return Some(vec!["data".to_string()]);
            }
        }
    }
    None
}

fn rewrite_expr_for_impl(
    e: &mut Expr,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    _pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
) {
    match e.kind.as_mut() {
        ExprKind::Ident(id) => {
            // Bare ident matching a testbench field (non-shadowed)
            // → `_tb.<id>`.
            if fields.contains(&id.name) && !shadow.contains(&id.name) {
                let new_id = id.clone();
                let span = e.span;
                let inner = Expr {
                    kind: Box::new(ExprKind::Ident(Ident {
                        name: "_tb".into(),
                        span,
                    })),
                    span,
                };
                *e.kind = ExprKind::Field {
                    target: inner,
                    name: new_id,
                };
            }
        }
        ExprKind::Call { callee, args } => {
            // Bare-name method call `<m>(args)` where `<m>` is a
            // testbench helper (function/hookable) and not shadowed
            // → rewrite callee to `_tb.<m>` so the existing
            // `resolve_component_method_call` dispatcher picks it up.
            if let ExprKind::Ident(id) = callee.kind.as_ref() {
                if methods.contains(&id.name) && !shadow.contains(&id.name) {
                    let new_id = id.clone();
                    let span = callee.span;
                    let inner = Expr {
                        kind: Box::new(ExprKind::Ident(Ident {
                            name: "_tb".into(),
                            span,
                        })),
                        span,
                    };
                    *callee.kind = ExprKind::Field {
                        target: inner,
                        name: new_id,
                    };
                }
            } else {
                rewrite_expr_for_impl(callee, fields, methods, _pointers, shadow);
            }
            for a in args.iter_mut() {
                match a {
                    CallArg::Expr(x) => {
                        rewrite_expr_for_impl(x, fields, methods, _pointers, shadow)
                    }
                    CallArg::Named { value, .. } => {
                        rewrite_expr_for_impl(value, fields, methods, _pointers, shadow);
                    }
                }
            }
        }
        ExprKind::Field { target, .. } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
        }
        ExprKind::Index { target, index } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(index, fields, methods, _pointers, shadow);
        }
        ExprKind::BitSlice { target, hi, lo } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(hi, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(lo, fields, methods, _pointers, shadow);
        }
        ExprKind::Cast { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::Send { target, value } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(value, fields, methods, _pointers, shadow);
        }
        ExprKind::Unary { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::ForkCall { call } => {
            rewrite_expr_for_impl(call, fields, methods, _pointers, shadow)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            rewrite_expr_for_impl(lhs, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(rhs, fields, methods, _pointers, shadow);
        }
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            rewrite_expr_for_impl(cond, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(then_branch, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(else_branch, fields, methods, _pointers, shadow);
        }
        ExprKind::HashHash { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::SeqRepeat { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::RangeLit { lo, hi } => {
            if let Some(lo) = lo.as_mut() {
                rewrite_expr_for_impl(lo, fields, methods, _pointers, shadow);
            }
            if let Some(hi) = hi.as_mut() {
                rewrite_expr_for_impl(hi, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::SetLit(es) => {
            for x in es.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::SystemCall { args, .. } => {
            for x in args.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::Randomize {
            target, with_body, ..
        } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            for x in with_body.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::ForEachConstraint { var, iter, body } => {
            rewrite_expr_for_impl(iter, fields, methods, _pointers, shadow);
            let mut inner_shadow = shadow.clone();
            inner_shadow.insert(var.name.clone());
            for x in body.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, &inner_shadow);
            }
        }
        ExprKind::SoftConstraint(sc) => {
            rewrite_expr_for_impl(&mut sc.expr, fields, methods, _pointers, shadow);
            if let Some(weight) = sc.weight.as_mut() {
                rewrite_expr_for_impl(weight, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::DistDirective { target, .. } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
        }
        ExprKind::Paren(x) => rewrite_expr_for_impl(x, fields, methods, _pointers, shadow),
        ExprKind::NamedArg { value, .. } => {
            rewrite_expr_for_impl(value, fields, methods, _pointers, shadow);
        }
        ExprKind::StructLit { fields: nfs, .. } => {
            for nf in nfs.iter_mut() {
                rewrite_expr_for_impl(&mut nf.value, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::CoverArrow { lhs, rhs, .. } => {
            rewrite_expr_for_impl(lhs, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(rhs, fields, methods, _pointers, shadow);
        }
        ExprKind::SolveOrder { args } => {
            for x in args.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::Membership { expr, set } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(set, fields, methods, _pointers, shadow);
        }
        ExprKind::String(s) => {
            // Rewrite testbench-field references inside `${...}`
            // interpolations. Without this, `fail("count = ${env.sb}")`
            // emits raw `env.sb` after the desugarer has already
            // rewritten the rest of the AST, producing a compile-time
            // "undeclared identifier" in C++. Textual rewrite is
            // safe — `${...}` spans don't nest in v0.
            *s = rewrite_interp_in_string(s, fields, methods, shadow);
        }
        ExprKind::DistLit(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Time(_)
        | ExprKind::Bool(_)
        | ExprKind::ImplicitSelf => {}
    }
}

/// Scan a string literal for `${...}` interpolation segments and
/// prepend `_tb.` to any segment whose leading identifier matches
/// a testbench field (or method that's been folded into scope) and
/// isn't shadowed. Operates textually so it can't get confused by
/// quote-escaping nuances inside the expression — `${...}` doesn't
/// nest in v0.
fn rewrite_interp_in_string(
    s: &str,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for the start of a `${` interpolation (not preceded by
        // a backslash). v0 doesn't ship a fancier escape grammar, so
        // a bare `${` is always an interpolation.
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            out.push_str("${");
            i += 2;
            // Capture up to the matching `}`.
            let start = i;
            while i < bytes.len() && bytes[i] != b'}' {
                i += 1;
            }
            let segment = &s[start..i];
            // Extract the leading identifier (before any `.`, `:`,
            // `[`, or whitespace). The format-spec suffix `:<fmt>` is
            // preserved unchanged so widthhex / decimal hints
            // continue to work.
            let head_end = segment
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(segment.len());
            let head = &segment[..head_end];
            if !head.is_empty()
                && (fields.contains(head) || methods.contains(head))
                && !shadow.contains(head)
            {
                out.push_str("_tb.");
            }
            out.push_str(segment);
            // Consume the closing `}` (if present — malformed
            // interpolations pass through untouched).
            if i < bytes.len() {
                out.push('}');
                i += 1;
            }
        } else {
            out.push(s.as_bytes()[i] as char);
            i += 1;
        }
    }
    out
}
