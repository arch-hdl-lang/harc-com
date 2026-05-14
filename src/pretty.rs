//! HARC pretty-printer.
//!
//! Round-trip target — output should re-parse to a structurally-equivalent AST
//! (modulo whitespace and formatting). Indentation is 4 spaces, matching ARCH.

use crate::ast::*;
use std::fmt::Write;

const INDENT: &str = "    ";

pub fn print(f: &SourceFile) -> String {
    let mut out = String::new();
    if let Some(d) = &f.inner_doc {
        for line in d.split('\n') {
            writeln!(out, "//! {line}").ok();
        }
        if !f.items.is_empty() {
            writeln!(out).ok();
        }
    }
    for (i, item) in f.items.iter().enumerate() {
        if i > 0 {
            writeln!(out).ok();
        }
        print_item(&mut out, item, 0);
    }
    out
}

fn pad(out: &mut String, depth: usize) {
    for _ in 0..depth { out.push_str(INDENT); }
}

fn print_doc(out: &mut String, doc: &Option<String>, depth: usize) {
    if let Some(d) = doc {
        for line in d.split('\n') {
            pad(out, depth);
            writeln!(out, "/// {line}").ok();
        }
    }
}

/// Print a per-construct `inner_doc` block — `//!` lines that came in
/// after the opening keyword + name and before the first body item.
/// Round-trips against `consume_inner_doc` in the parser.
fn print_inner_doc(out: &mut String, doc: &Option<String>, depth: usize) {
    if let Some(d) = doc {
        for line in d.split('\n') {
            pad(out, depth);
            writeln!(out, "//! {line}").ok();
        }
    }
}

fn print_item(out: &mut String, item: &Item, depth: usize) {
    match item {
        Item::Use(u) => {
            print_doc(out, &u.doc, depth);
            pad(out, depth);
            write!(out, "use ").ok();
            print_path(out, &u.path);
            writeln!(out).ok();
        }
        Item::Package(p) => {
            print_doc(out, &p.doc, depth);
            pad(out, depth);
            writeln!(out, "package {}", p.name.name).ok();
            print_inner_doc(out, &p.inner_doc, depth + 1);
            for (i, it) in p.items.iter().enumerate() {
                if i > 0 { writeln!(out).ok(); }
                print_item(out, it, depth + 1);
            }
            pad(out, depth);
            writeln!(out, "end package {}", p.name.name).ok();
        }
        Item::Domain(d) => {
            print_doc(out, &d.doc, depth);
            pad(out, depth);
            writeln!(out, "domain {}", d.name.name).ok();
            for f in &d.fields {
                pad(out, depth + 1);
                write!(out, "{}: ", f.name.name).ok();
                print_expr(out, &f.value);
                writeln!(out).ok();
            }
            pad(out, depth);
            writeln!(out, "end domain {}", d.name.name).ok();
        }
        Item::Const(c) => {
            print_doc(out, &c.doc, depth);
            pad(out, depth);
            write!(out, "const {}", c.name.name).ok();
            if let Some(t) = &c.ty {
                write!(out, " : ").ok();
                print_type(out, t);
            }
            write!(out, " = ").ok();
            print_expr(out, &c.value);
            writeln!(out).ok();
        }
        Item::Struct(s) => {
            print_doc(out, &s.doc, depth);
            pad(out, depth);
            writeln!(out, "struct {}", s.name.name).ok();
            print_inner_doc(out, &s.inner_doc, depth + 1);
            for f in &s.fields {
                print_field(out, f, depth + 1);
            }
            pad(out, depth);
            writeln!(out, "end struct {}", s.name.name).ok();
        }
        Item::Enum(e) => {
            print_doc(out, &e.doc, depth);
            pad(out, depth);
            write!(out, "enum {} {{ ", e.name.name).ok();
            for (i, v) in e.variants.iter().enumerate() {
                if i > 0 { write!(out, ", ").ok(); }
                write!(out, "{}", v.name).ok();
            }
            writeln!(out, " }}").ok();
        }
        Item::Transaction(t) => {
            print_doc(out, &t.doc, depth);
            pad(out, depth);
            write!(out, "transaction {}", t.name.name).ok();
            print_generic_params(out, &t.params);
            writeln!(out).ok();
            print_inner_doc(out, &t.inner_doc, depth + 1);
            for it in &t.body {
                print_txn_body_item(out, it, depth + 1);
            }
            pad(out, depth);
            writeln!(out, "end transaction {}", t.name.name).ok();
        }
        Item::Relation(r) => {
            print_doc(out, &r.doc, depth);
            pad(out, depth);
            write!(out, "relation {}", r.name.name).ok();
            print_paren_params(out, &r.params);
            match &r.body {
                RelationBody::Alias(e) => {
                    write!(out, " = ").ok();
                    print_expr(out, e);
                    writeln!(out).ok();
                }
                RelationBody::Block(es) => {
                    writeln!(out).ok();
                    for e in es {
                        pad(out, depth + 1);
                        print_expr(out, e);
                        writeln!(out).ok();
                    }
                    pad(out, depth);
                    writeln!(out, "end relation {}", r.name.name).ok();
                }
            }
        }
        Item::Tseq(t) => {
            print_doc(out, &t.doc, depth);
            pad(out, depth);
            write!(out, "tseq {}", t.name.name).ok();
            if !t.params.is_empty() {
                print_paren_params(out, &t.params);
            }
            if let Some(rt) = &t.return_ty {
                write!(out, " -> ").ok();
                print_type(out, rt);
            }
            writeln!(out).ok();
            print_inner_doc(out, &t.inner_doc, depth + 1);
            print_block_inner(out, &t.body, depth + 1);
            pad(out, depth);
            writeln!(out, "end tseq {}", t.name.name).ok();
        }
        Item::Agent(c) | Item::Env(c) | Item::Scoreboard(c) | Item::Sequencer(c) => {
            print_component(out, c, depth);
        }
        Item::Impl(i) => {
            print_doc(out, &i.doc, depth);
            pad(out, depth);
            writeln!(out, "impl {} for {}", i.target.name, i.test_name.name).ok();
            print_inner_doc(out, &i.inner_doc, depth + 1);
            for it in &i.items {
                match it {
                    ImplItem::Setup(b) => {
                        pad(out, depth + 1);
                        writeln!(out, "setup").ok();
                        print_block_inner(out, b, depth + 2);
                        pad(out, depth + 1);
                        writeln!(out, "end setup").ok();
                    }
                    ImplItem::Run(b) => {
                        pad(out, depth + 1);
                        writeln!(out, "run").ok();
                        print_block_inner(out, b, depth + 2);
                        pad(out, depth + 1);
                        writeln!(out, "end run").ok();
                    }
                    ImplItem::Check(b) => {
                        pad(out, depth + 1);
                        writeln!(out, "check").ok();
                        print_block_inner(out, b, depth + 2);
                        pad(out, depth + 1);
                        writeln!(out, "end check").ok();
                    }
                    ImplItem::Teardown(b) => {
                        pad(out, depth + 1);
                        writeln!(out, "teardown").ok();
                        print_block_inner(out, b, depth + 2);
                        pad(out, depth + 1);
                        writeln!(out, "end teardown").ok();
                    }
                    ImplItem::Phase(name, b) => {
                        pad(out, depth + 1);
                        writeln!(out, "phase {}", name.name).ok();
                        print_block_inner(out, b, depth + 2);
                        pad(out, depth + 1);
                        writeln!(out, "end phase {}", name.name).ok();
                    }
                }
            }
            pad(out, depth);
            writeln!(out, "end impl {}", i.test_name.name).ok();
        }
        Item::Test(t) => {
            print_doc(out, &t.doc, depth);
            pad(out, depth);
            write!(out, "test {}", t.name.name).ok();
            if !t.params.is_empty() {
                print_paren_params(out, &t.params);
            }
            writeln!(out).ok();
            print_inner_doc(out, &t.inner_doc, depth + 1);
            for it in &t.items {
                print_test_item(out, it, depth + 1);
            }
            pad(out, depth);
            writeln!(out, "end test {}", t.name.name).ok();
        }
        Item::Extend(e) => {
            print_doc(out, &e.doc, depth);
            pad(out, depth);
            write!(out, "extend ").ok();
            print_path(out, &e.target);
            writeln!(out).ok();
            print_inner_doc(out, &e.inner_doc, depth + 1);
            match &e.body {
                ExtendBody::TxnLike(items) => {
                    for it in items {
                        print_txn_body_item(out, it, depth + 1);
                    }
                }
                ExtendBody::Component(items) => {
                    for it in items {
                        print_component_item(out, it, depth + 1);
                    }
                }
                ExtendBody::Test(items) => {
                    for it in items {
                        print_test_item(out, it, depth + 1);
                    }
                }
            }
            pad(out, depth);
            write!(out, "end extend ").ok();
            print_path(out, &e.target);
            writeln!(out).ok();
        }
        Item::Covergroup(g) => {
            print_doc(out, &g.doc, depth);
            pad(out, depth);
            write!(out, "covergroup {}", g.name.name).ok();
            if let Some(c) = &g.clocking {
                write!(out, " @(").ok();
                print_clocking_expr(out, c);
                write!(out, ")").ok();
            }
            writeln!(out).ok();
            print_inner_doc(out, &g.inner_doc, depth + 1);
            for it in &g.items {
                print_cover_item(out, it, depth + 1);
            }
            pad(out, depth);
            writeln!(out, "end covergroup {}", g.name.name).ok();
        }
        Item::Property(p) => {
            print_doc(out, &p.doc, depth);
            pad(out, depth);
            write!(out, "property {}", p.name.name).ok();
            if !p.params.is_empty() {
                print_paren_params(out, &p.params);
            }
            writeln!(out).ok();
            print_inner_doc(out, &p.inner_doc, depth + 1);
            pad(out, depth + 1);
            print_expr(out, &p.body);
            writeln!(out).ok();
            pad(out, depth);
            writeln!(out, "end property {}", p.name.name).ok();
        }
        Item::Pseq(p) => {
            print_doc(out, &p.doc, depth);
            pad(out, depth);
            write!(out, "pseq {}", p.name.name).ok();
            if !p.params.is_empty() {
                print_paren_params(out, &p.params);
            }
            writeln!(out).ok();
            print_inner_doc(out, &p.inner_doc, depth + 1);
            pad(out, depth + 1);
            print_expr(out, &p.body);
            writeln!(out).ok();
            pad(out, depth);
            writeln!(out, "end pseq {}", p.name.name).ok();
        }
        Item::CoverSequence(c) => {
            // `cover sequence Name = expr` is a single-line construct.
            // When `inner_doc` is present, split onto a multi-line form
            // (`cover sequence Name\n  //! ...\n  = expr`) so the
            // parser can re-attach the `//!` to its `consume_inner_doc()`
            // slot between Name and `=`.
            print_doc(out, &c.doc, depth);
            if c.inner_doc.is_some() {
                pad(out, depth);
                writeln!(out, "cover sequence {}", c.name.name).ok();
                print_inner_doc(out, &c.inner_doc, depth + 1);
                pad(out, depth + 1);
                write!(out, "= ").ok();
                print_expr(out, &c.pattern);
                writeln!(out).ok();
            } else {
                pad(out, depth);
                write!(out, "cover sequence {} = ", c.name.name).ok();
                print_expr(out, &c.pattern);
                writeln!(out).ok();
            }
        }
        Item::ExternalModule(m) => {
            print_doc(out, &m.doc, depth);
            pad(out, depth);
            writeln!(out, "module {} kind {}", m.name.name, m.kind.name).ok();
            for f in &m.fields {
                pad(out, depth + 1);
                write!(out, "{}: ", f.name.name).ok();
                print_expr(out, &f.value);
                writeln!(out).ok();
            }
            pad(out, depth);
            writeln!(out, "end module {}", m.name.name).ok();
        }
        Item::Function(f) => {
            print_doc(out, &f.doc, depth);
            pad(out, depth);
            write!(out, "function {}", f.name.name).ok();
            print_paren_params(out, &f.params);
            if let Some(rt) = &f.return_ty {
                write!(out, " -> ").ok();
                print_type(out, rt);
            }
            writeln!(out).ok();
            print_inner_doc(out, &f.inner_doc, depth + 1);
            print_block_inner(out, &f.body, depth + 1);
            pad(out, depth);
            writeln!(out, "end function {}", f.name.name).ok();
        }
        Item::ExternFn(f) => {
            print_doc(out, &f.doc, depth);
            pad(out, depth);
            write!(out, "extern function {}", f.name.name).ok();
            print_paren_params(out, &f.params);
            if let Some(rt) = &f.return_ty {
                write!(out, " -> ").ok();
                print_type(out, rt);
            }
            writeln!(out).ok();
        }
        Item::Apply(a) => {
            pad(out, depth);
            write!(out, "apply ").ok();
            print_path(out, &a.path);
            writeln!(out).ok();
        }
        Item::Bus(b) => {
            print_doc(out, &b.doc, depth);
            pad(out, depth);
            write!(out, "bus {}", b.name.name).ok();
            if !b.params.is_empty() {
                print_paren_params(out, &b.params);
            }
            writeln!(out).ok();
            print_inner_doc(out, &b.inner_doc, depth + 1);
            for s in &b.signals {
                pad(out, depth + 1);
                let dir = match s.direction {
                    Direction::In => "in",
                    Direction::Out => "out",
                    Direction::InOut => "inout",
                };
                write!(out, "{}: {} ", s.name.name, dir).ok();
                print_type(out, &s.ty);
                writeln!(out).ok();
            }
            for h in &b.handshakes {
                pad(out, depth + 1);
                let role = match h.role {
                    HandshakeRole::Send => "send",
                    HandshakeRole::Receive => "receive",
                };
                writeln!(out, "handshake_channel {}: {} kind: {}", h.name.name, role, h.variant.name).ok();
                for s in &h.payload {
                    pad(out, depth + 2);
                    write!(out, "{}: ", s.name.name).ok();
                    print_type(out, &s.ty);
                    writeln!(out).ok();
                }
                pad(out, depth + 1);
                writeln!(out, "end handshake_channel {}", h.name.name).ok();
            }
            pad(out, depth);
            writeln!(out, "end bus {}", b.name.name).ok();
        }
        Item::Transactor(t) => {
            print_transactor(out, t, depth);
        }
    }
}

fn print_transactor(out: &mut String, t: &TransactorDecl, depth: usize) {
    print_doc(out, &t.doc, depth);
    pad(out, depth);
    write!(out, "transactor {}", t.name.name).ok();
    if !t.params.is_empty() {
        print_paren_params(out, &t.params);
    }
    if let Some(b) = &t.bound_to {
        write!(out, " bound to ").ok();
        print_type(out, b);
    }
    writeln!(out).ok();
    print_inner_doc(out, &t.inner_doc, depth + 1);
    for it in &t.items {
        print_component_item(out, it, depth + 1);
    }
    if let Some(active_items) = &t.when_active {
        pad(out, depth + 1);
        writeln!(out, "when active").ok();
        for it in active_items {
            print_component_item(out, it, depth + 2);
        }
        pad(out, depth + 1);
        writeln!(out, "end when").ok();
    }
    pad(out, depth);
    writeln!(out, "end transactor {}", t.name.name).ok();
}

fn print_component(out: &mut String, c: &ComponentDecl, depth: usize) {
    print_doc(out, &c.doc, depth);
    pad(out, depth);
    write!(out, "{} {}", c.kind.keyword(), c.name.name).ok();
    print_generic_params(out, &c.params);
    if let Some(b) = &c.bound_to {
        write!(out, " bound to ").ok();
        print_type(out, b);
    }
    writeln!(out).ok();
    print_inner_doc(out, &c.inner_doc, depth + 1);
    for it in &c.items {
        print_component_item(out, it, depth + 1);
    }
    pad(out, depth);
    writeln!(out, "end {} {}", c.kind.keyword(), c.name.name).ok();
}

fn print_component_item(out: &mut String, it: &ComponentItem, depth: usize) {
    match it {
        ComponentItem::Field(f) => {
            print_doc(out, &f.doc, depth);
            pad(out, depth);
            write!(out, "{} : ", f.name.name).ok();
            if let Some(d) = f.direction {
                let s = match d { Direction::In => "in", Direction::Out => "out", Direction::InOut => "inout" };
                write!(out, "{s} ").ok();
            }
            print_type(out, &f.ty);
            if let Some(b) = &f.bound_to {
                write!(out, " bound to ").ok();
                print_type(out, b);
            }
            if let Some(e) = &f.default {
                write!(out, " = ").ok();
                print_expr(out, e);
            }
            writeln!(out).ok();
        }
        ComponentItem::Connect(cb) => {
            pad(out, depth);
            writeln!(out, "connect").ok();
            for e in &cb.edges {
                pad(out, depth + 1);
                print_expr(out, &e.from);
                write!(out, " -> ").ok();
                print_expr(out, &e.to);
                writeln!(out).ok();
            }
            pad(out, depth);
            writeln!(out, "end connect").ok();
        }
        ComponentItem::OnHandler(h) => {
            print_on_handler(out, h, depth);
        }
        ComponentItem::Hookable(h) => {
            pad(out, depth);
            write!(out, "hookable {}", h.name.name).ok();
            print_paren_params(out, &h.params);
            if let Some(rt) = &h.return_ty {
                write!(out, " -> ").ok();
                print_type(out, rt);
            }
            writeln!(out).ok();
            print_block_inner(out, &h.body, depth + 1);
            pad(out, depth);
            writeln!(out, "end {}", h.name.name).ok();
        }
        ComponentItem::Apply(a) => {
            pad(out, depth);
            write!(out, "apply ").ok();
            print_path(out, &a.path);
            writeln!(out).ok();
        }
        ComponentItem::Watchdog(w) => {
            print_watchdog(out, w, depth);
        }
    }
}

fn print_on_handler(out: &mut String, h: &OnHandler, depth: usize) {
    pad(out, depth);
    write!(out, "on ").ok();
    print_expr(out, &h.event);
    if h.periodic {
        write!(out, " cycles").ok();
    }
    if let Some(s) = h.hook {
        let s = match s { HookSide::Pre => " pre", HookSide::Post => " post" };
        write!(out, "{s}").ok();
    }
    writeln!(out).ok();
    print_block_inner(out, &h.body, depth + 1);
    pad(out, depth);
    writeln!(out, "end on").ok();
}

fn print_watchdog(out: &mut String, w: &WatchdogDecl, depth: usize) {
    pad(out, depth);
    if w.disabled {
        writeln!(out, "watchdog disabled").ok();
        return;
    }
    writeln!(out, "watchdog").ok();
    if let Some(p) = &w.period {
        pad(out, depth + 1);
        write!(out, "period ").ok();
        print_expr(out, p);
        writeln!(out, " cycles").ok();
    }
    if let Some(m) = &w.max_idle {
        pad(out, depth + 1);
        write!(out, "max_idle ").ok();
        print_expr(out, m);
        writeln!(out, " cycles").ok();
    }
    print_block_inner(out, &w.body, depth + 1);
    pad(out, depth);
    writeln!(out, "end watchdog").ok();
}

fn print_test_item(out: &mut String, it: &TestItem, depth: usize) {
    match it {
        TestItem::Apply(a) => {
            pad(out, depth);
            write!(out, "apply ").ok();
            print_path(out, &a.path);
            writeln!(out).ok();
        }
        TestItem::Let(l) => {
            print_let(out, l, depth);
        }
        TestItem::Use(u) => {
            pad(out, depth);
            write!(out, "use ").ok();
            print_path(out, &u.path);
            writeln!(out).ok();
        }
        TestItem::Stmt(s) => print_stmt(out, s, depth),
        TestItem::Clock(c) => {
            print_doc(out, &c.doc, depth);
            pad(out, depth);
            write!(out, "clock {} = ", c.name.name).ok();
            print_expr(out, &c.period);
            writeln!(out).ok();
        }
        TestItem::Scope(s) => {
            // Inline form per docs/test-ergonomics.md — emit each
            // populated phase block directly at test scope, with no
            // outer `scope sim ... end scope sim` wrapper. The
            // round-trip discipline relies on this matching the
            // parser's accepted shape (parse_test).
            if let Some(b) = &s.setup {
                pad(out, depth);
                writeln!(out, "setup").ok();
                print_block_inner(out, b, depth + 1);
                pad(out, depth);
                writeln!(out, "end setup").ok();
            }
            if let Some(b) = &s.run {
                pad(out, depth);
                writeln!(out, "run").ok();
                print_block_inner(out, b, depth + 1);
                pad(out, depth);
                writeln!(out, "end run").ok();
            }
            if let Some(b) = &s.check {
                pad(out, depth);
                writeln!(out, "check").ok();
                print_block_inner(out, b, depth + 1);
                pad(out, depth);
                writeln!(out, "end check").ok();
            }
            if let Some(b) = &s.teardown {
                pad(out, depth);
                writeln!(out, "teardown").ok();
                print_block_inner(out, b, depth + 1);
                pad(out, depth);
                writeln!(out, "end teardown").ok();
            }
        }
        TestItem::Phase(name, body) => {
            pad(out, depth);
            writeln!(out, "phase {}", name.name).ok();
            print_block_inner(out, body, depth + 1);
            pad(out, depth);
            writeln!(out, "end phase {}", name.name).ok();
        }
    }
}

fn print_cover_item(out: &mut String, it: &CoverItem, depth: usize) {
    match it {
        CoverItem::Point(p) => {
            pad(out, depth);
            write!(out, "{} : cover ", p.name.name).ok();
            print_expr(out, &p.target);
            writeln!(out).ok();
            if !p.bins.is_empty() {
                pad(out, depth + 1);
                writeln!(out, "bins").ok();
                for b in &p.bins {
                    pad(out, depth + 2);
                    write!(out, "{} = ", b.name.name).ok();
                    print_expr(out, &b.spec);
                    writeln!(out).ok();
                }
                pad(out, depth + 1);
                writeln!(out, "end bins").ok();
            }
        }
        CoverItem::Cross(c) => {
            pad(out, depth);
            write!(out, "cross ").ok();
            for (i, p) in c.points.iter().enumerate() {
                if i > 0 { write!(out, ", ").ok(); }
                write!(out, "{}", p.name).ok();
            }
            writeln!(out).ok();
        }
    }
}

fn print_field(out: &mut String, f: &Field, depth: usize) {
    print_doc(out, &f.doc, depth);
    pad(out, depth);
    if f.non_random { write!(out, "!").ok(); }
    write!(out, "{} : ", f.name.name).ok();
    print_type(out, &f.ty);
    if let Some(d) = &f.default {
        write!(out, " default ").ok();
        print_expr(out, d);
    }
    if !f.attrs.is_empty() {
        write!(out, " with ").ok();
        for (i, a) in f.attrs.iter().enumerate() {
            if i > 0 { write!(out, " ").ok(); }
            print_attr(out, a);
        }
    }
    writeln!(out).ok();
}

fn print_attr(out: &mut String, a: &Attr) {
    write!(out, "[{}", a.name.name).ok();
    if !a.args.is_empty() {
        let mut started = false;
        for arg in &a.args {
            match arg {
                AttrArg::Expr(e) => {
                    if !started { write!(out, "(").ok(); started = true; }
                    else { write!(out, ", ").ok(); }
                    print_expr(out, e);
                }
                AttrArg::WithinScope(s) => {
                    write!(out, " within {}", s.name).ok();
                }
                AttrArg::Dist(entries) => {
                    write!(out, " {{").ok();
                    for (i, e) in entries.iter().enumerate() {
                        if i > 0 { write!(out, ", ").ok(); }
                        print_expr(out, &e.value);
                        write!(out, " :/ ").ok();
                        print_expr(out, &e.weight);
                    }
                    write!(out, "}}").ok();
                }
            }
        }
        if started { write!(out, ")").ok(); }
    }
    write!(out, "]").ok();
}

fn print_txn_body_item(out: &mut String, it: &TxnBodyItem, depth: usize) {
    match it {
        TxnBodyItem::Field(f) => print_field(out, f, depth),
        TxnBodyItem::Keep(k) => {
            pad(out, depth);
            write!(out, "keep ").ok();
            print_expr(out, &k.expr);
            writeln!(out).ok();
        }
        TxnBodyItem::When(w) => {
            pad(out, depth);
            write!(out, "when ").ok();
            print_expr(out, &w.discriminant);
            writeln!(out).ok();
            for it in &w.items {
                print_txn_body_item(out, it, depth + 1);
            }
            pad(out, depth);
            writeln!(out, "end when").ok();
        }
    }
}

fn print_path(out: &mut String, p: &Path) {
    for (i, s) in p.segments.iter().enumerate() {
        if i > 0 { write!(out, ".").ok(); }
        write!(out, "{}", s.name).ok();
    }
}

fn print_generic_params(out: &mut String, ps: &[Param]) {
    if ps.is_empty() { return; }
    write!(out, "#(").ok();
    for (i, p) in ps.iter().enumerate() {
        if i > 0 { write!(out, ", ").ok(); }
        write!(out, "{}", p.name.name).ok();
        if let Some(t) = &p.ty {
            write!(out, ": ").ok();
            print_type(out, t);
        }
        if let Some(d) = &p.default {
            write!(out, " = ").ok();
            print_expr(out, d);
        }
    }
    write!(out, ")").ok();
}

fn print_paren_params(out: &mut String, ps: &[Param]) {
    write!(out, "(").ok();
    for (i, p) in ps.iter().enumerate() {
        if i > 0 { write!(out, ", ").ok(); }
        write!(out, "{}", p.name.name).ok();
        if let Some(t) = &p.ty {
            write!(out, ": ").ok();
            print_type(out, t);
        }
        if let Some(d) = &p.default {
            write!(out, " = ").ok();
            print_expr(out, d);
        }
    }
    write!(out, ")").ok();
}

fn print_type(out: &mut String, t: &TypeExpr) {
    match t {
        TypeExpr::Named { name, generics, mode, .. } => {
            print_path(out, name);
            if !generics.is_empty() {
                write!(out, "#(").ok();
                print_type_args(out, generics);
                write!(out, ")").ok();
            }
            // Transactor mode annotation: `T active` / `T passive`
            // (see ast::TransactorMode). Only set at instantiation
            // sites; round-trips through fmt → reparse.
            if let Some(m) = mode {
                let s = match m {
                    TransactorMode::Active => "active",
                    TransactorMode::Passive => "passive",
                };
                write!(out, " {s}").ok();
            }
        }
        TypeExpr::Builtin { name, args, .. } => {
            let s = builtin_ty_name(*name);
            write!(out, "{s}").ok();
            if !args.is_empty() {
                write!(out, "<").ok();
                print_type_args(out, args);
                write!(out, ">").ok();
            }
        }
    }
}

fn builtin_ty_name(n: BuiltinTy) -> &'static str {
    match n {
        BuiltinTy::UInt => "uint",
        BuiltinTy::SInt => "sint",
        BuiltinTy::Bits => "bits",
        BuiltinTy::UIntCap => "UInt",
        BuiltinTy::SIntCap => "SInt",
        BuiltinTy::Bool => "Bool",
        BuiltinTy::BoolLower => "bool",
        BuiltinTy::Bit => "Bit",
        BuiltinTy::Int => "int",
        BuiltinTy::Time => "time",
        BuiltinTy::Prop => "prop",
        BuiltinTy::Pseq => "pseq",
        BuiltinTy::Severity => "Severity",
        BuiltinTy::Logger => "Logger",
        BuiltinTy::String => "String",
        BuiltinTy::Vec => "Vec",
        BuiltinTy::Event => "event",
        BuiltinTy::EventComb => "event comb",
        BuiltinTy::Buffer => "buffer",
        BuiltinTy::Stream => "stream",
        BuiltinTy::State => "state",
        BuiltinTy::Queue => "queue",
        BuiltinTy::TSeq => "TSeq",
        BuiltinTy::Clock => "Clock",
        BuiltinTy::Reset => "Reset",
    }
}

fn print_type_args(out: &mut String, args: &[TypeArg]) {
    for (i, a) in args.iter().enumerate() {
        if i > 0 { write!(out, ", ").ok(); }
        match a {
            TypeArg::Type(t) => print_type(out, t),
            TypeArg::Expr(e) => print_expr(out, e),
            TypeArg::Named { name, value } => {
                write!(out, "{}=", name.name).ok();
                print_expr(out, value);
            }
        }
    }
}

fn print_block_inner(out: &mut String, b: &Block, depth: usize) {
    for s in &b.stmts {
        print_stmt(out, s, depth);
    }
}

fn print_stmt(out: &mut String, s: &Stmt, depth: usize) {
    match &s.kind {
        StmtKind::Let(l) => print_let(out, l, depth),
        StmtKind::Assign { target, value } => {
            pad(out, depth);
            print_expr(out, target);
            write!(out, " = ").ok();
            print_expr(out, value);
            writeln!(out).ok();
        }
        StmtKind::Send { target, value } => {
            pad(out, depth);
            print_expr(out, target);
            write!(out, " <- ").ok();
            print_expr(out, value);
            writeln!(out).ok();
        }
        StmtKind::For(f) => {
            pad(out, depth);
            write!(out, "for {} in ", f.var.name).ok();
            print_expr(out, &f.iter);
            writeln!(out).ok();
            print_block_inner(out, &f.body, depth + 1);
            pad(out, depth);
            writeln!(out, "end for").ok();
        }
        StmtKind::Repeat(r) => {
            pad(out, depth);
            write!(out, "repeat ").ok();
            print_expr(out, &r.count);
            writeln!(out).ok();
            print_block_inner(out, &r.body, depth + 1);
            pad(out, depth);
            writeln!(out, "end repeat").ok();
        }
        StmtKind::Loop(b) => {
            pad(out, depth);
            writeln!(out, "loop").ok();
            print_block_inner(out, b, depth + 1);
            pad(out, depth);
            writeln!(out, "end loop").ok();
        }
        StmtKind::While { cond, body, .. } => {
            pad(out, depth);
            write!(out, "while ").ok();
            print_expr(out, cond);
            writeln!(out).ok();
            print_block_inner(out, body, depth + 1);
            pad(out, depth);
            writeln!(out, "end while").ok();
        }
        StmtKind::Break { .. } => {
            pad(out, depth);
            writeln!(out, "break").ok();
        }
        StmtKind::Continue { .. } => {
            pad(out, depth);
            writeln!(out, "continue").ok();
        }
        StmtKind::If(i) => {
            pad(out, depth);
            write!(out, "if ").ok();
            print_expr(out, &i.cond);
            writeln!(out).ok();
            print_block_inner(out, &i.then_block, depth + 1);
            for (c, b) in &i.elsifs {
                pad(out, depth);
                write!(out, "elsif ").ok();
                print_expr(out, c);
                writeln!(out).ok();
                print_block_inner(out, b, depth + 1);
            }
            if let Some(eb) = &i.else_block {
                pad(out, depth);
                writeln!(out, "else").ok();
                print_block_inner(out, eb, depth + 1);
            }
            pad(out, depth);
            writeln!(out, "end if").ok();
        }
        StmtKind::Fork(f) => {
            pad(out, depth);
            writeln!(out, "fork").ok();
            for b in &f.branches {
                pad(out, depth + 1);
                writeln!(out, "branch").ok();
                print_block_inner(out, b, depth + 2);
                pad(out, depth + 1);
                writeln!(out, "end branch").ok();
            }
            pad(out, depth);
            let kw = match f.join { ForkJoin::All => "join_all", ForkJoin::Any => "join_any", ForkJoin::None => "join_none" };
            writeln!(out, "{kw}").ok();
        }
        StmtKind::Parallel(branches) => {
            pad(out, depth);
            writeln!(out, "parallel").ok();
            for b in branches {
                print_block_inner(out, b, depth + 1);
            }
            pad(out, depth);
            writeln!(out, "end parallel").ok();
        }
        StmtKind::Schedule(branches) => {
            pad(out, depth);
            writeln!(out, "schedule").ok();
            for b in branches {
                print_block_inner(out, b, depth + 1);
            }
            pad(out, depth);
            writeln!(out, "end schedule").ok();
        }
        StmtKind::Select(arms) => {
            pad(out, depth);
            writeln!(out, "select").ok();
            for a in arms {
                pad(out, depth + 1);
                print_expr(out, &a.event);
                write!(out, " => ").ok();
                if let Some(s) = a.action.stmts.first() {
                    // Inline the action statement.
                    let mut inner = String::new();
                    print_stmt(&mut inner, s, 0);
                    out.push_str(inner.trim_end());
                    writeln!(out).ok();
                } else {
                    writeln!(out).ok();
                }
            }
            pad(out, depth);
            writeln!(out, "end select").ok();
        }
        StmtKind::On(h) => print_on_handler(out, h, depth),
        StmtKind::Emit { name, args, .. } => {
            pad(out, depth);
            write!(out, "emit ").ok();
            print_path(out, name);
            if !args.is_empty() {
                write!(out, "(").ok();
                for (i, a) in args.iter().enumerate() {
                    if i > 0 { write!(out, ", ").ok(); }
                    match a {
                        CallArg::Expr(e) => print_expr(out, e),
                        CallArg::Named { name, value } => {
                            write!(out, "{}=", name.name).ok();
                            print_expr(out, value);
                        }
                    }
                }
                write!(out, ")").ok();
            }
            writeln!(out).ok();
        }
        StmtKind::Yield(e) => {
            pad(out, depth);
            write!(out, "yield ").ok();
            print_expr(out, e);
            writeln!(out).ok();
        }
        StmtKind::Return(e) => {
            pad(out, depth);
            write!(out, "return").ok();
            if let Some(v) = e {
                write!(out, " ").ok();
                print_expr(out, v);
            }
            writeln!(out).ok();
        }
        StmtKind::Apply(a) => {
            pad(out, depth);
            write!(out, "apply ").ok();
            print_path(out, &a.path);
            writeln!(out).ok();
        }
        StmtKind::Assert(v) => print_verify(out, "assert", v, depth),
        StmtKind::Assume(v) => print_verify(out, "assume", v, depth),
        StmtKind::Cover(v) => print_verify(out, "cover", v, depth),
        StmtKind::Randomize { blocking, target, with_body } => {
            pad(out, depth);
            if *blocking { write!(out, "blocking ").ok(); }
            write!(out, "randomize(").ok();
            print_expr(out, target);
            write!(out, ")").ok();
            if !with_body.is_empty() {
                writeln!(out, " with").ok();
                for e in with_body {
                    pad(out, depth + 1);
                    print_expr(out, e);
                    writeln!(out).ok();
                }
                pad(out, depth);
                writeln!(out, "end randomize").ok();
            } else {
                writeln!(out).ok();
            }
        }
        StmtKind::Log { args, .. } => print_log_call(out, "log", args, depth),
        StmtKind::LogF { args, .. } => print_log_call(out, "logf", args, depth),
        StmtKind::Expr(e) => {
            pad(out, depth);
            print_expr(out, e);
            writeln!(out).ok();
        }
        StmtKind::After { duration, body, .. } => {
            pad(out, depth);
            write!(out, "after ").ok();
            print_expr(out, duration);
            writeln!(out, " cycles").ok();
            print_block_inner(out, body, depth + 1);
            pad(out, depth);
            writeln!(out, "end after").ok();
        }
        StmtKind::Wait { duration, clock, .. } => {
            pad(out, depth);
            write!(out, "wait ").ok();
            print_expr(out, duration);
            write!(out, " cycles").ok();
            if let Some(c) = clock {
                write!(out, " on {}", c.name).ok();
            }
            writeln!(out).ok();
        }
        StmtKind::WaitUntil { mode, conditions, timeout, .. } => {
            pad(out, depth);
            write!(out, "wait until").ok();
            match mode {
                WaitUntilMode::Single => {}
                WaitUntilMode::AllOf  => { write!(out, " all of").ok(); }
                WaitUntilMode::AnyOf  => { write!(out, " any of").ok(); }
            }
            for (i, c) in conditions.iter().enumerate() {
                if i == 0 { write!(out, " ").ok(); }
                else      { write!(out, ", ").ok(); }
                print_expr(out, c);
            }
            if let Some(to) = timeout {
                write!(out, " timeout ").ok();
                print_expr(out, &to.cycles);
                write!(out, " cycles").ok();
                if let Some(m) = &to.message {
                    write!(out, " fail(").ok();
                    print_expr(out, m);
                    write!(out, ")").ok();
                }
            }
            writeln!(out).ok();
        }
        StmtKind::Fail { msg, .. } => {
            pad(out, depth);
            write!(out, "fail(").ok();
            print_expr(out, msg);
            writeln!(out, ")").ok();
        }
    }
}

fn print_verify(out: &mut String, kw: &str, v: &Verify, depth: usize) {
    pad(out, depth);
    write!(out, "{kw}").ok();
    if v.property_kw {
        write!(out, " property").ok();
    }
    if let Some(e) = &v.expr {
        write!(out, " ").ok();
        print_expr(out, e);
    } else if let Some(n) = &v.named {
        write!(out, " {}", n.name).ok();
    }
    if let Some(f) = &v.else_fail {
        write!(out, " else fail(").ok();
        print_expr(out, f);
        write!(out, ")").ok();
    }
    writeln!(out).ok();
}

fn print_let(out: &mut String, l: &LetStmt, depth: usize) {
    pad(out, depth);
    write!(out, "let {}", l.name.name).ok();
    if let Some(t) = &l.ty {
        write!(out, " : ").ok();
        print_type(out, t);
    }
    if let Some(v) = &l.value {
        write!(out, " = ").ok();
        if l.bind {
            write!(out, "bind ").ok();
        }
        print_expr(out, v);
    }
    writeln!(out).ok();
}

/// Pretty-print an expression appearing inside a clocking spec `@(...)`.
/// `posedge`/`negedge` are parsed as `Call(<edge>, [arg])` for AST
/// uniformity but must be rendered SVA-style (`posedge clk`, no parens
/// on the arg) so `harc fmt` is idempotent.
fn print_clocking_expr(out: &mut String, e: &Expr) {
    if let ExprKind::Call { callee, args } = &*e.kind {
        if let ExprKind::Ident(id) = &*callee.kind {
            if (id.name == "posedge" || id.name == "negedge") && args.len() == 1 {
                if let CallArg::Expr(inner) = &args[0] {
                    write!(out, "{} ", id.name).ok();
                    print_expr(out, inner);
                    return;
                }
            }
        }
    }
    print_expr(out, e);
}

pub fn print_expr(out: &mut String, e: &Expr) {
    match &*e.kind {
        ExprKind::Int(s) | ExprKind::Float(s) | ExprKind::Time(s) => { write!(out, "{s}").ok(); }
        ExprKind::String(s) => { write!(out, "\"{s}\"").ok(); }
        ExprKind::Bool(b) => { write!(out, "{}", if *b { "true" } else { "false" }).ok(); }
        ExprKind::Ident(id) => { write!(out, "{}", id.name).ok(); }
        ExprKind::ImplicitSelf => {} // emitted as the leading dot in Field
        ExprKind::Field { target, name } => {
            if matches!(&*target.kind, ExprKind::ImplicitSelf) {
                write!(out, ".{}", name.name).ok();
            } else {
                print_expr(out, target);
                write!(out, ".{}", name.name).ok();
            }
        }
        ExprKind::Index { target, index } => {
            print_expr(out, target);
            write!(out, "[").ok();
            print_expr(out, index);
            write!(out, "]").ok();
        }
        ExprKind::BitSlice { target, hi, lo } => {
            print_expr(out, target);
            write!(out, "[").ok();
            print_expr(out, hi);
            write!(out, ":").ok();
            print_expr(out, lo);
            write!(out, "]").ok();
        }
        ExprKind::Call { callee, args } => {
            print_expr(out, callee);
            write!(out, "(").ok();
            for (i, a) in args.iter().enumerate() {
                if i > 0 { write!(out, ", ").ok(); }
                match a {
                    CallArg::Expr(e) => print_expr(out, e),
                    CallArg::Named { name, value } => {
                        write!(out, "{}=", name.name).ok();
                        print_expr(out, value);
                    }
                }
            }
            write!(out, ")").ok();
        }
        ExprKind::Cast { expr, ty } => {
            print_expr(out, expr);
            write!(out, " as ").ok();
            print_type(out, ty);
        }
        ExprKind::Send { target, value } => {
            print_expr(out, target);
            write!(out, " <- ").ok();
            print_expr(out, value);
        }
        ExprKind::Unary { op, expr } => {
            let s = match op {
                UnaryOp::Neg => "-", UnaryOp::Not => "!", UnaryOp::NotKw => "not ", UnaryOp::BitNot => "~",
            };
            write!(out, "{s}").ok();
            print_expr(out, expr);
        }
        ExprKind::Binary { op, lhs, rhs } => {
            print_expr(out, lhs);
            write!(out, " {} ", binary_op_str(*op)).ok();
            print_expr(out, rhs);
        }
        ExprKind::Ternary { cond, then_branch, else_branch } => {
            print_expr(out, cond);
            write!(out, " ? ").ok();
            print_expr(out, then_branch);
            write!(out, " : ").ok();
            print_expr(out, else_branch);
        }
        ExprKind::HashHash { count, expr } => {
            write!(out, "##").ok();
            print_hash_count(out, count);
            write!(out, " ").ok();
            print_expr(out, expr);
        }
        ExprKind::SeqRepeat { expr, count } => {
            print_expr(out, expr);
            write!(out, " [*").ok();
            print_hash_count(out, count);
            write!(out, "]").ok();
        }
        ExprKind::RangeLit { lo, hi } => {
            write!(out, "[").ok();
            if let Some(l) = lo { print_expr(out, l); }
            write!(out, "..").ok();
            if let Some(h) = hi { print_expr(out, h); }
            write!(out, "]").ok();
        }
        ExprKind::SetLit(items) => {
            write!(out, "{{").ok();
            for (i, e) in items.iter().enumerate() {
                if i > 0 { write!(out, ", ").ok(); }
                print_expr(out, e);
            }
            write!(out, "}}").ok();
        }
        ExprKind::DistLit(entries) => {
            write!(out, "dist {{").ok();
            for (i, e) in entries.iter().enumerate() {
                if i > 0 { write!(out, ", ").ok(); }
                print_expr(out, &e.value);
                write!(out, " :/ ").ok();
                print_expr(out, &e.weight);
            }
            write!(out, "}}").ok();
        }
        ExprKind::SystemCall { name, args } => {
            let s = match name {
                SystemFn::Rose => "$rose", SystemFn::Fell => "$fell",
                SystemFn::Stable => "$stable", SystemFn::Past => "$past",
                SystemFn::Clog2 => "$clog2",
            };
            write!(out, "{s}").ok();
            if !args.is_empty() {
                write!(out, "(").ok();
                for (i, a) in args.iter().enumerate() {
                    if i > 0 { write!(out, ", ").ok(); }
                    print_expr(out, a);
                }
                write!(out, ")").ok();
            }
        }
        ExprKind::Randomize { blocking, target, with_body } => {
            if *blocking { write!(out, "blocking ").ok(); }
            write!(out, "randomize(").ok();
            print_expr(out, target);
            write!(out, ")").ok();
            if !with_body.is_empty() {
                write!(out, " with ").ok();
                for (i, e) in with_body.iter().enumerate() {
                    if i > 0 { write!(out, "; ").ok(); }
                    print_expr(out, e);
                }
                write!(out, " end randomize").ok();
            }
        }
        ExprKind::DistDirective { target, entries } => {
            if !matches!(&*target.kind, ExprKind::ImplicitSelf) {
                print_expr(out, target);
                write!(out, " ").ok();
            }
            write!(out, "dist {{").ok();
            for (i, e) in entries.iter().enumerate() {
                if i > 0 { write!(out, ", ").ok(); }
                print_expr(out, &e.value);
                write!(out, " :/ ").ok();
                print_expr(out, &e.weight);
            }
            write!(out, "}}").ok();
        }
        ExprKind::Paren(e) => {
            write!(out, "(").ok();
            print_expr(out, e);
            write!(out, ")").ok();
        }
        ExprKind::NamedArg { name, value } => {
            write!(out, "{}=", name.name).ok();
            print_expr(out, value);
        }
        ExprKind::StructLit { ty, fields } => {
            print_type(out, ty);
            write!(out, " {{").ok();
            for (i, f) in fields.iter().enumerate() {
                if i > 0 { write!(out, ", ").ok(); }
                write!(out, "{}: ", f.name.name).ok();
                print_expr(out, &f.value);
            }
            write!(out, "}}").ok();
        }
        ExprKind::CoverArrow { lhs, rhs, count } => {
            print_expr(out, lhs);
            write!(out, " ->").ok();
            if let Some(c) = count {
                write!(out, "[").ok();
                print_hash_count(out, c);
                write!(out, "]").ok();
            }
            write!(out, " ").ok();
            print_expr(out, rhs);
        }
        ExprKind::Solve { kind, args } => {
            let s = match kind { SolveKind::Before => "solve_before", SolveKind::After => "solve_after" };
            write!(out, "{s}(").ok();
            for (i, a) in args.iter().enumerate() {
                if i > 0 { write!(out, ", ").ok(); }
                print_expr(out, a);
            }
            write!(out, ")").ok();
        }
        ExprKind::Membership { expr, set } => {
            print_expr(out, expr);
            write!(out, " in ").ok();
            print_expr(out, set);
        }
    }
}

fn print_hash_count(out: &mut String, c: &HashCount) {
    match c {
        HashCount::Const(e) => print_expr(out, e),
        HashCount::Range { lo, hi } => {
            write!(out, "[").ok();
            print_expr(out, lo);
            write!(out, ":").ok();
            print_expr(out, hi);
            write!(out, "]").ok();
        }
    }
}

fn print_log_call(out: &mut String, kw: &str, args: &[CallArg], depth: usize) {
    pad(out, depth);
    write!(out, "{kw}(").ok();
    for (i, a) in args.iter().enumerate() {
        if i > 0 { write!(out, ", ").ok(); }
        match a {
            CallArg::Expr(e) => print_expr(out, e),
            CallArg::Named { name, value } => {
                write!(out, "{}=", name.name).ok();
                print_expr(out, value);
            }
        }
    }
    writeln!(out, ")").ok();
}

fn binary_op_str(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+", Sub => "-", Mul => "*", Div => "/", Mod => "%",
        Eq => "==", Ne => "!=", Lt => "<", Le => "<=", Gt => ">", Ge => ">=",
        AndAnd => "&&", OrOr => "||", AndKw => "and", OrKw => "or",
        BitAnd => "&", BitOr => "|", BitXor => "^", Shl => "<<", Shr => ">>",
        PipeImplies => "|->", PipeImpliesNext => "|=>",
        Throughout => "throughout", Within => "within", Intersect => "intersect",
        In => "in", Inside => "inside",
    }
}
