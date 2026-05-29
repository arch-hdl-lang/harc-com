//! HARC parser — recursive descent, LL(1) per spec §1 / §2.

use crate::ast::*;
use crate::diagnostics::CompileError;
use crate::lexer::{Span, Token, TokenKind};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// When true, `>` is not treated as a binary operator (inside type angle brackets).
    no_angle: bool,
    /// Source text — kept so that newline-aware disambiguation (e.g.
    /// `property foo\n(body)` vs `property foo(args) body`) can check for
    /// newlines between tokens.
    source: String,
}

/// Parse a single expression fragment (for codegen-time string-interpolation
/// `${expr}` capture). Returns the parsed `Expr`; codegen then routes it
/// through `emit_expr` so pointer-deref / field rewrites apply correctly.
pub fn parse_expr_fragment(source: &str) -> Result<Expr, CompileError> {
    let tokens = crate::lexer::tokenize(source).map_err(|spans| {
        let span = spans.first().copied().unwrap_or(Span::new(0, 0));
        CompileError::LexerError {
            span: crate::diagnostics::span_to_source_span(span),
        }
    })?;
    let mut p = Parser::new(tokens, source);
    p.parse_expr()
}

pub fn parse_source(source: &str) -> Result<SourceFile, CompileError> {
    let tokens = crate::lexer::tokenize(source).map_err(|spans| {
        let span = spans.first().copied().unwrap_or(Span::new(0, 0));
        CompileError::LexerError {
            span: crate::diagnostics::span_to_source_span(span),
        }
    })?;
    let mut p = Parser::new(tokens, source);
    p.parse_source_file()
}

impl Parser {
    pub fn new(tokens: Vec<Token>, source: &str) -> Self {
        Self {
            tokens,
            pos: 0,
            no_angle: false,
            source: source.to_string(),
        }
    }

    /// True if a newline appears in the source between `prev_end` (the end
    /// of a known-just-consumed token) and the next non-doc token's start.
    /// Used at sites where `(` could be a parameter list opener or the
    /// start of a body expression.
    fn newline_before_peek(&self, prev_end: usize) -> bool {
        let next = self.peek_span().start;
        if next <= prev_end || next > self.source.len() {
            return false;
        }
        self.source[prev_end..next].contains('\n')
    }

    // ── Token helpers ─────────────────────────────────────────────────────────

    fn peek(&self) -> Option<&Token> {
        // Skip doc tokens — they are consumed explicitly via consume_outer_doc / consume_inner_doc.
        let mut i = self.pos;
        while let Some(t) = self.tokens.get(i) {
            if matches!(t.kind, TokenKind::DocOuter(_) | TokenKind::DocInner(_)) {
                i += 1;
            } else {
                break;
            }
        }
        self.tokens.get(i)
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.peek().map(|t| &t.kind)
    }

    fn peek2_kind(&self) -> Option<&TokenKind> {
        let mut i = self.pos;
        let mut seen = 0;
        while let Some(t) = self.tokens.get(i) {
            if matches!(t.kind, TokenKind::DocOuter(_) | TokenKind::DocInner(_)) {
                i += 1;
                continue;
            }
            if seen == 1 {
                return Some(&t.kind);
            }
            seen += 1;
            i += 1;
        }
        None
    }

    fn peek_span(&self) -> Span {
        self.peek().map(|t| t.span).unwrap_or(Span::new(0, 0))
    }

    fn at_end(&self) -> bool {
        self.peek().is_none()
    }

    fn check(&self, kind: TokenKind) -> bool {
        self.peek_kind() == Some(&kind)
    }

    fn check_ident(&self, name: &str) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Ident(s)) if s == name)
    }

    fn advance(&mut self) -> Option<Token> {
        // Skip doc tokens.
        while let Some(t) = self.tokens.get(self.pos) {
            if matches!(t.kind, TokenKind::DocOuter(_) | TokenKind::DocInner(_)) {
                self.pos += 1;
            } else {
                break;
            }
        }
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, kind: TokenKind) -> Result<Token, CompileError> {
        let span = self.peek_span();
        match self.peek_kind() {
            Some(k) if *k == kind => Ok(self.advance().unwrap()),
            Some(k) => Err(CompileError::unexpected_token(
                &kind.to_string(),
                &k.to_string(),
                span,
            )),
            None => Err(CompileError::UnexpectedEof),
        }
    }

    fn expect_ident(&mut self) -> Result<Ident, CompileError> {
        let span = self.peek_span();
        match self.peek_kind().cloned() {
            Some(TokenKind::Ident(name)) => {
                self.advance();
                Ok(Ident { name, span })
            }
            Some(other) => Err(CompileError::unexpected_token(
                "identifier",
                &other.to_string(),
                span,
            )),
            None => Err(CompileError::UnexpectedEof),
        }
    }

    fn consume_outer_doc(&mut self) -> Option<String> {
        let mut lines = Vec::new();
        while let Some(t) = self.tokens.get(self.pos) {
            if let TokenKind::DocOuter(s) = &t.kind {
                lines.push(s.clone());
                self.pos += 1;
            } else if matches!(t.kind, TokenKind::DocInner(_)) {
                self.pos += 1;
            } else {
                break;
            }
        }
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    fn consume_inner_doc(&mut self) -> Option<String> {
        let mut lines = Vec::new();
        while let Some(t) = self.tokens.get(self.pos) {
            if let TokenKind::DocInner(s) = &t.kind {
                lines.push(s.clone());
                self.pos += 1;
            } else {
                break;
            }
        }
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    fn check_end_keyword(&self) -> bool {
        self.check(TokenKind::End)
    }

    /// Match `end <kw>` or `end <kw> <name>` and verify the name matches.
    fn expect_end(&mut self, expected_kw: TokenKind, name: &str) -> Result<Span, CompileError> {
        let end_tok = self.expect(TokenKind::End)?;
        let kw_tok = self.advance().ok_or(CompileError::UnexpectedEof)?;
        if kw_tok.kind != expected_kw {
            return Err(CompileError::mismatched_kind(
                &expected_kw.to_string(),
                &kw_tok.kind.to_string(),
                kw_tok.span,
            ));
        }
        // Optional closing name (named-end form). When present, must match.
        if let Some(TokenKind::Ident(_)) = self.peek_kind() {
            let id = self.expect_ident()?;
            if id.name != name {
                return Err(CompileError::mismatched_closing(name, &id.name, id.span));
            }
            Ok(end_tok.span.merge(id.span))
        } else {
            Ok(end_tok.span.merge(kw_tok.span))
        }
    }

    /// `end <kw>` — for anonymous compound blocks (`end on`, `end fork`, etc.).
    fn expect_end_anon(&mut self, expected_kw: TokenKind) -> Result<Span, CompileError> {
        let end_tok = self.expect(TokenKind::End)?;
        let kw_tok = self.advance().ok_or(CompileError::UnexpectedEof)?;
        if kw_tok.kind != expected_kw {
            return Err(CompileError::mismatched_kind(
                &expected_kw.to_string(),
                &kw_tok.kind.to_string(),
                kw_tok.span,
            ));
        }
        Ok(end_tok.span.merge(kw_tok.span))
    }

    // ── Top level ─────────────────────────────────────────────────────────────

    pub fn parse_source_file(&mut self) -> Result<SourceFile, CompileError> {
        let inner_doc = self.consume_inner_doc();
        // Extract the `---` … `---` YAML frontmatter sub-block (if any)
        // from `inner_doc`. Frontmatter is the contiguous block at the
        // very top of inner_doc: first line is exactly `---`, and the
        // closing `---` line ends the block. Anything before the first
        // `---` (free-form prose) disqualifies the file from having a
        // frontmatter — matches arch-com's behavior. The frontmatter
        // text retains the inside of the fences (no `---` lines).
        let frontmatter = inner_doc.as_ref().and_then(|d| extract_frontmatter(d));
        let mut items = Vec::new();
        while !self.at_end() {
            items.push(self.parse_item()?);
        }
        Ok(SourceFile {
            items,
            inner_doc,
            frontmatter,
        })
    }

    fn parse_item(&mut self) -> Result<Item, CompileError> {
        let doc = self.consume_outer_doc();
        match self.peek_kind() {
            Some(TokenKind::Use) => self.parse_use(doc).map(Item::Use),
            Some(TokenKind::Package) => self.parse_package(doc).map(Item::Package),
            Some(TokenKind::Const) => self.parse_const(doc).map(Item::Const),
            Some(TokenKind::Domain) => self.parse_domain(doc).map(Item::Domain),
            Some(TokenKind::Struct) => self.parse_struct(doc).map(Item::Struct),
            Some(TokenKind::Enum) => self.parse_enum(doc).map(Item::Enum),
            Some(TokenKind::Transaction) => self.parse_transaction(doc).map(Item::Transaction),
            Some(TokenKind::Relation) => self.parse_relation(doc).map(Item::Relation),
            Some(TokenKind::Tseq) => self.parse_tseq(doc).map(Item::Tseq),
            Some(TokenKind::Agent) => self.parse_component(ComponentKind::Agent, doc).map(Item::Agent),
            Some(TokenKind::Env) => self.parse_component(ComponentKind::Env, doc).map(Item::Env),
            // `testbench T ... end testbench T` shares env's component
            // machinery (fields, hookable methods, bus binding) — the
            // distinction is the source keyword and the convention of
            // owning a DUT-typed field. Stored as `Item::Env` so all
            // downstream codegen passes treat it identically.
            Some(TokenKind::Testbench) => self.parse_component(ComponentKind::Testbench, doc).map(Item::Env),
            Some(TokenKind::Scoreboard) => self.parse_component(ComponentKind::Scoreboard, doc).map(Item::Scoreboard),
            Some(TokenKind::Sequencer) => self.parse_component(ComponentKind::Sequencer, doc).map(Item::Sequencer),
            Some(TokenKind::Transactor) => self.parse_transactor(doc).map(Item::Transactor),
            // Classic `test <name> ... end test <name>` form. The
            // corpus sweep (PR #110 follow-up) migrated every fixture
            // to the new `impl <name> for <Tb>` form
            // (docs/test-ergonomics.md §3.3); the parser entry is
            // kept alive in this PR so the in-tree codegen unit
            // tests (~50 inline-source assertions) keep building.
            // Removal of the parser entry, plus the codegen-test
            // sweep, lands in a separate follow-up PR (same pattern
            // as PR #91 → #92 staged the inline-run migration).
            Some(TokenKind::Test) => self.parse_test(doc).map(Item::Test),
            Some(TokenKind::Extend) => self.parse_extend(doc).map(Item::Extend),
            Some(TokenKind::Covergroup) => self.parse_covergroup(doc).map(Item::Covergroup),
            Some(TokenKind::Property) => self.parse_property(doc).map(Item::Property),
            Some(TokenKind::Pseq) => self.parse_pseq(doc).map(Item::Pseq),
            Some(TokenKind::Cover) => {
                // `cover sequence Name = pattern` at item level (§17.3).
                if matches!(self.peek2_kind(), Some(TokenKind::Sequence)) {
                    self.parse_cover_sequence(doc).map(Item::CoverSequence)
                } else {
                    let span = self.peek_span();
                    Err(CompileError::unexpected_token(
                        "`sequence` after item-level `cover`",
                        &self.peek_kind().map(|k| k.to_string()).unwrap_or("EOF".into()),
                        span,
                    ))
                }
            }
            Some(TokenKind::Module) => self.parse_external_module(doc).map(Item::ExternalModule),
            Some(TokenKind::Function) => self.parse_function(doc).map(Item::Function),
            Some(TokenKind::Extern) => self.parse_extern_fn(doc).map(Item::ExternFn),
            Some(TokenKind::Apply) => self.parse_apply().map(Item::Apply),
            Some(TokenKind::Bus) => self.parse_bus(doc).map(Item::Bus),
            Some(TokenKind::Regblock) => self.parse_regblock(doc).map(Item::Regblock),
            Some(TokenKind::Addrmap) => self.parse_addrmap(doc).map(Item::Addrmap),
            // `impl <name> for <TbType> ... end impl <name>` — the
            // testbench-bound test form. Replaces the legacy
            // `impl sim for <Test>` two-block form (which was removed
            // in PR #92) with a different semantic: the `for` target
            // is a `testbench` declaration the test binds to, and the
            // testbench's fields + helper functions fold into scope
            // for the bound test's body. See docs/test-ergonomics.md
            // §3.3 and the AST `TestDecl.for_testbench` field.
            Some(TokenKind::Impl) => self.parse_impl_for_test(doc).map(Item::Test),
            Some(other) => Err(CompileError::unexpected_token(
                "use, package, const, struct, enum, transaction, relation, tseq, agent, env, scoreboard, sequencer, transactor, test, extend, covergroup, property, pseq, cover sequence, module, function, or apply",
                &other.to_string(),
                self.peek_span(),
            )),
            None => Err(CompileError::UnexpectedEof),
        }
    }

    // ── Use / Package / Const ────────────────────────────────────────────────

    fn parse_use(&mut self, doc: Option<String>) -> Result<UseDecl, CompileError> {
        let start = self.expect(TokenKind::Use)?.span;
        let path = self.parse_dotted_path()?;
        let span = start.merge(path.span);
        Ok(UseDecl { path, span, doc })
    }

    fn parse_dotted_path(&mut self) -> Result<Path, CompileError> {
        let first = self.expect_ident()?;
        let start = first.span;
        let mut segments = vec![first];
        let mut end = start;
        while self.check(TokenKind::Dot) {
            self.advance();
            let id = self.expect_ident()?;
            end = id.span;
            segments.push(id);
        }
        Ok(Path {
            segments,
            span: start.merge(end),
        })
    }

    fn parse_package(&mut self, doc: Option<String>) -> Result<PackageDecl, CompileError> {
        let start = self.expect(TokenKind::Package)?.span;
        let name = self.expect_ident()?;
        let inner_doc = self.consume_inner_doc();
        let mut items = Vec::new();
        while !self.check_end_keyword() {
            items.push(self.parse_item()?);
        }
        let end = self.expect_end(TokenKind::Package, &name.name)?;
        Ok(PackageDecl {
            name,
            items,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn parse_domain(&mut self, doc: Option<String>) -> Result<DomainDecl, CompileError> {
        let start = self.expect(TokenKind::Domain)?.span;
        let name = self.expect_ident()?;
        let mut fields = Vec::new();
        while !self.check_end_keyword() {
            let fname = self.expect_field_name()?;
            self.expect(TokenKind::Colon)?;
            let value = self.parse_expr()?;
            fields.push(DomainField { name: fname, value });
        }
        let end = self.expect_end(TokenKind::Domain, &name.name)?;
        Ok(DomainDecl {
            name,
            fields,
            span: start.merge(end),
            doc,
        })
    }

    fn parse_const(&mut self, doc: Option<String>) -> Result<ConstDecl, CompileError> {
        let start = self.expect(TokenKind::Const)?.span;
        let name = self.expect_ident()?;
        let ty = if self.check(TokenKind::Colon) {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        let span = start.merge(value.span);
        Ok(ConstDecl {
            name,
            ty,
            value,
            span,
            doc,
        })
    }

    // ── Struct / Enum ─────────────────────────────────────────────────────────

    fn parse_struct(&mut self, doc: Option<String>) -> Result<StructDecl, CompileError> {
        let start = self.expect(TokenKind::Struct)?.span;
        let name = self.expect_ident()?;
        let inner_doc = self.consume_inner_doc();
        let body = self.parse_txn_body_until_end()?;
        let fields = body
            .iter()
            .filter_map(|item| match item {
                TxnBodyItem::Field(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        let end = self.expect_end(TokenKind::Struct, &name.name)?;
        Ok(StructDecl {
            name,
            fields,
            body,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn parse_enum(&mut self, doc: Option<String>) -> Result<EnumDecl, CompileError> {
        let start = self.expect(TokenKind::Enum)?.span;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LBrace)?;
        let mut variants = Vec::new();
        while !self.check(TokenKind::RBrace) {
            variants.push(self.expect_ident()?);
            if self.check(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let end = self.expect(TokenKind::RBrace)?.span;
        Ok(EnumDecl {
            name,
            variants,
            span: start.merge(end),
            doc,
        })
    }

    // ── Transaction (§3.1, §3.3) ──────────────────────────────────────────────

    fn parse_transaction(&mut self, doc: Option<String>) -> Result<TransactionDecl, CompileError> {
        let start = self.expect(TokenKind::Transaction)?.span;
        let name = self.expect_ident()?;
        let params = self.parse_optional_generic_params()?;
        let inner_doc = self.consume_inner_doc();
        let body = self.parse_txn_body_until_end()?;
        let end = self.expect_end(TokenKind::Transaction, &name.name)?;
        Ok(TransactionDecl {
            name,
            params,
            body,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn parse_txn_body_until_end(&mut self) -> Result<Vec<TxnBodyItem>, CompileError> {
        let mut items = Vec::new();
        while !self.check_end_keyword() {
            items.push(self.parse_txn_body_item()?);
        }
        Ok(items)
    }

    fn parse_txn_body_item(&mut self) -> Result<TxnBodyItem, CompileError> {
        let doc = self.consume_outer_doc();
        match self.peek_kind() {
            Some(TokenKind::Keep) => self.parse_keep().map(TxnBodyItem::Keep),
            Some(TokenKind::When) => self.parse_when_subtype().map(TxnBodyItem::When),
            _ => self.parse_field(doc).map(TxnBodyItem::Field),
        }
    }

    fn parse_field(&mut self, doc: Option<String>) -> Result<Field, CompileError> {
        let mut non_random = false;
        let start = self.peek_span();
        if self.check(TokenKind::Bang) {
            self.advance();
            non_random = true;
        }
        let name = self.expect_field_name()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type_expr()?;
        let default = if self.check(TokenKind::Default) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let mut attrs = Vec::new();
        if self.check(TokenKind::With) {
            self.advance();
            // One or more `[attr]` forms; spec allows them stacked on multiple lines.
            while self.check(TokenKind::LBracket) {
                attrs.push(self.parse_attr()?);
            }
        }
        let end_span = attrs
            .last()
            .map(|a| a.span)
            .or(default.as_ref().map(|e| e.span))
            .unwrap_or(ty.span());
        Ok(Field {
            name,
            non_random,
            ty,
            default,
            attrs,
            span: start.merge(end_span),
            doc,
        })
    }

    fn parse_attr(&mut self) -> Result<Attr, CompileError> {
        let start = self.expect(TokenKind::LBracket)?.span;
        let name = self.expect_ident_or_kw()?;
        let mut args = Vec::new();
        if self.check(TokenKind::LParen) {
            self.advance();
            while !self.check(TokenKind::RParen) {
                let a = self.parse_attr_arg()?;
                args.push(a);
                if self.check(TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect(TokenKind::RParen)?;
        } else if name.name == "unique" && self.check(TokenKind::Within) {
            // `unique within tseq|sequencer|test` — special-form clause.
            // Scope names overlap with construct keywords (tseq/sequencer/test).
            self.advance();
            let scope = self.expect_scope_name()?;
            args.push(AttrArg::WithinScope(scope));
        } else if name.name == "dist" && self.check(TokenKind::LBrace) {
            // `[dist {[range] :/ w, ...}]`
            args.push(AttrArg::Dist(self.parse_dist_entries()?));
        }
        let end = self.expect(TokenKind::RBracket)?.span;
        Ok(Attr {
            name,
            args,
            span: start.merge(end),
        })
    }

    fn parse_attr_arg(&mut self) -> Result<AttrArg, CompileError> {
        // Range: `a..b` is parsed as part of a normal expression via RangeLit.
        Ok(AttrArg::Expr(self.parse_expr()?))
    }

    /// `unique within <scope>` — scope names include construct keywords.
    fn expect_scope_name(&mut self) -> Result<Ident, CompileError> {
        let span = self.peek_span();
        let tok = self.advance().ok_or(CompileError::UnexpectedEof)?;
        let name = match tok.kind {
            TokenKind::Ident(s) => s,
            TokenKind::Tseq => "tseq".into(),
            TokenKind::Sequencer => "sequencer".into(),
            TokenKind::Test => "test".into(),
            other => {
                return Err(CompileError::unexpected_token(
                    "scope name",
                    &other.to_string(),
                    span,
                ))
            }
        };
        Ok(Ident { name, span })
    }

    /// Accept any plain identifier, plus soft keywords usable as member names.
    fn expect_field_name(&mut self) -> Result<Ident, CompileError> {
        let span = self.peek_span();
        if let Some(name) = self.peek_kind().and_then(soft_keyword_to_ident) {
            self.advance();
            return Ok(Ident {
                name: name.into(),
                span,
            });
        }
        self.expect_ident()
    }

    /// Accept any keyword or identifier as a covergroup bin name.
    /// Bin names live inside a covergroup's own namespace and never
    /// resolve to a regular identifier in expression context — so even
    /// reserved tokens like `on`, `event`, `if`, etc. are safe to use
    /// as bin labels. Numeric/string literals and punctuation are
    /// rejected.
    fn expect_bin_name(&mut self) -> Result<Ident, CompileError> {
        let span = self.peek_span();
        let tok = self.advance().ok_or(CompileError::UnexpectedEof)?;
        let name = match &tok.kind {
            TokenKind::Ident(s) => s.clone(),
            other => {
                let s = other.to_string();
                let first = s.chars().next();
                if matches!(first, Some(c) if c.is_alphabetic() || c == '_') {
                    s
                } else {
                    return Err(CompileError::unexpected_token("bin name", &s, span));
                }
            }
        };
        Ok(Ident { name, span })
    }

    fn expect_ident_or_kw(&mut self) -> Result<Ident, CompileError> {
        // For attribute names, allow keywords-as-identifiers (e.g. `unique`, `dist`, `range`).
        let span = self.peek_span();
        let tok = self.advance().ok_or(CompileError::UnexpectedEof)?;
        let name = match tok.kind {
            TokenKind::Ident(s) => s,
            TokenKind::Unique => "unique".into(),
            TokenKind::Dist => "dist".into(),
            TokenKind::Default => "default".into(),
            TokenKind::Inside => "inside".into(),
            other => {
                return Err(CompileError::unexpected_token(
                    "identifier",
                    &other.to_string(),
                    span,
                ));
            }
        };
        Ok(Ident { name, span })
    }

    fn parse_keep(&mut self) -> Result<Keep, CompileError> {
        let start = self.expect(TokenKind::Keep)?.span;
        let expr = self.parse_constraint_expr()?;
        let span = start.merge(expr.span);
        Ok(Keep { expr, span })
    }

    fn parse_when_subtype(&mut self) -> Result<WhenSubtype, CompileError> {
        let start = self.expect(TokenKind::When)?.span;
        let discriminant = self.parse_expr()?;
        let mut items = Vec::new();
        while !self.check_end_keyword() {
            items.push(self.parse_txn_body_item()?);
        }
        let end = self.expect_end_anon(TokenKind::When)?;
        Ok(WhenSubtype {
            discriminant,
            items,
            span: start.merge(end),
        })
    }

    fn parse_dist_entries(&mut self) -> Result<Vec<DistEntry>, CompileError> {
        self.expect(TokenKind::LBrace)?;
        let mut entries = Vec::new();
        while !self.check(TokenKind::RBrace) {
            let value = self.parse_expr()?;
            self.expect(TokenKind::ColonSlash)?;
            let weight = self.parse_expr()?;
            entries.push(DistEntry { value, weight });
            if self.check(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(entries)
    }

    // ── Relation ─────────────────────────────────────────────────────────────

    fn parse_relation(&mut self, doc: Option<String>) -> Result<RelationDecl, CompileError> {
        let start = self.expect(TokenKind::Relation)?.span;
        let name = self.expect_ident()?;
        let params = self.parse_paren_params()?;
        // Alias form: `relation X = expr` (but identifier-only since we already consumed the name).
        // Spec §4.2: `relation AxiAlignedBurst(t) = AxiBurstLegal(t) && t.addr % 64 == 0`
        let (body, end_span) = if self.check(TokenKind::Eq) {
            self.advance();
            let e = self.parse_expr()?;
            let s = e.span;
            (RelationBody::Alias(e), s)
        } else {
            let mut block = Vec::new();
            while !self.check_end_keyword() {
                let e = self.parse_expr()?;
                block.push(e);
            }
            let end = self.expect_end(TokenKind::Relation, &name.name)?;
            (RelationBody::Block(block), end)
        };
        Ok(RelationDecl {
            name,
            params,
            body,
            span: start.merge(end_span),
            doc,
        })
    }

    // ── Tseq ──────────────────────────────────────────────────────────────────

    fn parse_tseq(&mut self, doc: Option<String>) -> Result<TseqDecl, CompileError> {
        let start = self.expect(TokenKind::Tseq)?.span;
        let name = self.expect_ident()?;
        let params = if self.check(TokenKind::LParen) {
            self.parse_paren_params()?
        } else {
            Vec::new()
        };
        let return_ty = if self.check(TokenKind::RArrow) {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        let inner_doc = self.consume_inner_doc();
        // Body is a normal block, terminated by `end tseq Name`.
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end(TokenKind::Tseq, &name.name)?;
        let body = Block {
            stmts,
            span: body_start.merge(end),
        };
        Ok(TseqDecl {
            name,
            params,
            return_ty,
            body,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    // ── Component declarations ────────────────────────────────────────────────

    fn parse_component(
        &mut self,
        kind: ComponentKind,
        doc: Option<String>,
    ) -> Result<ComponentDecl, CompileError> {
        let start_kw = match kind {
            ComponentKind::Agent => TokenKind::Agent,
            ComponentKind::Env => TokenKind::Env,
            ComponentKind::Scoreboard => TokenKind::Scoreboard,
            ComponentKind::Sequencer => TokenKind::Sequencer,
            ComponentKind::Transactor => unreachable!(
                "ComponentKind::Transactor is a synthetic codegen-only kind; \
                 transactors enter the parser via parse_transactor, not parse_component"
            ),
            ComponentKind::Testbench => TokenKind::Testbench,
        };
        let start = self.expect(start_kw.clone())?.span;
        let name = self.expect_ident()?;
        let params = self.parse_optional_generic_params()?;
        let bound_to = if self.check(TokenKind::Bound) {
            self.advance();
            self.expect(TokenKind::To)?;
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        let inner_doc = self.consume_inner_doc();
        let mut items = Vec::new();
        while !self.check_end_keyword() {
            items.push(self.parse_component_item(kind, &name.name, &items)?);
        }
        let end = self.expect_end(start_kw, &name.name)?;
        Ok(ComponentDecl {
            kind,
            name,
            params,
            bound_to,
            items,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn parse_component_item(
        &mut self,
        component_kind: ComponentKind,
        component_name: &str,
        existing_items: &[ComponentItem],
    ) -> Result<ComponentItem, CompileError> {
        let doc = self.consume_outer_doc();
        match self.peek_kind() {
            Some(TokenKind::Setup | TokenKind::Check | TokenKind::Teardown)
                if component_kind == ComponentKind::Testbench =>
            {
                self.parse_testbench_lifecycle(component_name, existing_items)
            }
            Some(TokenKind::Run) if component_kind == ComponentKind::Testbench => {
                Err(CompileError::general(
                    "`run` belongs to a testcase; use `setup`, `check`, or `teardown` in `testbench`",
                    self.peek_span(),
                ))
            }
            Some(TokenKind::Setup | TokenKind::Check | TokenKind::Teardown | TokenKind::Run) => {
                Err(CompileError::general(
                    "lifecycle blocks are currently supported only inside `test`/`impl` and `testbench`",
                    self.peek_span(),
                ))
            }
            Some(TokenKind::Let) if component_kind == ComponentKind::Testbench => {
                Ok(ComponentItem::Field(self.parse_testbench_let_field(doc)?))
            }
            Some(TokenKind::Let) => Err(CompileError::unexpected_token(
                "`let` declarations are currently only supported directly inside `testbench` bodies",
                "let",
                self.peek_span(),
            )),
            Some(TokenKind::Connect) => Ok(ComponentItem::Connect(self.parse_connect_block()?)),
            Some(TokenKind::On) => Ok(ComponentItem::OnHandler(self.parse_on_handler()?)),
            Some(TokenKind::Thread) => Ok(ComponentItem::TargetTlmThread(
                self.parse_target_tlm_thread()?,
            )),
            Some(TokenKind::Hookable) => Ok(ComponentItem::Hookable(self.parse_hookable()?)),
            // `function name(...) ... end function` — non-hookable
            // method on the enclosing component. Stored in the same
            // AST slot as `hookable` (HookableMethod with
            // `is_hookable = false`); codegen suppresses the pre/post
            // hook-vector machinery for it. Docs/test-ergonomics.md
            // §3.2 covers the surface.
            Some(TokenKind::Function) => {
                Ok(ComponentItem::Hookable(self.parse_component_function()?))
            }
            Some(TokenKind::Apply) => Ok(ComponentItem::Apply(self.parse_apply()?)),
            Some(TokenKind::Watchdog) => Ok(ComponentItem::Watchdog(self.parse_watchdog()?)),
            _ => Ok(ComponentItem::Field(self.parse_component_field(doc)?)),
        }
    }

    fn parse_testbench_let_field(
        &mut self,
        doc: Option<String>,
    ) -> Result<ComponentField, CompileError> {
        let l = self.parse_let_stmt()?;
        if l.ty.is_none() {
            return Err(CompileError::general(
                "`let` inside a testbench must have an explicit type",
                l.span,
            ));
        }
        if l.bind || l.value.is_some() {
            return Err(CompileError::general(
                "`let` inside a testbench currently supports DUT declarations only; use `name : Type` for ordinary fields",
                l.span,
            ));
        }
        Ok(ComponentField {
            name: l.name,
            direction: None,
            ty: l.ty.expect("checked above"),
            bound_to: None,
            default: None,
            probes: l.probes,
            bind_remap: l.bind_remap,
            span: l.span,
            doc,
        })
    }

    /// Parse target-side TLM responder syntax inside a bound transactor:
    ///
    /// ```harc
    /// thread bus.read(addr: uint<8>)
    ///     return addr + 0x100
    /// end thread
    /// ```
    fn parse_target_tlm_thread(&mut self) -> Result<TargetTlmThread, CompileError> {
        let start = self.expect(TokenKind::Thread)?.span;
        let method = self.parse_target_tlm_thread_path()?;
        let params = self.parse_paren_params()?;
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end_anon(TokenKind::Thread)?;
        Ok(TargetTlmThread {
            method,
            params,
            body: Block {
                stmts,
                span: body_start.merge(end),
            },
            span: start.merge(end),
        })
    }

    fn parse_target_tlm_thread_path(&mut self) -> Result<Path, CompileError> {
        let first = if self.check(TokenKind::Bus) {
            let tok = self.advance().unwrap();
            Ident {
                name: "bus".into(),
                span: tok.span,
            }
        } else {
            self.expect_ident()?
        };
        let start = first.span;
        let mut segments = vec![first];
        let mut end = start;
        while self.check(TokenKind::Dot) {
            self.advance();
            let id = self.expect_ident()?;
            end = id.span;
            segments.push(id);
        }
        Ok(Path {
            segments,
            span: start.merge(end),
        })
    }

    /// Parse a `watchdog … end watchdog` declaration (spec §8.6).
    /// Three shapes:
    ///   * `watchdog disabled` — single-line opt-out
    ///   * `watchdog\nend watchdog` — defaults (period 1000, max_idle 10000)
    ///   * `watchdog\n[period N cycles]\n[max_idle M cycles]\n[body…]\nend watchdog`
    ///
    /// `period` and `max_idle` clauses, when present, must precede any
    /// other body statements. Their `<expr>` may be a literal or a
    /// reference to a component field — letting users override the
    /// budget per-test by initializing a `wdog_period`/`wdog_max_idle`
    /// field at test scope.
    fn parse_watchdog(&mut self) -> Result<WatchdogDecl, CompileError> {
        let start = self.expect(TokenKind::Watchdog)?.span;
        // Inline `disabled` opt-out.
        if self.check_ident("disabled") {
            let end = self.advance().unwrap().span;
            return Ok(WatchdogDecl {
                disabled: true,
                period: None,
                max_idle: None,
                body: Block {
                    stmts: Vec::new(),
                    span: start.merge(end),
                },
                span: start.merge(end),
            });
        }
        // Optional `period <expr> cycles` and `max_idle <expr> cycles`
        // clauses. Either order; both optional. The `cycles` /
        // `cycle` decoration is required (matches the `wait N cycles`
        // convention so the human-facing units are explicit).
        let mut period: Option<Expr> = None;
        let mut max_idle: Option<Expr> = None;
        loop {
            if self.check_ident("period") && period.is_none() {
                self.advance();
                let e = self.parse_expr()?;
                if self.check_ident("cycles") || self.check_ident("cycle") {
                    self.advance();
                }
                period = Some(e);
            } else if self.check_ident("max_idle") && max_idle.is_none() {
                self.advance();
                let e = self.parse_expr()?;
                if self.check_ident("cycles") || self.check_ident("cycle") {
                    self.advance();
                }
                max_idle = Some(e);
            } else {
                break;
            }
        }
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end_anon(TokenKind::Watchdog)?;
        Ok(WatchdogDecl {
            disabled: false,
            period,
            max_idle,
            body: Block {
                stmts,
                span: body_start.merge(end),
            },
            span: start.merge(end),
        })
    }

    /// Parse `transactor T#(generics) bound to BusType { items;
    /// when active { items } end when } end transactor T`. Same body
    /// shape as parse_component (driver/agent/monitor) but with an
    /// optional `when active` block separating active-only items
    /// from the always-present body. See spec §8.1.
    fn parse_transactor(&mut self, doc: Option<String>) -> Result<TransactorDecl, CompileError> {
        let start = self.expect(TokenKind::Transactor)?.span;
        let name = self.expect_ident()?;
        let params = self.parse_optional_generic_params()?;
        let bound_to = if self.check(TokenKind::Bound) {
            self.advance();
            self.expect(TokenKind::To)?;
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        let inner_doc = self.consume_inner_doc();

        // Body: items in any order; at most one `when active` block.
        // Active-block items are collected separately so codegen can
        // emit them under `generate_if ACTIVE`.
        let mut items: Vec<ComponentItem> = Vec::new();
        let mut when_active: Option<Vec<ComponentItem>> = None;
        while !self.check_end_keyword() {
            // Recognize `when active` as a special block delimiter.
            // Plain `when` (already in HARC for transaction subtype
            // matching, etc.) keeps its existing meaning everywhere
            // else; here we peek for `When + Active` specifically.
            if self.check(TokenKind::When) && self.peek2_kind() == Some(&TokenKind::Active) {
                if when_active.is_some() {
                    return Err(CompileError::general(
                        "transactor body has more than one `when active` block; only one is allowed".into(),
                        self.peek_span(),
                    ));
                }
                self.advance(); // consume `when`
                self.advance(); // consume `active`
                let mut active_items: Vec<ComponentItem> = Vec::new();
                while !(self.check(TokenKind::End) && self.peek2_kind() == Some(&TokenKind::When)) {
                    active_items.push(self.parse_component_item(
                        ComponentKind::Transactor,
                        &name.name,
                        &active_items,
                    )?);
                }
                self.expect(TokenKind::End)?;
                self.expect(TokenKind::When)?;
                when_active = Some(active_items);
            } else {
                items.push(self.parse_component_item(
                    ComponentKind::Transactor,
                    &name.name,
                    &items,
                )?);
            }
        }
        let end = self.expect_end(TokenKind::Transactor, &name.name)?;
        Ok(TransactorDecl {
            name,
            params,
            bound_to,
            items,
            when_active,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn parse_component_field(
        &mut self,
        doc: Option<String>,
    ) -> Result<ComponentField, CompileError> {
        let start = self.peek_span();
        let name = self.expect_field_name()?;
        self.expect(TokenKind::Colon)?;
        // Direction: `in` / `out` / `inout` (kw `in` / ident `out`/`inout`).
        let direction = match self.peek_kind() {
            Some(TokenKind::In) => {
                self.advance();
                Some(Direction::In)
            }
            Some(TokenKind::Ident(s)) if s == "out" => {
                self.advance();
                Some(Direction::Out)
            }
            Some(TokenKind::Ident(s)) if s == "inout" => {
                self.advance();
                Some(Direction::InOut)
            }
            _ => None,
        };
        let mut ty = self.parse_type_expr()?;
        // Optional transactor mode annotation on a component field
        // (env / agent / sequencer body):
        //     drv : AxilXactor active
        //     mon : AxilXactor passive
        // Same shape as the let-statement form (parse_let_stmt). Only
        // valid on a Named type; codegen validates the referenced type
        // is a `transactor`.
        let mode = match self.peek_kind() {
            Some(TokenKind::Active) => {
                self.advance();
                Some(TransactorMode::Active)
            }
            Some(TokenKind::Passive) => {
                self.advance();
                Some(TransactorMode::Passive)
            }
            _ => None,
        };
        if let Some(m) = mode {
            if let TypeExpr::Named {
                mode: existing_mode,
                ..
            } = &mut ty
            {
                *existing_mode = Some(m);
            } else {
                return Err(CompileError::general(
                    "active/passive mode annotation only applies to a named (transactor) type"
                        .into(),
                    self.peek_span(),
                ));
            }
        }
        let bound_to = if self.check(TokenKind::Bound) {
            self.advance();
            self.expect(TokenKind::To)?;
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        // Accept either `= <expr>` or the transaction-style `default <expr>`.
        let default = if self.check(TokenKind::Eq) {
            self.advance();
            Some(self.parse_expr()?)
        } else if self.check(TokenKind::Default) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let end = default
            .as_ref()
            .map(|e| e.span)
            .or(bound_to.as_ref().map(|t| t.span()))
            .unwrap_or(ty.span());
        Ok(ComponentField {
            name,
            direction,
            ty,
            bound_to,
            default,
            probes: Vec::new(),
            bind_remap: Vec::new(),
            span: start.merge(end),
            doc,
        })
    }

    fn parse_connect_block(&mut self) -> Result<ConnectBlock, CompileError> {
        let start = self.expect(TokenKind::Connect)?.span;
        let mut edges = Vec::new();
        while !self.check_end_keyword() {
            let from = self.parse_expr()?;
            self.expect(TokenKind::RArrow)?;
            let to = self.parse_expr()?;
            let span = from.span.merge(to.span);
            edges.push(ConnectEdge { from, to, span });
        }
        let end = self.expect_end_anon(TokenKind::Connect)?;
        Ok(ConnectBlock {
            edges,
            span: start.merge(end),
        })
    }

    fn parse_on_handler(&mut self) -> Result<OnHandler, CompileError> {
        let start = self.expect(TokenKind::On)?.span;
        let event = self.parse_expr()?;
        // `on <N> cycles ... end on` — periodic trigger form (spec
        // §7.10). Fires the body once every `<N>` primary-clock
        // cycles. The `cycles` / `cycle` decoration is required (it's
        // what distinguishes this from a boolean trigger expression
        // that happens to be an integer); without it, an `on 1000 ...`
        // would mean "fire when the integer 1000 transitions to true",
        // which is nonsense.
        let periodic = if self.check_ident("cycles") || self.check_ident("cycle") {
            self.advance();
            true
        } else {
            false
        };
        let hook = match self.peek_kind() {
            Some(TokenKind::Pre) => {
                self.advance();
                Some(HookSide::Pre)
            }
            Some(TokenKind::Post) => {
                self.advance();
                Some(HookSide::Post)
            }
            _ => None,
        };
        let phase = if self.check(TokenKind::Phase) {
            let phase_span = self.advance().map(|t| t.span).unwrap_or(start);
            let name = self.expect_ident()?;
            match name.name.as_str() {
                "post_eval" => OnPhase::PostEval,
                "checker" => OnPhase::Checker,
                other => {
                    let message = format!(
                        "unknown on-handler phase `{other}`; expected `post_eval` or `checker`"
                    );
                    return Err(CompileError::general(&message, phase_span.merge(name.span)));
                }
            }
        } else {
            OnPhase::Checker
        };
        // Optional edge-mode keyword for cycle-trigger form: `rising` /
        // `falling` / `level`. Ident-tokens, not reserved keywords (so a
        // user can still name a variable `level` outside trigger context).
        // Periodic handlers ignore the edge mode (always fire on the
        // counter-match), but the parser still accepts it for symmetry.
        let edge = if self.check_ident("rising") {
            self.advance();
            EdgeMode::Rising
        } else if self.check_ident("falling") {
            self.advance();
            EdgeMode::Falling
        } else if self.check_ident("level") {
            self.advance();
            EdgeMode::Level
        } else {
            EdgeMode::Rising
        };
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end_anon(TokenKind::On)?;
        let body = Block {
            stmts,
            span: body_start.merge(end),
        };
        Ok(OnHandler {
            event,
            hook,
            edge,
            phase,
            body,
            span: start.merge(end),
            periodic,
        })
    }

    fn parse_hookable(&mut self) -> Result<HookableMethod, CompileError> {
        let start = self.expect(TokenKind::Hookable)?.span;
        let name = self.expect_ident()?;
        let params = self.parse_paren_params()?;
        let return_ty = if self.check(TokenKind::RArrow) {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end(TokenKind::End, &name.name).or_else(|_| {
            // Allow `end <ident>` form (no specific keyword) — rare.
            Ok::<Span, CompileError>(body_start)
        })?;
        Ok(HookableMethod {
            name,
            params,
            return_ty,
            body: Block {
                stmts,
                span: body_start.merge(end),
            },
            span: start.merge(end),
            is_hookable: true,
        })
    }

    /// Parse `function name(params) [-> Type] ... end function [name]`
    /// inside a component body (testbench / env / agent / sequencer).
    /// Same shape as `hookable`, but the resulting `HookableMethod`
    /// carries `is_hookable = false` so codegen skips the pre/post
    /// hook-vector emission and the corresponding fan-out in the
    /// method body. See docs/test-ergonomics.md §3.2.
    ///
    /// The `end` form accepts both `end function` and `end function
    /// <name>` (matching the open form), with the same lenient
    /// fallback as `parse_hookable`.
    fn parse_component_function(&mut self) -> Result<HookableMethod, CompileError> {
        let start = self.expect(TokenKind::Function)?.span;
        let name = self.expect_ident()?;
        let params = self.parse_paren_params()?;
        let return_ty = if self.check(TokenKind::RArrow) {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self
            .expect_end(TokenKind::Function, &name.name)
            .or_else(|_| Ok::<Span, CompileError>(body_start))?;
        Ok(HookableMethod {
            name,
            params,
            return_ty,
            body: Block {
                stmts,
                span: body_start.merge(end),
            },
            span: start.merge(end),
            is_hookable: false,
        })
    }

    // ── Test ──────────────────────────────────────────────────────────────────

    fn parse_test(&mut self, doc: Option<String>) -> Result<TestDecl, CompileError> {
        let start = self.expect(TokenKind::Test)?.span;
        let name = self.expect_ident()?;
        let params = if self.check(TokenKind::LParen) {
            self.parse_paren_params()?
        } else {
            Vec::new()
        };
        let inner_doc = self.consume_inner_doc();
        let mut items = Vec::new();

        // Per docs/test-ergonomics.md, the four reserved phase blocks
        // (`run` / `setup` / `check` / `teardown`) may appear directly
        // inside a `test ... end test` body, collapsing the
        // two-block `test T { ... } / impl sim for T { ... }` form
        // into one. Inline phases accumulate into a single synthetic
        // `TestItem::Scope`, preserving the codegen shape used for
        // lifecycle blocks.
        let mut inline_scope = ScopeDecl {
            name: Ident {
                name: "sim".into(),
                span: start,
            },
            setup: None,
            run: None,
            check: None,
            teardown: None,
            span: start,
        };
        let mut saw_inline_phase = false;

        while !self.check_end_keyword() {
            match self.peek_kind() {
                Some(TokenKind::Run) => {
                    let kw_span = self.advance().unwrap().span;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end_anon(TokenKind::Run)?;
                    if inline_scope.run.is_some() {
                        return Err(CompileError::general(
                            &format!("duplicate `run` block in test `{}`", name.name),
                            kw_span,
                        ));
                    }
                    inline_scope.run = Some(Block {
                        stmts,
                        span: end_span,
                    });
                    inline_scope.span = end_span;
                    saw_inline_phase = true;
                }
                Some(TokenKind::Setup) => {
                    let kw_span = self.advance().unwrap().span;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end_anon(TokenKind::Setup)?;
                    if inline_scope.setup.is_some() {
                        return Err(CompileError::general(
                            &format!("duplicate `setup` block in test `{}`", name.name),
                            kw_span,
                        ));
                    }
                    inline_scope.setup = Some(Block {
                        stmts,
                        span: end_span,
                    });
                    inline_scope.span = end_span;
                    saw_inline_phase = true;
                }
                Some(TokenKind::Check) => {
                    let kw_span = self.advance().unwrap().span;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end_anon(TokenKind::Check)?;
                    if inline_scope.check.is_some() {
                        return Err(CompileError::general(
                            &format!("duplicate `check` block in test `{}`", name.name),
                            kw_span,
                        ));
                    }
                    inline_scope.check = Some(Block {
                        stmts,
                        span: end_span,
                    });
                    inline_scope.span = end_span;
                    saw_inline_phase = true;
                }
                Some(TokenKind::Teardown) => {
                    let kw_span = self.advance().unwrap().span;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end_anon(TokenKind::Teardown)?;
                    if inline_scope.teardown.is_some() {
                        return Err(CompileError::general(
                            &format!("duplicate `teardown` block in test `{}`", name.name),
                            kw_span,
                        ));
                    }
                    inline_scope.teardown = Some(Block {
                        stmts,
                        span: end_span,
                    });
                    inline_scope.span = end_span;
                    saw_inline_phase = true;
                }
                Some(TokenKind::Phase) => {
                    self.advance();
                    let phase_name = self.expect_ident()?;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end(TokenKind::Phase, &phase_name.name)?;
                    items.push(TestItem::Phase(
                        phase_name,
                        Block {
                            stmts,
                            span: end_span,
                        },
                    ));
                }
                _ => {
                    items.push(self.parse_test_item()?);
                }
            }
        }
        if saw_inline_phase {
            items.push(TestItem::Scope(inline_scope));
        }
        let end = self.expect_end(TokenKind::Test, &name.name)?;
        Ok(TestDecl {
            name,
            params,
            items,
            span: start.merge(end),
            doc,
            inner_doc,
            for_testbench: None,
        })
    }

    /// Parse `impl <name> for <TbType> { run/setup/check/teardown/phase }
    /// end impl <name>` — the testbench-bound test form
    /// (docs/test-ergonomics.md §3.3). Body items are the same as
    /// `parse_test` accepts (phases + bare statements form the
    /// implicit `run`), but the test does NOT carry user-visible
    /// `let dut` / `let tb` declarations — those are derived from
    /// the bound testbench's field list at codegen time.
    ///
    /// Lowered through the same `TestDecl` AST as the classic `test`
    /// form, with `for_testbench: Some(TbType)`. The classic form's
    /// codegen ignores the field; the new form's codegen uses it to
    /// emit a per-test Tb instance and to fold testbench fields /
    /// helper methods into the run-body scope.
    fn parse_impl_for_test(&mut self, doc: Option<String>) -> Result<TestDecl, CompileError> {
        let start = self.expect(TokenKind::Impl)?.span;
        let name = self.expect_ident()?;
        self.expect(TokenKind::For)?;
        let tb_ty = self.expect_ident()?;
        let inner_doc = self.consume_inner_doc();

        // Body — same shape as `parse_test`. Bare statements +
        // run/setup/check/teardown/phase blocks accumulate into the
        // items list. Lets at impl scope are accepted (rare but legal
        // — e.g. test-local helpers that aren't testbench fields).
        let mut items: Vec<TestItem> = Vec::new();
        let mut inline_scope = ScopeDecl {
            name: Ident {
                name: "sim".into(),
                span: start,
            },
            setup: None,
            run: None,
            check: None,
            teardown: None,
            span: start,
        };
        let mut saw_inline_phase = false;
        while !self.check_end_keyword() {
            match self.peek_kind() {
                Some(TokenKind::Run) => {
                    let kw_span = self.advance().unwrap().span;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end_anon(TokenKind::Run)?;
                    if inline_scope.run.is_some() {
                        return Err(CompileError::general(
                            &format!("duplicate `run` block in impl `{}`", name.name),
                            kw_span,
                        ));
                    }
                    inline_scope.run = Some(Block {
                        stmts,
                        span: end_span,
                    });
                    inline_scope.span = end_span;
                    saw_inline_phase = true;
                }
                Some(TokenKind::Setup) => {
                    let kw_span = self.advance().unwrap().span;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end_anon(TokenKind::Setup)?;
                    if inline_scope.setup.is_some() {
                        return Err(CompileError::general(
                            &format!("duplicate `setup` block in impl `{}`", name.name),
                            kw_span,
                        ));
                    }
                    inline_scope.setup = Some(Block {
                        stmts,
                        span: end_span,
                    });
                    inline_scope.span = end_span;
                    saw_inline_phase = true;
                }
                Some(TokenKind::Check) => {
                    let kw_span = self.advance().unwrap().span;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end_anon(TokenKind::Check)?;
                    if inline_scope.check.is_some() {
                        return Err(CompileError::general(
                            &format!("duplicate `check` block in impl `{}`", name.name),
                            kw_span,
                        ));
                    }
                    inline_scope.check = Some(Block {
                        stmts,
                        span: end_span,
                    });
                    inline_scope.span = end_span;
                    saw_inline_phase = true;
                }
                Some(TokenKind::Teardown) => {
                    let kw_span = self.advance().unwrap().span;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end_anon(TokenKind::Teardown)?;
                    if inline_scope.teardown.is_some() {
                        return Err(CompileError::general(
                            &format!("duplicate `teardown` block in impl `{}`", name.name),
                            kw_span,
                        ));
                    }
                    inline_scope.teardown = Some(Block {
                        stmts,
                        span: end_span,
                    });
                    inline_scope.span = end_span;
                    saw_inline_phase = true;
                }
                Some(TokenKind::Phase) => {
                    self.advance();
                    let phase_name = self.expect_ident()?;
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end_span = self.expect_end(TokenKind::Phase, &phase_name.name)?;
                    items.push(TestItem::Phase(
                        phase_name,
                        Block {
                            stmts,
                            span: end_span,
                        },
                    ));
                }
                _ => {
                    items.push(self.parse_test_item()?);
                }
            }
        }
        if saw_inline_phase {
            items.push(TestItem::Scope(inline_scope));
        }
        let end = self.expect_end(TokenKind::Impl, &name.name)?;
        Ok(TestDecl {
            name,
            params: Vec::new(),
            items,
            span: start.merge(end),
            doc,
            inner_doc,
            for_testbench: Some(tb_ty),
        })
    }

    fn parse_test_item(&mut self) -> Result<TestItem, CompileError> {
        match self.peek_kind() {
            Some(TokenKind::Apply) => Ok(TestItem::Apply(self.parse_apply()?)),
            Some(TokenKind::Let) => Ok(TestItem::Let(self.parse_let_stmt()?)),
            Some(TokenKind::Use) => Ok(TestItem::Use(self.parse_use(None)?)),
            Some(TokenKind::ClockGen) => Ok(TestItem::Clock(self.parse_clock_decl()?)),
            // `scope sim` was the legacy lifecycle wrapper. Lifecycle
            // blocks now live directly inside `test` (spec §7.2). The
            // token still lexes; surface a clear error rather than
            // accept it silently.
            Some(TokenKind::Scope) => Err(CompileError::unexpected_token(
                "`run` / `setup` / `check` / `teardown` directly inside `test` \
                 (the legacy `scope sim` block was removed — see spec §7.2)",
                "scope",
                self.peek_span(),
            )),
            // Anything else: a bare statement, treated as implicit `run`.
            Some(_) => Ok(TestItem::Stmt(self.parse_stmt()?)),
            None => Err(CompileError::UnexpectedEof),
        }
    }

    fn parse_testbench_lifecycle(
        &mut self,
        component_name: &str,
        existing_items: &[ComponentItem],
    ) -> Result<ComponentItem, CompileError> {
        let start = self.peek_span();
        let (phase, stmts, end_span) = match self.peek_kind() {
            Some(TokenKind::Setup) => {
                self.advance();
                let stmts = self.parse_stmt_list_until_end()?;
                let end_span = self.expect_end_anon(TokenKind::Setup)?;
                (LifecyclePhase::Setup, stmts, end_span)
            }
            Some(TokenKind::Check) => {
                self.advance();
                let stmts = self.parse_stmt_list_until_end()?;
                let end_span = self.expect_end_anon(TokenKind::Check)?;
                (LifecyclePhase::Check, stmts, end_span)
            }
            Some(TokenKind::Teardown) => {
                self.advance();
                let stmts = self.parse_stmt_list_until_end()?;
                let end_span = self.expect_end_anon(TokenKind::Teardown)?;
                (LifecyclePhase::Teardown, stmts, end_span)
            }
            _ => unreachable!("parse_testbench_lifecycle called for non-lifecycle token"),
        };

        // Typed duplicate check: with the new variant shape this is a
        // direct phase comparison, no field-of-ScopeDecl inspection.
        let duplicate = existing_items.iter().any(|it| {
            matches!(it, ComponentItem::Lifecycle(p, _) if *p == phase)
        });
        if duplicate {
            return Err(CompileError::general(
                &format!(
                    "duplicate `{}` block in testbench `{component_name}`",
                    phase.keyword(),
                ),
                start,
            ));
        }

        let body = Block {
            stmts,
            span: end_span,
        };
        Ok(ComponentItem::Lifecycle(phase, body))
    }

    fn parse_clock_decl(&mut self) -> Result<ClockDecl, CompileError> {
        let start = self.expect(TokenKind::ClockGen)?.span;
        let name = self.expect_field_name()?;
        self.expect(TokenKind::Eq)?;
        let period = self.parse_expr()?;
        let span = start.merge(period.span);
        Ok(ClockDecl {
            name,
            period,
            span,
            doc: None,
        })
    }

    fn parse_apply(&mut self) -> Result<ApplyDecl, CompileError> {
        let start = self.expect(TokenKind::Apply)?.span;
        let path = self.parse_dotted_path()?;
        let span = start.merge(path.span);
        Ok(ApplyDecl { path, span })
    }

    // The legacy `impl <target> for <Test>` block (Item::Impl, ImplDecl,
    // ImplItem) was removed alongside its parser entry in Phase 2 of
    // docs/test-ergonomics.md. Inline-form phase blocks inside `test`
    // (handled by `parse_test`) are the canonical surface.

    // ── Extend ────────────────────────────────────────────────────────────────

    fn parse_extend(&mut self, doc: Option<String>) -> Result<ExtendDecl, CompileError> {
        let start = self.expect(TokenKind::Extend)?.span;
        let target = self.parse_dotted_path()?;
        let inner_doc = self.consume_inner_doc();
        // Pick the body grammar from the first body token. Test-style extends
        // start with `scope`/`apply`/`use`; component-style start with
        // `connect`/`on`/`hookable`; everything else is txn/struct-style. All
        // three are unambiguous at one-token lookahead.
        let body = match self.peek_kind() {
            // Test-style extend: items are scope decls / applies / uses /
            // statements (incl. `assert`/`assume`/`cover`/`log`/`wait`/etc.)
            Some(TokenKind::Scope)
            | Some(TokenKind::Apply)
            | Some(TokenKind::Use)
            | Some(TokenKind::Let)
            | Some(TokenKind::Assert)
            | Some(TokenKind::Assume)
            | Some(TokenKind::Cover)
            | Some(TokenKind::Log)
            | Some(TokenKind::LogF)
            | Some(TokenKind::Wait)
            | Some(TokenKind::For)
            | Some(TokenKind::Repeat)
            | Some(TokenKind::Loop)
            | Some(TokenKind::While)
            | Some(TokenKind::Break)
            | Some(TokenKind::Continue)
            | Some(TokenKind::If)
            | Some(TokenKind::Fork)
            | Some(TokenKind::Randomize)
            | Some(TokenKind::On)
            | Some(TokenKind::Emit) => {
                let mut items = Vec::new();
                while !self.check_end_keyword() {
                    items.push(self.parse_test_item()?);
                }
                ExtendBody::Test(items)
            }
            // `On` would be reachable here too, but the test-body arm above
            // already claims it (the dispatcher disambiguates structurally,
            // not by target-kind lookup). Keep the component-only tokens.
            Some(TokenKind::Connect) | Some(TokenKind::Hookable) => {
                let mut items = Vec::new();
                while !self.check_end_keyword() {
                    items.push(self.parse_component_item(
                        ComponentKind::Env,
                        "<extend>",
                        &items,
                    )?);
                }
                ExtendBody::Component(items)
            }
            _ => {
                let mut items = Vec::new();
                while !self.check_end_keyword() {
                    items.push(self.parse_txn_body_item()?);
                }
                ExtendBody::TxnLike(items)
            }
        };
        // Closing form: `end extend Name` (single-segment) or `end extend Pkg.Name`.
        let end_tok = self.expect(TokenKind::End)?;
        let kw_tok = self.advance().ok_or(CompileError::UnexpectedEof)?;
        if kw_tok.kind != TokenKind::Extend {
            return Err(CompileError::mismatched_kind(
                "extend",
                &kw_tok.kind.to_string(),
                kw_tok.span,
            ));
        }
        let _close_path = self.parse_dotted_path()?;
        let span = start.merge(end_tok.span);
        Ok(ExtendDecl {
            target,
            body,
            span,
            doc,
            inner_doc,
        })
    }

    // ── Covergroup ────────────────────────────────────────────────────────────

    fn parse_covergroup(&mut self, doc: Option<String>) -> Result<CovergroupDecl, CompileError> {
        let start = self.expect(TokenKind::Covergroup)?.span;
        let name = self.expect_ident()?;
        let trigger = if self.check(TokenKind::AtSign) {
            self.advance();
            self.expect(TokenKind::LParen)?;
            // Allow optional `posedge`/`negedge` identifier prefix (SVA-style).
            // We model this as a Call: `posedge(clk)` so the AST stays uniform.
            let e = if self.check_ident("posedge") || self.check_ident("negedge") {
                let edge = self.expect_ident()?;
                let arg = self.parse_expr()?;
                let span = edge.span.merge(arg.span);
                let callee = Expr::new(ExprKind::Ident(edge), span);
                Expr::new(
                    ExprKind::Call {
                        callee,
                        args: vec![CallArg::Expr(arg)],
                    },
                    span,
                )
            } else {
                self.parse_expr()?
            };
            let hook_side = match self.peek_kind() {
                Some(TokenKind::Pre) => {
                    self.advance();
                    Some(HookSide::Pre)
                }
                Some(TokenKind::Post) => {
                    self.advance();
                    Some(HookSide::Post)
                }
                _ => None,
            };
            self.expect(TokenKind::RParen)?;
            if let Some(side) = hook_side {
                self.validate_cover_hook_trigger(&e)?;
                Some(CoverTrigger::Hook { call: e, side })
            } else {
                Some(CoverTrigger::Clock(e))
            }
        } else {
            None
        };
        let inner_doc = self.consume_inner_doc();
        let mut items = Vec::new();
        while !self.check_end_keyword() {
            items.push(self.parse_cover_item()?);
        }
        let end = self.expect_end(TokenKind::Covergroup, &name.name)?;
        Ok(CovergroupDecl {
            name,
            trigger,
            items,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn validate_cover_hook_trigger(&self, e: &Expr) -> Result<(), CompileError> {
        let ExprKind::Call { args, .. } = &*e.kind else {
            return Err(CompileError::general(
                "covergroup hook trigger must be a method call before `pre` or `post`",
                e.span,
            ));
        };
        for arg in args {
            let CallArg::Expr(expr) = arg else {
                return Err(CompileError::general(
                    "covergroup hook trigger arguments must be identifiers",
                    e.span,
                ));
            };
            if !matches!(&*expr.kind, ExprKind::Ident(_)) {
                return Err(CompileError::general(
                    "covergroup hook trigger arguments must be identifiers",
                    expr.span,
                ));
            }
        }
        Ok(())
    }

    fn parse_cover_item(&mut self) -> Result<CoverItem, CompileError> {
        if self.check(TokenKind::Cross) {
            let start = self.advance().unwrap().span;
            let mut points = Vec::new();
            points.push(self.expect_ident()?);
            while self.check(TokenKind::Comma) {
                self.advance();
                points.push(self.expect_ident()?);
            }
            let end = points.last().map(|p| p.span).unwrap_or(start);
            Ok(CoverItem::Cross(CoverCross {
                points,
                span: start.merge(end),
            }))
        } else {
            // `name : cover <expr> [bins ... end bins]`
            let name = self.expect_ident()?;
            let start = name.span;
            self.expect(TokenKind::Colon)?;
            self.expect(TokenKind::Cover)?;
            let target = self.parse_expr()?;
            let mut bins = Vec::new();
            if self.check(TokenKind::Bins) {
                self.advance();
                while !self.check_end_keyword() {
                    // Bin names live inside a covergroup namespace and never
                    // appear as expression-position identifiers, so accept
                    // any keyword (`on`, `event`, `state`, `default`, ...)
                    // as a bin name verbatim — its source form becomes the
                    // string identifier.
                    let bn = self.expect_bin_name()?;
                    self.expect(TokenKind::Eq)?;
                    let spec = self.parse_expr()?;
                    let span = bn.span.merge(spec.span);
                    bins.push(CoverBin {
                        name: bn,
                        spec,
                        span,
                    });
                }
                self.expect_end_anon(TokenKind::Bins)?;
            }
            let end = bins.last().map(|b| b.span).unwrap_or(target.span);
            Ok(CoverItem::Point(CoverPoint {
                name,
                target,
                bins,
                span: start.merge(end),
            }))
        }
    }

    // ── Property / Pseq / Cover sequence ──────────────────────────────────────

    fn parse_property(&mut self, doc: Option<String>) -> Result<PropertyDecl, CompileError> {
        let start = self.expect(TokenKind::Property)?.span;
        let name = self.expect_ident()?;
        // `property foo(args)\n  body` — params attach if `(` is on the same
        // line as the name. A newline before `(` means the `(` is the start
        // of the body expression (e.g. `(dut.x == 15) |=> dut.y`).
        let params = if self.check(TokenKind::LParen) && !self.newline_before_peek(name.span.end) {
            self.parse_paren_params()?
        } else {
            Vec::new()
        };
        let inner_doc = self.consume_inner_doc();
        let body = self.parse_expr()?;
        let end = self.expect_end(TokenKind::Property, &name.name)?;
        Ok(PropertyDecl {
            name,
            params,
            body,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn parse_pseq(&mut self, doc: Option<String>) -> Result<PseqDecl, CompileError> {
        let start = self.expect(TokenKind::Pseq)?.span;
        let name = self.expect_ident()?;
        // Same newline-disambiguation as parse_property — see comment there.
        let params = if self.check(TokenKind::LParen) && !self.newline_before_peek(name.span.end) {
            self.parse_paren_params()?
        } else {
            Vec::new()
        };
        let inner_doc = self.consume_inner_doc();
        let body = self.parse_expr()?;
        let end = self.expect_end(TokenKind::Pseq, &name.name)?;
        Ok(PseqDecl {
            name,
            params,
            body,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn parse_cover_sequence(
        &mut self,
        doc: Option<String>,
    ) -> Result<CoverSequenceDecl, CompileError> {
        let start = self.expect(TokenKind::Cover)?.span;
        self.expect(TokenKind::Sequence)?;
        let name = self.expect_ident()?;
        let inner_doc = self.consume_inner_doc();
        self.expect(TokenKind::Eq)?;
        let pattern = self.parse_expr()?;
        let span = start.merge(pattern.span);
        Ok(CoverSequenceDecl {
            name,
            pattern,
            span,
            doc,
            inner_doc,
        })
    }

    // ── External (Verilator-bound) module ─────────────────────────────────────

    /// Parse a `bus Name ... end bus Name` declaration. v0 surface:
    /// plain signals (`name: in|out Type;`) and `handshake_channel`
    /// groupings (`handshake_channel ch: send kind: valid_ready { ... }
    /// end handshake_channel ch`), and `tlm_method` declarations.
    /// `credit_channel` blocks are explicitly out of scope for v0 —
    /// those will need their own follow-up PRs.
    fn parse_bus(&mut self, doc: Option<String>) -> Result<BusDecl, CompileError> {
        let start = self.expect(TokenKind::Bus)?.span;
        let name = self.expect_ident()?;
        let inner_doc = self.consume_inner_doc();
        let mut signals = Vec::new();
        let mut handshakes = Vec::new();
        let mut tlm_methods = Vec::new();
        while !self.check_end_keyword() {
            // `param NAME: const = default;` — bus-level parameter.
            // Parsed but ignored at the AST level for v0; the stdlib
            // bus types (BusAxiLite, BusApb, BusAxiStream) all ship
            // with these and we want extern-import to succeed.
            if self.check(TokenKind::Param) {
                self.advance();
                self.expect_ident()?; // param name
                self.expect(TokenKind::Colon)?;
                if self.check(TokenKind::Const) {
                    self.advance();
                } else if self.check(TokenKind::Type) {
                    self.advance();
                }
                if self.check(TokenKind::Eq) {
                    self.advance();
                    let _default = self.parse_expr()?;
                }
                if self.check(TokenKind::Semi) {
                    self.advance();
                }
                continue;
            }
            // `handshake_channel <name>: send|receive kind: <variant>`.
            if self.check_ident("handshake_channel") {
                self.advance();
                let h_name = self.expect_ident()?;
                self.expect(TokenKind::Colon)?;
                let role = if self.check_ident("send") {
                    self.advance();
                    HandshakeRole::Send
                } else if self.check_ident("receive") {
                    self.advance();
                    HandshakeRole::Receive
                } else {
                    return Err(CompileError::unexpected_token(
                        "`send` or `receive`",
                        &self.peek_kind().map(|k| k.to_string()).unwrap_or_default(),
                        self.peek_span(),
                    ));
                };
                // `kind: <variant>` per arch §19.2. `kind` is a
                // reserved HARC keyword (token), not a soft ident, so
                // match the token directly.
                if self.check(TokenKind::Kind) {
                    self.advance();
                    self.expect(TokenKind::Colon)?;
                }
                let variant = self.expect_ident()?;
                let mut payload = Vec::new();
                while !self.check_end_keyword() {
                    let s_name = self.expect_ident()?;
                    self.expect(TokenKind::Colon)?;
                    let ty = self.parse_type_expr()?;
                    if self.check(TokenKind::Semi) {
                        self.advance();
                    }
                    let span = s_name.span.merge(ty.span());
                    payload.push(BusSignal {
                        name: s_name,
                        direction: Direction::Out, // role flips at lower-time
                        ty,
                        span,
                    });
                }
                self.expect(TokenKind::End)?;
                if self.check_ident("handshake_channel") {
                    self.advance();
                }
                if self.check_ident(&h_name.name) {
                    self.advance();
                }
                let span = h_name.span;
                handshakes.push(HandshakeChannel {
                    name: h_name,
                    role,
                    variant,
                    payload,
                    span,
                });
                continue;
            }
            // `tlm_method read(addr: uint<32>) -> uint<64>: blocking;`
            if self.check_ident("tlm_method") {
                tlm_methods.push(self.parse_tlm_method_decl()?);
                continue;
            }
            // Plain signal: `name: in|out Type;`
            let s_name = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let direction = if self.check_ident("in") {
                self.advance();
                Direction::In
            } else if self.check_ident("out") {
                self.advance();
                Direction::Out
            } else {
                return Err(CompileError::unexpected_token(
                    "`in` or `out`",
                    &self.peek_kind().map(|k| k.to_string()).unwrap_or_default(),
                    self.peek_span(),
                ));
            };
            let ty = self.parse_type_expr()?;
            if self.check(TokenKind::Semi) {
                self.advance();
            }
            let span = s_name.span.merge(ty.span());
            signals.push(BusSignal {
                name: s_name,
                direction,
                ty,
                span,
            });
        }
        let end = self.expect_end(TokenKind::Bus, &name.name)?;
        Ok(BusDecl {
            name,
            params: Vec::new(),
            signals,
            handshakes,
            tlm_methods,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    /// Parse a `tlm_method` declaration inside a bus body. Matches ARCH's
    /// current surface: `blocking` and `out_of_order tags N` are accepted in
    /// the AST; direct HARC codegen currently lowers only `blocking` calls.
    fn parse_tlm_method_decl(&mut self) -> Result<TlmMethod, CompileError> {
        let start = self.advance().unwrap().span;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;
        let mut args = Vec::new();
        if !self.check(TokenKind::RParen) {
            loop {
                let arg_name = self.expect_ident()?;
                self.expect(TokenKind::Colon)?;
                let arg_ty = self.parse_type_expr()?;
                args.push((arg_name, arg_ty));
                if self.check(TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        self.expect(TokenKind::RParen)?;
        let ret = if self.check(TokenKind::RArrow) {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        self.expect(TokenKind::Colon)?;
        let mode = if self.check(TokenKind::Blocking) {
            let tok = self.advance().unwrap();
            Ident {
                name: "blocking".into(),
                span: tok.span,
            }
        } else {
            self.expect_ident()?
        };
        let out_of_order_tags = if mode.name == "out_of_order" {
            if self.check_ident("tags") {
                self.advance();
            } else {
                return Err(CompileError::unexpected_token(
                    "`tags`",
                    &self.peek_kind().map(|k| k.to_string()).unwrap_or_default(),
                    self.peek_span(),
                ));
            }
            Some(self.parse_expr()?)
        } else {
            None
        };
        let end_span = if self.check(TokenKind::Semi) {
            self.advance().unwrap().span
        } else {
            mode.span
        };
        if mode.name != "blocking" && mode.name != "out_of_order" {
            return Err(CompileError::general(
                &format!(
                    "tlm_method concurrency mode `{}` is not implemented — use `blocking` or `out_of_order tags N`.",
                    mode.name
                ),
                mode.span,
            ));
        }
        if mode.name == "blocking" && out_of_order_tags.is_some() {
            return Err(CompileError::general(
                "`tags` is only valid on `out_of_order` TLM methods",
                mode.span,
            ));
        }
        Ok(TlmMethod {
            name,
            args,
            ret,
            mode,
            out_of_order_tags,
            span: start.merge(end_span),
        })
    }

    // ── Register Abstraction Layer (RAL) ──────────────────────────────────────

    /// Parse:
    ///   regblock <Name> via <Helper> [width <N>]
    ///       register <Name> @ <addr> [width <N>] [reset <V>] [access <Policy>]
    ///       ...
    ///   end regblock <Name>
    ///
    /// Phase 1a: registers only (no field-level decomposition); single
    /// access policy (`rw`); helper-routed protocol (`via <Transactor>`),
    /// not direct `bound to <Bus>`. See docs/ral-support.md.
    fn parse_regblock(&mut self, doc: Option<String>) -> Result<RegblockDecl, CompileError> {
        let start = self.expect(TokenKind::Regblock)?.span;
        let name = self.expect_ident()?;
        self.expect(TokenKind::Via)?;
        let via_helper = self.expect_ident()?;
        let default_width = if self.check_ident("width") {
            self.advance();
            Some(self.expect_uint_literal("width")?)
        } else {
            None
        };
        let inner_doc = self.consume_inner_doc();
        let mut registers = Vec::new();
        while !self.check_end_keyword() {
            registers.push(self.parse_register()?);
        }
        let end = self.expect_end(TokenKind::Regblock, &name.name)?;
        Ok(RegblockDecl {
            name,
            via_helper,
            default_width,
            registers,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn parse_register(&mut self) -> Result<RegisterDecl, CompileError> {
        let doc = self.consume_outer_doc();
        let start = self.expect(TokenKind::Register)?.span;
        let name = self.expect_ident()?;
        self.expect(TokenKind::AtSign)?;
        let offset = self.parse_expr()?;

        let mut width: Option<u32> = None;
        let mut reset: Option<Expr> = None;
        let mut access = RegAccess::Rw;
        let mut end = offset.span;
        loop {
            if self.check_ident("width") {
                self.advance();
                width = Some(self.expect_uint_literal("width")?);
                end = self.prev_span_or(end);
            } else if self.check_ident("reset") {
                self.advance();
                let e = self.parse_expr()?;
                end = e.span;
                reset = Some(e);
            } else if self.check_ident("access") {
                self.advance();
                let kw = self.expect_ident_or_kw()?;
                access = match kw.name.as_str() {
                    "rw" => RegAccess::Rw,
                    "ro" => RegAccess::Ro,
                    "wo" => RegAccess::Wo,
                    other => {
                        return Err(CompileError::general(
                            &format!(
                                "register access policy `{other}` not supported yet \
                                 (`rw`/`ro`/`wo` ship; `w1c`/`w1s`/`wclr`/`wset`/`rc`/`rs` \
                                 follow per docs/ral-support.md)"
                            ),
                            kw.span,
                        ));
                    }
                };
                end = kw.span;
            } else {
                break;
            }
        }

        // Optional field block: presence of a `field` keyword switches
        // the register into block form. Closed by `end register
        // [<Name>]`. Single-line registers (no `field` keyword) leave
        // `fields` empty and don't expect a closer — the next
        // `register` keyword or the regblock's `end regblock` ends them.
        let mut fields: Vec<FieldDecl> = Vec::new();
        if self.check(TokenKind::Field) {
            while self.check(TokenKind::Field) {
                fields.push(self.parse_register_field(access)?);
            }
            end = self.expect_end(TokenKind::Register, &name.name)?;
        }

        Ok(RegisterDecl {
            name,
            offset,
            width,
            reset,
            access,
            fields,
            span: start.merge(end),
            doc,
        })
    }

    /// Parse a single `field <name> : <ty> @ <bit_pos> [reset <v>]
    /// [access <policy>]` declaration inside a `register` block. The
    /// `parent_access` argument supplies the access policy when the
    /// field decl doesn't override it explicitly.
    /// Parse:
    ///   addrmap <Name> via <Helper>
    ///       instance <name> : <RegblockType> @ <base_addr>
    ///       ...
    ///   end addrmap <Name>
    ///
    /// Phase 1e: flat container only — no nested addrmaps, no
    /// `alias of`, no per-instance bus override. See
    /// docs/ral-support.md §4.
    fn parse_addrmap(&mut self, doc: Option<String>) -> Result<AddrmapDecl, CompileError> {
        let start = self.expect(TokenKind::Addrmap)?.span;
        let name = self.expect_ident()?;
        self.expect(TokenKind::Via)?;
        let via_helper = self.expect_ident()?;
        let inner_doc = self.consume_inner_doc();
        let mut instances = Vec::new();
        while !self.check_end_keyword() {
            instances.push(self.parse_addrmap_instance()?);
        }
        let end = self.expect_end(TokenKind::Addrmap, &name.name)?;
        Ok(AddrmapDecl {
            name,
            via_helper,
            instances,
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    fn parse_addrmap_instance(&mut self) -> Result<InstanceDecl, CompileError> {
        let doc = self.consume_outer_doc();
        let start = self.expect(TokenKind::Instance)?.span;
        let name = self.expect_ident()?;
        self.expect(TokenKind::Colon)?;
        let regblock_ty = self.expect_ident()?;
        self.expect(TokenKind::AtSign)?;
        let base_addr = self.parse_expr()?;
        // Optional `size <expr>` and `alias of <ident>` clauses,
        // either order. Both are soft keywords (parsed via
        // `check_ident`) so they don't clash with the identifiers of
        // the same name used elsewhere (e.g. AXI transaction fields).
        let mut size: Option<Expr> = None;
        let mut alias_of: Option<Ident> = None;
        let mut end = base_addr.span;
        loop {
            if self.check_ident("size") && size.is_none() {
                self.advance();
                let e = self.parse_expr()?;
                end = e.span;
                size = Some(e);
            } else if self.check_ident("alias") && alias_of.is_none() {
                self.advance();
                if !self.check_ident("of") {
                    return Err(CompileError::general(
                        "expected `of` after `alias` in instance clause",
                        self.peek_span(),
                    ));
                }
                self.advance();
                let target = self.expect_ident()?;
                end = target.span;
                alias_of = Some(target);
            } else {
                break;
            }
        }
        Ok(InstanceDecl {
            name,
            regblock_ty,
            base_addr,
            size,
            alias_of,
            span: start.merge(end),
            doc,
        })
    }

    fn parse_register_field(
        &mut self,
        parent_access: RegAccess,
    ) -> Result<FieldDecl, CompileError> {
        let doc = self.consume_outer_doc();
        let start = self.expect(TokenKind::Field)?.span;
        let name = self.expect_ident()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type_expr()?;
        self.expect(TokenKind::AtSign)?;
        let bit_pos = self.expect_uint_literal("bit position")?;
        let mut reset: Option<Expr> = None;
        let mut access = parent_access;
        let mut end = ty.span();
        loop {
            if self.check_ident("reset") {
                self.advance();
                let e = self.parse_expr()?;
                end = e.span;
                reset = Some(e);
            } else if self.check_ident("access") {
                self.advance();
                let kw = self.expect_ident_or_kw()?;
                access = match kw.name.as_str() {
                    "rw" => RegAccess::Rw,
                    "ro" => RegAccess::Ro,
                    "wo" => RegAccess::Wo,
                    other => {
                        return Err(CompileError::general(
                            &format!(
                                "field access policy `{other}` not supported yet \
                                 (`rw`/`ro`/`wo` ship; `w1c`/`w1s`/`wclr`/`wset`/`rc`/`rs` \
                                 follow per docs/ral-support.md)"
                            ),
                            kw.span,
                        ));
                    }
                };
                end = kw.span;
            } else {
                break;
            }
        }
        Ok(FieldDecl {
            name,
            ty,
            bit_pos,
            reset,
            access,
            span: start.merge(end),
            doc,
        })
    }

    /// Helper: consume the next token as a `uint` decimal literal and return
    /// its value as `u32`. Used for `width <N>` clauses that need a numeric
    /// width, not an arbitrary expression.
    fn expect_uint_literal(&mut self, label: &str) -> Result<u32, CompileError> {
        let span = self.peek_span();
        let e = self.parse_expr()?;
        if let ExprKind::Int(s) = &*e.kind {
            s.replace('_', "").parse::<u32>().map_err(|_| {
                CompileError::general(
                    &format!("`{label}` value must be a positive integer (got `{s}`)"),
                    span,
                )
            })
        } else {
            Err(CompileError::general(
                &format!("`{label}` requires an integer literal"),
                span,
            ))
        }
    }

    /// Helper: span of the previously-consumed token, fallback to `prev` if
    /// the parser hasn't advanced since `prev` was captured.
    fn prev_span_or(&self, prev: Span) -> Span {
        // The lexer doesn't expose a public "previous token span"; the
        // caller usually has the most recently consumed token's span. This
        // helper is mostly cosmetic for span-merging during error reports.
        prev
    }

    fn parse_external_module(
        &mut self,
        doc: Option<String>,
    ) -> Result<ExternalModuleDecl, CompileError> {
        let start = self.expect(TokenKind::Module)?.span;
        let name = self.expect_ident()?;
        self.expect(TokenKind::Kind)?;
        let kind = self.expect_ident()?;
        let mut fields = Vec::new();
        while !self.check_end_keyword() {
            let fname = self.expect_ident_or_kw()?;
            self.expect(TokenKind::Colon)?;
            let value = self.parse_expr()?;
            let span = fname.span.merge(value.span);
            fields.push(ExternalField {
                name: fname,
                value,
                span,
            });
        }
        let end = self.expect_end(TokenKind::Module, &name.name)?;
        Ok(ExternalModuleDecl {
            name,
            kind,
            fields,
            span: start.merge(end),
            doc,
        })
    }

    // ── Functions ─────────────────────────────────────────────────────────────

    fn parse_function(&mut self, doc: Option<String>) -> Result<FunctionDecl, CompileError> {
        let start = self.expect(TokenKind::Function)?.span;
        let name = self.expect_ident()?;
        let params = self.parse_paren_params()?;
        let return_ty = if self.check(TokenKind::RArrow) {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        let inner_doc = self.consume_inner_doc();
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end(TokenKind::Function, &name.name)?;
        Ok(FunctionDecl {
            name,
            params,
            return_ty,
            body: Block {
                stmts,
                span: body_start.merge(end),
            },
            span: start.merge(end),
            doc,
            inner_doc,
        })
    }

    /// `extern function <name>(<params>) [-> <ret>]` — forward-declares
    /// a C / C++ reference function (spec §9). No body, no
    /// `end function` — the declaration terminates at the return type
    /// (or after the param list, for a void function). The
    /// implementation lives in a separate source file passed via
    /// `harc sim --ref-src <file>`.
    fn parse_extern_fn(&mut self, doc: Option<String>) -> Result<ExternFnDecl, CompileError> {
        let start = self.expect(TokenKind::Extern)?.span;
        // `extern` is only valid as a prefix to `function` in v0.
        // Reject anything else with a clear error.
        if !self.check(TokenKind::Function) {
            return Err(CompileError::unexpected_token(
                "`function` after `extern`",
                &self
                    .peek_kind()
                    .map(|k| k.to_string())
                    .unwrap_or_else(|| "EOF".into()),
                self.peek_span(),
            ));
        }
        self.advance();
        let name = self.expect_ident()?;
        let params = self.parse_paren_params()?;
        let return_ty = if self.check(TokenKind::RArrow) {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        let end_span = return_ty
            .as_ref()
            .map(|t| t.span())
            .unwrap_or_else(|| params.last().map(|p| p.span).unwrap_or(name.span));
        Ok(ExternFnDecl {
            name,
            params,
            return_ty,
            span: start.merge(end_span),
            doc,
        })
    }

    // ── Generic / function parameters ─────────────────────────────────────────

    fn parse_optional_generic_params(&mut self) -> Result<Vec<Param>, CompileError> {
        if self.check(TokenKind::Hash) && matches!(self.peek2_kind(), Some(TokenKind::LParen)) {
            self.advance(); // #
            self.parse_paren_params()
        } else {
            Ok(Vec::new())
        }
    }

    fn parse_paren_params(&mut self) -> Result<Vec<Param>, CompileError> {
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        while !self.check(TokenKind::RParen) {
            params.push(self.parse_param()?);
            if self.check(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(TokenKind::RParen)?;
        Ok(params)
    }

    fn parse_param(&mut self) -> Result<Param, CompileError> {
        let name = if self.check(TokenKind::Underscore) {
            let span = self.advance().unwrap().span;
            Ident {
                name: "_".into(),
                span,
            }
        } else {
            self.expect_ident()?
        };
        let start = name.span;
        let mut ty = None;
        let mut default = None;
        if self.check(TokenKind::Colon) {
            self.advance();
            ty = Some(self.parse_type_expr()?);
        }
        if self.check(TokenKind::Eq) {
            self.advance();
            default = Some(self.parse_expr()?);
        }
        let end = default
            .as_ref()
            .map(|e| e.span)
            .or(ty.as_ref().map(|t| t.span()))
            .unwrap_or(start);
        Ok(Param {
            name,
            ty,
            default,
            span: start.merge(end),
        })
    }

    // ── Type expressions ──────────────────────────────────────────────────────

    pub fn parse_type_expr(&mut self) -> Result<TypeExpr, CompileError> {
        let span0 = self.peek_span();
        match self.peek_kind() {
            Some(TokenKind::UIntKw) => {
                self.parse_builtin_ty(BuiltinTy::UInt, TokenKind::UIntKw, true)
            }
            Some(TokenKind::SIntKw) => {
                self.parse_builtin_ty(BuiltinTy::SInt, TokenKind::SIntKw, true)
            }
            Some(TokenKind::BitsKw) => {
                self.parse_builtin_ty(BuiltinTy::Bits, TokenKind::BitsKw, true)
            }
            Some(TokenKind::UInt) => {
                self.parse_builtin_ty(BuiltinTy::UIntCap, TokenKind::UInt, true)
            }
            Some(TokenKind::SInt) => {
                self.parse_builtin_ty(BuiltinTy::SIntCap, TokenKind::SInt, true)
            }
            Some(TokenKind::Bool) => self.consume_atomic_ty(BuiltinTy::Bool, TokenKind::Bool),
            Some(TokenKind::BoolLower) => {
                self.consume_atomic_ty(BuiltinTy::BoolLower, TokenKind::BoolLower)
            }
            Some(TokenKind::Bit) => self.consume_atomic_ty(BuiltinTy::Bit, TokenKind::Bit),
            Some(TokenKind::Int) => {
                let start = self.expect(TokenKind::Int)?.span;
                if self.check(TokenKind::Lt) {
                    return Err(CompileError::unsupported_syntax(
                        "`int<N>` is not HARC syntax",
                        "Use `sint<N>` for explicit-width signed values, or plain `int` for the unqualified scalar type.",
                        start.merge(self.peek_span()),
                    ));
                }
                Ok(TypeExpr::Builtin {
                    name: BuiltinTy::Int,
                    args: Vec::new(),
                    span: start,
                })
            }
            Some(TokenKind::Time) => self.consume_atomic_ty(BuiltinTy::Time, TokenKind::Time),
            Some(TokenKind::Prop) => self.consume_atomic_ty(BuiltinTy::Prop, TokenKind::Prop),
            Some(TokenKind::Pseq) => self.consume_atomic_ty(BuiltinTy::Pseq, TokenKind::Pseq),
            Some(TokenKind::SeverityTy) => {
                self.consume_atomic_ty(BuiltinTy::Severity, TokenKind::SeverityTy)
            }
            Some(TokenKind::LoggerTy) => {
                self.consume_atomic_ty(BuiltinTy::Logger, TokenKind::LoggerTy)
            }
            Some(TokenKind::StringTy) => {
                self.consume_atomic_ty(BuiltinTy::String, TokenKind::StringTy)
            }
            Some(TokenKind::Clock) => self.consume_atomic_ty(BuiltinTy::Clock, TokenKind::Clock),
            Some(TokenKind::Reset) => self.consume_atomic_ty(BuiltinTy::Reset, TokenKind::Reset),
            Some(TokenKind::KwVec) => self.parse_builtin_ty(BuiltinTy::Vec, TokenKind::KwVec, true),
            Some(TokenKind::TSeqTy) => {
                self.parse_builtin_ty(BuiltinTy::TSeq, TokenKind::TSeqTy, true)
            }
            Some(TokenKind::Event) => {
                self.advance();
                // `event comb<T>` vs `event<T>`
                let kind = if self.check(TokenKind::Comb) {
                    self.advance();
                    BuiltinTy::EventComb
                } else {
                    BuiltinTy::Event
                };
                let args = if self.check(TokenKind::Lt) {
                    self.parse_type_arg_list()?
                } else {
                    Vec::new()
                };
                let end = args.last().map(arg_span).unwrap_or(span0);
                Ok(TypeExpr::Builtin {
                    name: kind,
                    args,
                    span: span0.merge(end),
                })
            }
            Some(TokenKind::Buffer) => {
                self.parse_builtin_ty(BuiltinTy::Buffer, TokenKind::Buffer, true)
            }
            Some(TokenKind::Stream) => {
                self.parse_builtin_ty(BuiltinTy::Stream, TokenKind::Stream, true)
            }
            Some(TokenKind::State) => {
                self.parse_builtin_ty(BuiltinTy::State, TokenKind::State, true)
            }
            Some(TokenKind::Queue) => {
                self.parse_builtin_ty(BuiltinTy::Queue, TokenKind::Queue, true)
            }
            Some(TokenKind::Ident(_)) => {
                let path = self.parse_dotted_path()?;
                let mut generics = Vec::new();
                let mut span = path.span;
                if self.check(TokenKind::Hash)
                    && matches!(self.peek2_kind(), Some(TokenKind::LParen))
                {
                    self.advance(); // #
                    self.expect(TokenKind::LParen)?;
                    while !self.check(TokenKind::RParen) {
                        let arg = self.parse_type_arg(true)?;
                        generics.push(arg);
                        if self.check(TokenKind::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    let close = self.expect(TokenKind::RParen)?.span;
                    span = span.merge(close);
                } else if self.check(TokenKind::Lt) {
                    let g = self.parse_type_arg_list()?;
                    let last = g.last().map(arg_span).unwrap_or(span);
                    generics = g;
                    span = span.merge(last);
                }
                Ok(TypeExpr::Named {
                    name: path,
                    generics,
                    mode: None,
                    span,
                })
            }
            Some(other) => Err(CompileError::unexpected_token(
                "type",
                &other.to_string(),
                span0,
            )),
            None => Err(CompileError::UnexpectedEof),
        }
    }

    fn consume_atomic_ty(
        &mut self,
        name: BuiltinTy,
        tok: TokenKind,
    ) -> Result<TypeExpr, CompileError> {
        let span = self.expect(tok)?.span;
        Ok(TypeExpr::Builtin {
            name,
            args: Vec::new(),
            span,
        })
    }

    fn parse_builtin_ty(
        &mut self,
        name: BuiltinTy,
        tok: TokenKind,
        _angle: bool,
    ) -> Result<TypeExpr, CompileError> {
        let start = self.expect(tok)?.span;
        let args = if self.check(TokenKind::Lt) {
            self.parse_type_arg_list()?
        } else {
            Vec::new()
        };
        let end = args.last().map(arg_span).unwrap_or(start);
        Ok(TypeExpr::Builtin {
            name,
            args,
            span: start.merge(end),
        })
    }

    fn parse_type_arg_list(&mut self) -> Result<Vec<TypeArg>, CompileError> {
        self.expect(TokenKind::Lt)?;
        let prev = self.no_angle;
        self.no_angle = true;
        let mut args = Vec::new();
        while !self.is_close_angle() {
            args.push(self.parse_type_arg(false)?);
            if self.check(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.no_angle = prev;
        self.expect_close_angle()?;
        Ok(args)
    }

    /// True if the next token closes a generic — either a plain `>` or the
    /// first half of a `>>` (lexed as a single `Shr` token from nested
    /// generics like `queue<uint<32>>`).
    fn is_close_angle(&self) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Gt) | Some(TokenKind::Shr))
    }

    /// Consume a closing `>`. If the next token is `>>` (Shr — produced by
    /// the lexer when two `>`s sit adjacent), split it: take the first
    /// `>` and rewrite the remaining position to a fresh `Gt` so the
    /// outer generic can consume it next.
    fn expect_close_angle(&mut self) -> Result<Span, CompileError> {
        match self.peek_kind() {
            Some(TokenKind::Gt) => Ok(self.advance().unwrap().span),
            Some(TokenKind::Shr) => {
                let span = self.peek_span();
                let half = Span::new(span.start, span.start + 1);
                // Rewrite this token in place to be a single `>` whose span
                // covers the second char; do NOT advance past it.
                self.tokens[self.pos].kind = TokenKind::Gt;
                self.tokens[self.pos].span = Span::new(span.start + 1, span.end);
                Ok(half)
            }
            Some(other) => Err(CompileError::unexpected_token(
                ">",
                &other.to_string(),
                self.peek_span(),
            )),
            None => Err(CompileError::UnexpectedEof),
        }
    }

    fn parse_type_arg(&mut self, paren_form: bool) -> Result<TypeArg, CompileError> {
        // Named: `Name = expr` or `depth=N` (LL(1) — peek 2 tokens for `IDENT =`).
        if matches!(self.peek_kind(), Some(TokenKind::Ident(_)))
            && matches!(self.peek2_kind(), Some(TokenKind::Eq))
        {
            let name = self.expect_ident()?;
            self.expect(TokenKind::Eq)?;
            let value = self.parse_expr()?;
            return Ok(TypeArg::Named { name, value });
        }
        // Otherwise: try a type expression first, fall back to expression for value-args.
        // Heuristic: if leading token introduces a type (uint/sint/bits/Vec/Bool/etc.), parse a type.
        let starts_type = matches!(
            self.peek_kind(),
            Some(
                TokenKind::UIntKw
                    | TokenKind::SIntKw
                    | TokenKind::BitsKw
                    | TokenKind::UInt
                    | TokenKind::SInt
                    | TokenKind::Bool
                    | TokenKind::Bit
                    | TokenKind::Int
                    | TokenKind::BoolLower
                    | TokenKind::Time
                    | TokenKind::Prop
                    | TokenKind::Pseq
                    | TokenKind::SeverityTy
                    | TokenKind::LoggerTy
                    | TokenKind::StringTy
                    | TokenKind::Clock
                    | TokenKind::Reset
                    | TokenKind::KwVec
                    | TokenKind::Event
                    | TokenKind::Buffer
                    | TokenKind::Stream
                    | TokenKind::State
                    | TokenKind::Queue
                    | TokenKind::TSeqTy
            )
        );
        if starts_type {
            let ty = self.parse_type_expr()?;
            return Ok(TypeArg::Type(ty));
        }
        // Identifier: could be a named type or a value expression. Parse expression — it covers both.
        let _ = paren_form;
        let e = self.parse_expr()?;
        Ok(TypeArg::Expr(e))
    }

    // ── Statements / Block ────────────────────────────────────────────────────

    fn parse_stmt_list_until_end(&mut self) -> Result<Vec<Stmt>, CompileError> {
        let mut stmts = Vec::new();
        while !self.is_block_terminator() {
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
    }

    fn is_block_terminator(&self) -> bool {
        match self.peek_kind() {
            Some(TokenKind::End) => true,
            Some(TokenKind::Else) | Some(TokenKind::ElsIf) => true,
            Some(TokenKind::Branch) => false, // branch is opener inside fork
            None => true,
            _ => false,
        }
    }

    fn parse_stmt(&mut self) -> Result<Stmt, CompileError> {
        // Skip stray doc — statements don't carry doc strings here.
        let _ = self.consume_outer_doc();
        let start = self.peek_span();
        match self.peek_kind() {
            Some(TokenKind::Let) => {
                let l = self.parse_let_stmt()?;
                Ok(Stmt {
                    kind: StmtKind::Let(l.clone()),
                    span: l.span,
                })
            }
            Some(TokenKind::For) => {
                let s = self.parse_for_stmt()?;
                let span = s.span;
                Ok(Stmt {
                    kind: StmtKind::For(s),
                    span,
                })
            }
            Some(TokenKind::Repeat) => {
                let s = self.parse_repeat_stmt()?;
                let span = s.span;
                Ok(Stmt {
                    kind: StmtKind::Repeat(s),
                    span,
                })
            }
            Some(TokenKind::Loop) => {
                self.advance();
                let body_start = self.peek_span();
                let stmts = self.parse_stmt_list_until_end()?;
                let end = self.expect_end_anon(TokenKind::Loop)?;
                Ok(Stmt {
                    kind: StmtKind::Loop(Block {
                        stmts,
                        span: body_start.merge(end),
                    }),
                    span: start.merge(end),
                })
            }
            Some(TokenKind::While) => {
                self.advance();
                let cond = self.parse_expr()?;
                let body_start = self.peek_span();
                let stmts = self.parse_stmt_list_until_end()?;
                let end = self.expect_end_anon(TokenKind::While)?;
                let span = start.merge(end);
                Ok(Stmt {
                    kind: StmtKind::While {
                        cond,
                        body: Block {
                            stmts,
                            span: body_start.merge(end),
                        },
                        span,
                    },
                    span,
                })
            }
            Some(TokenKind::Break) => {
                let s = self.expect(TokenKind::Break)?.span;
                Ok(Stmt {
                    kind: StmtKind::Break { span: s },
                    span: s,
                })
            }
            Some(TokenKind::Continue) => {
                let s = self.expect(TokenKind::Continue)?.span;
                Ok(Stmt {
                    kind: StmtKind::Continue { span: s },
                    span: s,
                })
            }
            Some(TokenKind::If) => {
                let s = self.parse_if_stmt()?;
                let span = s.span;
                Ok(Stmt {
                    kind: StmtKind::If(s),
                    span,
                })
            }
            Some(TokenKind::Fork) => {
                let s = self.parse_fork_stmt()?;
                let span = s.span;
                Ok(Stmt {
                    kind: StmtKind::Fork(s),
                    span,
                })
            }
            Some(TokenKind::JoinAll) => {
                let span = self.advance().unwrap().span;
                Ok(Stmt {
                    kind: StmtKind::JoinAll { span },
                    span,
                })
            }
            Some(TokenKind::Parallel) => {
                self.advance();
                let mut branches = Vec::new();
                while !self.check_end_keyword() {
                    branches.push(self.parse_inline_block_until_terminator()?);
                }
                let end = self.expect_end_anon(TokenKind::Parallel)?;
                Ok(Stmt {
                    kind: StmtKind::Parallel(branches),
                    span: start.merge(end),
                })
            }
            Some(TokenKind::Schedule) => {
                self.advance();
                let mut branches = Vec::new();
                while !self.check_end_keyword() {
                    branches.push(self.parse_inline_block_until_terminator()?);
                }
                let end = self.expect_end_anon(TokenKind::Schedule)?;
                Ok(Stmt {
                    kind: StmtKind::Schedule(branches),
                    span: start.merge(end),
                })
            }
            Some(TokenKind::Select) => {
                self.advance();
                let mut arms = Vec::new();
                while !self.check_end_keyword() {
                    let event = self.parse_expr()?;
                    self.expect(TokenKind::FatArrow)?;
                    let action_stmt = self.parse_stmt()?;
                    let span = event.span.merge(action_stmt.span);
                    let action = Block {
                        stmts: vec![action_stmt],
                        span,
                    };
                    arms.push(SelectArm {
                        event,
                        action,
                        span,
                    });
                }
                let end = self.expect_end_anon(TokenKind::Select)?;
                Ok(Stmt {
                    kind: StmtKind::Select(arms),
                    span: start.merge(end),
                })
            }
            Some(TokenKind::On) => {
                let h = self.parse_on_handler()?;
                let span = h.span;
                Ok(Stmt {
                    kind: StmtKind::On(h),
                    span,
                })
            }
            Some(TokenKind::Emit) => {
                self.advance();
                let path = self.parse_dotted_path()?;
                let mut args = Vec::new();
                if self.check(TokenKind::LParen) {
                    self.advance();
                    while !self.check(TokenKind::RParen) {
                        args.push(self.parse_call_arg()?);
                        if self.check(TokenKind::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                }
                let span = start.merge(path.span);
                Ok(Stmt {
                    kind: StmtKind::Emit {
                        name: path,
                        args,
                        span,
                    },
                    span,
                })
            }
            Some(TokenKind::Yield) => {
                self.advance();
                let e = self.parse_expr()?;
                let span = start.merge(e.span);
                Ok(Stmt {
                    kind: StmtKind::Yield(e),
                    span,
                })
            }
            Some(TokenKind::Return) => {
                self.advance();
                if self.is_block_terminator() {
                    Ok(Stmt {
                        kind: StmtKind::Return(None),
                        span: start,
                    })
                } else {
                    let e = self.parse_expr()?;
                    let span = start.merge(e.span);
                    Ok(Stmt {
                        kind: StmtKind::Return(Some(e)),
                        span,
                    })
                }
            }
            Some(TokenKind::Apply) => {
                let a = self.parse_apply()?;
                let span = a.span;
                Ok(Stmt {
                    kind: StmtKind::Apply(a),
                    span,
                })
            }
            Some(TokenKind::Release) => {
                // `release <expr>` — disable a `probe force` signal's
                // SV procedural force. See docs/probe-signals.md.
                self.advance();
                let e = self.parse_expr()?;
                let span = start.merge(e.span);
                Ok(Stmt {
                    kind: StmtKind::Release(e),
                    span,
                })
            }
            Some(TokenKind::Assert) => {
                self.advance();
                // `property` keyword is allowed in all three roles per spec
                // §5 (`assert property`, `assume property`, `cover property`).
                let v = self.parse_verify(true)?;
                let span = start.merge(v.span);
                Ok(Stmt {
                    kind: StmtKind::Assert(v),
                    span,
                })
            }
            Some(TokenKind::Fail) => {
                // `fail("...")` standalone statement. Same message slot
                // as the `else fail("...")` clause of `assert`, just
                // unconditional. Useful when the failure trigger is
                // structural (inside `if`/`for` flow control) rather
                // than expressible as a single boolean predicate.
                self.advance();
                self.expect(TokenKind::LParen)?;
                let msg = self.parse_expr()?;
                let close = self.expect(TokenKind::RParen)?.span;
                let span = start.merge(close);
                Ok(Stmt {
                    kind: StmtKind::Fail { msg, span },
                    span,
                })
            }
            Some(TokenKind::Assume) => {
                self.advance();
                let v = self.parse_verify(true)?;
                let span = start.merge(v.span);
                Ok(Stmt {
                    kind: StmtKind::Assume(v),
                    span,
                })
            }
            Some(TokenKind::Cover) => {
                self.advance();
                let v = self.parse_verify(true)?;
                let span = start.merge(v.span);
                Ok(Stmt {
                    kind: StmtKind::Cover(v),
                    span,
                })
            }
            Some(TokenKind::Randomize) => {
                self.advance();
                let target_e = self.parse_paren_expr_one()?;
                let mut with_body = Vec::new();
                if self.check(TokenKind::With) {
                    self.advance();
                    while !self.check_end_keyword() {
                        with_body.push(self.parse_constraint_expr()?);
                    }
                    self.expect_end_anon(TokenKind::Randomize)?;
                }
                let span = start.merge(target_e.span);
                Ok(Stmt {
                    kind: StmtKind::Randomize {
                        blocking: false,
                        target: target_e,
                        with_body,
                    },
                    span,
                })
            }
            Some(TokenKind::Blocking) => {
                self.advance();
                self.expect(TokenKind::Randomize)?;
                let target_e = self.parse_paren_expr_one()?;
                let mut with_body = Vec::new();
                if self.check(TokenKind::With) {
                    self.advance();
                    while !self.check_end_keyword() {
                        with_body.push(self.parse_constraint_expr()?);
                    }
                    self.expect_end_anon(TokenKind::Randomize)?;
                }
                let span = start.merge(target_e.span);
                Ok(Stmt {
                    kind: StmtKind::Randomize {
                        blocking: true,
                        target: target_e,
                        with_body,
                    },
                    span,
                })
            }
            Some(TokenKind::Log) | Some(TokenKind::LogF) => {
                let is_logf = matches!(self.peek_kind(), Some(TokenKind::LogF));
                self.advance();
                self.expect(TokenKind::LParen)?;
                let mut args = Vec::new();
                while !self.check(TokenKind::RParen) {
                    args.push(self.parse_call_arg()?);
                    if self.check(TokenKind::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                let end = self.expect(TokenKind::RParen)?.span;
                let span = start.merge(end);
                let kind = if is_logf {
                    StmtKind::LogF { args, span }
                } else {
                    StmtKind::Log { args, span }
                };
                Ok(Stmt { kind, span })
            }
            Some(TokenKind::After) => {
                self.advance();
                let dur = self.parse_expr()?;
                // Optional `cycles` keyword — already lexed inside time literal, but
                // also accepted as a free word. We allow it as ident.
                if self.check_ident("cycles") {
                    self.advance();
                }
                let body_start = self.peek_span();
                let stmts = self.parse_stmt_list_until_end()?;
                let end = self.expect_end_anon(TokenKind::After)?;
                Ok(Stmt {
                    kind: StmtKind::After {
                        duration: dur,
                        body: Block {
                            stmts,
                            span: body_start.merge(end),
                        },
                        span: start.merge(end),
                    },
                    span: start.merge(end),
                })
            }
            Some(TokenKind::Wait) => {
                self.advance();
                // `wait until …` form (spec §7.9) — UVM's objection
                // mechanism replaced by a positive "wait until these
                // conditions hold" with optional timeout + per-predicate
                // diagnostics. Always-single-line; not a block stmt.
                if self.check_ident("until") {
                    self.advance();
                    // Optional `all of` / `any of` quantifier prefix.
                    let mode = if self.check_ident("all") {
                        self.advance();
                        if !self.check_ident("of") {
                            return Err(CompileError::unexpected_token(
                                "`of` after `all`",
                                &self
                                    .peek_kind()
                                    .map(|k| k.to_string())
                                    .unwrap_or("EOF".into()),
                                self.peek_span(),
                            ));
                        }
                        self.advance();
                        WaitUntilMode::AllOf
                    } else if self.check_ident("any") {
                        self.advance();
                        if !self.check_ident("of") {
                            return Err(CompileError::unexpected_token(
                                "`of` after `any`",
                                &self
                                    .peek_kind()
                                    .map(|k| k.to_string())
                                    .unwrap_or("EOF".into()),
                                self.peek_span(),
                            ));
                        }
                        self.advance();
                        WaitUntilMode::AnyOf
                    } else {
                        WaitUntilMode::Single
                    };
                    // Parse first condition. For all-of/any-of, accept
                    // additional comma-separated conditions until we hit
                    // `timeout` or statement end.
                    let mut conditions = vec![self.parse_expr()?];
                    if matches!(mode, WaitUntilMode::AllOf | WaitUntilMode::AnyOf) {
                        while self.check(TokenKind::Comma) {
                            self.advance();
                            conditions.push(self.parse_expr()?);
                        }
                    }
                    // Optional `timeout N cycles fail("…")` tail.
                    // Each clause is itself optional inside the timeout
                    // block: `timeout N cycles` (no message), `timeout N
                    // cycles fail("...")` (with message). The default
                    // message ("wait until timed out at cycle N") is
                    // supplied by codegen when `message` is None.
                    let timeout = if self.check_ident("timeout") {
                        let to_start = self.peek_span();
                        self.advance();
                        let cycles = self.parse_expr()?;
                        if self.check_ident("cycles") || self.check_ident("cycle") {
                            self.advance();
                        }
                        let message = if self.check(TokenKind::Fail) {
                            self.advance();
                            self.expect(TokenKind::LParen)?;
                            let msg = self.parse_expr()?;
                            self.expect(TokenKind::RParen)?;
                            Some(msg)
                        } else {
                            None
                        };
                        let to_end = message.as_ref().map(|m| m.span).unwrap_or(cycles.span);
                        Some(WaitTimeout {
                            cycles,
                            message,
                            span: to_start.merge(to_end),
                        })
                    } else {
                        None
                    };
                    let last_span = timeout
                        .as_ref()
                        .map(|t| t.span)
                        .or_else(|| conditions.last().map(|c| c.span))
                        .unwrap_or(start);
                    let span = start.merge(last_span);
                    return Ok(Stmt {
                        kind: StmtKind::WaitUntil {
                            mode,
                            conditions,
                            timeout,
                            span,
                        },
                        span,
                    });
                }
                let dur = self.parse_expr()?;
                // `cycles` / `cycle` decoration — optional, ARCH-shape.
                if self.check_ident("cycles") || self.check_ident("cycle") {
                    self.advance();
                }
                // Optional `on <clock>` clause — advances time so the named
                // clock sees N more rising edges (other clocks tick at
                // their natural rate). Useful for multi-clock CDC tests
                // where the assertion is "after N dst_clk cycles, X holds".
                let clock = if self.check(TokenKind::On) {
                    self.advance();
                    Some(self.expect_ident()?)
                } else {
                    None
                };
                let end_span = clock.as_ref().map(|c| c.span).unwrap_or(dur.span);
                let span = start.merge(end_span);
                Ok(Stmt {
                    kind: StmtKind::Wait {
                        duration: dur,
                        clock,
                        span,
                    },
                    span,
                })
            }
            _ => {
                // Expression-or-assignment statement.
                let lhs = self.parse_expr()?;
                if self.check(TokenKind::Eq) {
                    self.advance();
                    let rhs = self.parse_expr()?;
                    let span = lhs.span.merge(rhs.span);
                    Ok(Stmt {
                        kind: StmtKind::Assign {
                            target: lhs,
                            value: rhs,
                        },
                        span,
                    })
                } else if self.check(TokenKind::LArrow) {
                    self.advance();
                    let rhs = self.parse_expr()?;
                    let span = lhs.span.merge(rhs.span);
                    Ok(Stmt {
                        kind: StmtKind::Send {
                            target: lhs,
                            value: rhs,
                        },
                        span,
                    })
                } else {
                    let span = lhs.span;
                    Ok(Stmt {
                        kind: StmtKind::Expr(lhs),
                        span,
                    })
                }
            }
        }
    }

    fn parse_inline_block_until_terminator(&mut self) -> Result<Block, CompileError> {
        // For `parallel`/`schedule` whose branches are sub-statements without `branch` markers.
        // Each "branch" is a single statement at this level, lifted to a Block.
        let s = self.parse_stmt()?;
        let span = s.span;
        Ok(Block {
            stmts: vec![s],
            span,
        })
    }

    fn parse_let_stmt(&mut self) -> Result<LetStmt, CompileError> {
        let start = self.expect(TokenKind::Let)?.span;
        let name = if self.check(TokenKind::Underscore) {
            let span = self.advance().unwrap().span;
            Ident {
                name: "_".into(),
                span,
            }
        } else {
            self.expect_field_name()?
        };
        let ty = if self.check(TokenKind::Colon) {
            self.advance();
            let mut t = self.parse_type_expr()?;
            // Optional transactor mode annotation:
            //     let xact : AxilXactor active  = bind axil
            //     let obs  : AxilXactor passive = bind axil
            // Only valid on a Named type at the let-instantiation
            // grammar slot. The codegen later validates that the
            // referenced type is actually a `transactor` decl;
            // mode-on-non-transactor is a clear error there.
            let mode = match self.peek_kind() {
                Some(TokenKind::Active) => {
                    self.advance();
                    Some(TransactorMode::Active)
                }
                Some(TokenKind::Passive) => {
                    self.advance();
                    Some(TransactorMode::Passive)
                }
                _ => None,
            };
            if let Some(m) = mode {
                if let TypeExpr::Named {
                    mode: existing_mode,
                    ..
                } = &mut t
                {
                    *existing_mode = Some(m);
                } else {
                    return Err(CompileError::general(
                        "active/passive mode annotation only applies to a named (transactor) type"
                            .into(),
                        self.peek_span(),
                    ));
                }
            }
            Some(t)
        } else {
            None
        };
        let mut value = None;
        let mut bind = false;
        let mut bind_remap: Vec<BindRemapEntry> = Vec::new();
        if self.check(TokenKind::Eq) {
            self.advance();
            if self.check(TokenKind::Bind) {
                self.advance();
                bind = true;
                // Bind value can be a free expression: `bind dut.s_axi`.
                value = Some(self.parse_expr()?);
                // Optional per-signal remap clause:
                //   bind dut with { aw.valid: "awvalid", w.data: "wdata" }
                if self.check(TokenKind::With) {
                    self.advance();
                    self.expect(TokenKind::LBrace)?;
                    while !self.check(TokenKind::RBrace) {
                        bind_remap.push(self.parse_bind_remap_entry()?);
                        if self.check(TokenKind::Comma) {
                            self.advance();
                        }
                    }
                    self.expect(TokenKind::RBrace)?;
                }
            } else {
                value = Some(self.parse_expr()?);
            }
        }
        // Optional probe block: `let dut : T  probe N : T at p.q  ...  end let`.
        // Only valid when a type was given (no inferred-type lets); enforced
        // by the parser surfacing a clear error if `probe` appears with no
        // type. The block terminator is `end let [name]`, matching HARC's
        // standard `end <kw> <name>` closer convention.
        let mut probes = Vec::new();
        let mut probe_end_span = None;
        if self.check(TokenKind::Probe) {
            if ty.is_none() {
                return Err(CompileError::general(
                    "`probe` block requires the `let` to have a typed annotation \
                     (`let dut : <DutType>`)"
                        .into(),
                    self.peek_span(),
                ));
            }
            while self.check(TokenKind::Probe) {
                probes.push(self.parse_probe_decl()?);
            }
            probe_end_span = Some(self.expect_end(TokenKind::Let, &name.name)?);
        }

        let end = probe_end_span
            .or_else(|| value.as_ref().map(|e| e.span))
            .or_else(|| ty.as_ref().map(|t| t.span()))
            .unwrap_or(name.span);
        Ok(LetStmt {
            name,
            ty,
            value,
            bind,
            probes,
            bind_remap,
            span: start.merge(end),
        })
    }

    /// Parse one `<dotted.path>: "<port-name>"` entry inside a
    /// `bind ... with { ... }` block.
    fn parse_bind_remap_entry(&mut self) -> Result<BindRemapEntry, CompileError> {
        let start = self.peek_span();
        let first = self.expect_field_name()?;
        let mut path = vec![first];
        while self.check(TokenKind::Dot) {
            self.advance();
            path.push(self.expect_field_name()?);
        }
        self.expect(TokenKind::Colon)?;
        let lit_span = self.peek_span();
        let lit_tok = self.advance().ok_or(CompileError::UnexpectedEof)?;
        let port = match lit_tok.kind {
            TokenKind::StringLit(s) => s,
            other => {
                return Err(CompileError::unexpected_token(
                    "string literal for SV port name (e.g. \"s_axi_awvalid\")",
                    &other.to_string(),
                    lit_span,
                ));
            }
        };
        Ok(BindRemapEntry {
            path,
            port,
            span: start.merge(lit_tok.span),
        })
    }

    /// Parse a single probe declaration:
    ///     probe [force] <name> : <type> at <dotted.path>
    /// The path is parsed as a sequence of identifiers separated by `.`
    /// with optional bracket selectors after each segment, and stored
    /// verbatim as a string — HARC does not validate paths against the
    /// DUT's SV source; Verilator does. The optional `force` modifier
    /// opts into SV-procedural-force support for fault injection
    /// (docs/probe-signals.md §3.1).
    fn parse_probe_decl(&mut self) -> Result<Probe, CompileError> {
        let start = self.expect(TokenKind::Probe)?.span;
        let force = if self.check(TokenKind::Force) {
            self.advance();
            true
        } else {
            false
        };
        let name = self.expect_field_name()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type_expr()?;
        self.expect(TokenKind::At)?;
        // Dotted SV path: segment ('.' segment)*, where each segment may
        // carry array/generate selectors such as `regs[0]`.
        let (mut path, mut end) = self.parse_probe_path_segment()?;
        while self.check(TokenKind::Dot) {
            self.advance();
            let (next, next_end) = self.parse_probe_path_segment()?;
            path.push('.');
            path.push_str(&next);
            end = next_end;
        }
        Ok(Probe {
            name,
            ty,
            path,
            force,
            span: start.merge(end),
        })
    }

    fn parse_probe_path_segment(&mut self) -> Result<(String, Span), CompileError> {
        let first = self.expect_field_name()?;
        let mut path = first.name;
        let mut end = first.span;
        while self.check(TokenKind::LBracket) {
            self.advance();
            path.push('[');
            let mut saw_selector = false;
            while !self.check(TokenKind::RBracket) {
                let tok = self.advance().ok_or(CompileError::UnexpectedEof)?;
                path.push_str(&tok.kind.to_string());
                saw_selector = true;
            }
            if !saw_selector {
                return Err(CompileError::unexpected_token(
                    "probe path selector",
                    "]",
                    self.peek_span(),
                ));
            }
            let close = self.expect(TokenKind::RBracket)?;
            path.push(']');
            end = close.span;
        }
        Ok((path, end))
    }

    fn parse_for_stmt(&mut self) -> Result<ForStmt, CompileError> {
        let start = self.expect(TokenKind::For)?.span;
        let var = if self.check(TokenKind::Underscore) {
            let s = self.advance().unwrap().span;
            Ident {
                name: "_".into(),
                span: s,
            }
        } else {
            self.expect_ident()?
        };
        self.expect(TokenKind::In)?;
        let iter = self.parse_expr()?;
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end_anon(TokenKind::For)?;
        Ok(ForStmt {
            var,
            iter,
            body: Block {
                stmts,
                span: body_start.merge(end),
            },
            span: start.merge(end),
        })
    }

    fn parse_constraint_expr(&mut self) -> Result<Expr, CompileError> {
        if self.check(TokenKind::For) {
            return self.parse_foreach_constraint_expr();
        }
        self.parse_expr()
    }

    fn parse_foreach_constraint_expr(&mut self) -> Result<Expr, CompileError> {
        let start = self.expect(TokenKind::For)?.span;
        let var = if self.check(TokenKind::Underscore) {
            let s = self.advance().unwrap().span;
            Ident {
                name: "_".into(),
                span: s,
            }
        } else {
            self.expect_ident()?
        };
        self.expect(TokenKind::In)?;
        let iter = self.parse_expr()?;
        let mut body = Vec::new();
        while !self.check_end_keyword() {
            body.push(self.parse_constraint_expr()?);
        }
        let end = self.expect_end_anon(TokenKind::For)?;
        Ok(Expr::new(
            ExprKind::ForEachConstraint { var, iter, body },
            start.merge(end),
        ))
    }

    fn parse_repeat_stmt(&mut self) -> Result<RepeatStmt, CompileError> {
        let start = self.expect(TokenKind::Repeat)?.span;
        let count = self.parse_expr()?;
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end_anon(TokenKind::Repeat)?;
        Ok(RepeatStmt {
            count,
            body: Block {
                stmts,
                span: body_start.merge(end),
            },
            span: start.merge(end),
        })
    }

    fn parse_if_stmt(&mut self) -> Result<IfStmt, CompileError> {
        let start = self.expect(TokenKind::If)?.span;
        let cond = self.parse_expr()?;
        let then_start = self.peek_span();
        let mut then_stmts = Vec::new();
        while !matches!(
            self.peek_kind(),
            Some(TokenKind::ElsIf | TokenKind::Else | TokenKind::End)
        ) {
            then_stmts.push(self.parse_stmt()?);
        }
        let then_block = Block {
            stmts: then_stmts,
            span: then_start,
        };
        let mut elsifs = Vec::new();
        while self.check(TokenKind::ElsIf) {
            self.advance();
            let c = self.parse_expr()?;
            let block_start = self.peek_span();
            let mut block_stmts = Vec::new();
            while !matches!(
                self.peek_kind(),
                Some(TokenKind::ElsIf | TokenKind::Else | TokenKind::End)
            ) {
                block_stmts.push(self.parse_stmt()?);
            }
            elsifs.push((
                c,
                Block {
                    stmts: block_stmts,
                    span: block_start,
                },
            ));
        }
        let else_block = if self.check(TokenKind::Else) {
            self.advance();
            // Catch `else if` — a two-token mistake from SV/Verilog
            // muscle memory. HARC uses single-token `elsif`. Without
            // this directed error the parser silently treats it as
            // `else { nested if }`, which then runs out of `end`s
            // and surfaces a misleading "opened with `if`, closed
            // with <enclosing>" error far from the actual typo.
            if self.check(TokenKind::If) {
                return Err(CompileError::general(
                    "`else if` is not HARC syntax — use single-token `elsif` instead. \
                     (Spelling matches the keyword shape used by `elsif <cond>` chains in \
                     `if … elsif … else … end if`.)",
                    self.peek_span(),
                ));
            }
            let block_start = self.peek_span();
            let mut block_stmts = Vec::new();
            while !self.check(TokenKind::End) {
                block_stmts.push(self.parse_stmt()?);
            }
            Some(Block {
                stmts: block_stmts,
                span: block_start,
            })
        } else {
            None
        };
        let end = self.expect_end_anon(TokenKind::If)?;
        Ok(IfStmt {
            cond,
            then_block,
            elsifs,
            else_block,
            span: start.merge(end),
        })
    }

    fn parse_fork_stmt(&mut self) -> Result<ForkStmt, CompileError> {
        let start = self.expect(TokenKind::Fork)?.span;
        let mut branches = Vec::new();
        while self.check(TokenKind::Branch) {
            self.advance();
            let body_start = self.peek_span();
            let stmts = self.parse_stmt_list_until_end()?;
            let end = self.expect_end_anon(TokenKind::Branch)?;
            branches.push(Block {
                stmts,
                span: body_start.merge(end),
            });
        }
        let (join, end) = match self.peek_kind() {
            Some(TokenKind::JoinAll) => (ForkJoin::All, self.advance().unwrap().span),
            Some(TokenKind::JoinAny) => (ForkJoin::Any, self.advance().unwrap().span),
            Some(TokenKind::JoinNone) => (ForkJoin::None, self.advance().unwrap().span),
            _ => {
                return Err(CompileError::unexpected_token(
                    "join_all, join_any, or join_none",
                    &self
                        .peek_kind()
                        .map(|k| k.to_string())
                        .unwrap_or("EOF".into()),
                    self.peek_span(),
                ))
            }
        };
        Ok(ForkStmt {
            branches,
            join,
            span: start.merge(end),
        })
    }

    fn parse_verify(&mut self, allow_property_kw: bool) -> Result<Verify, CompileError> {
        let start = self.peek_span();
        let mut property_kw = false;
        if allow_property_kw && self.check(TokenKind::Property) {
            self.advance();
            property_kw = true;
        }
        // `assert name` — bare identifier reference; `assert <expr>` — full expression.
        // To keep LL(1), parse an expression and let the user's intent flow through.
        let expr = self.parse_expr()?;
        let mut else_fail = None;
        if self.check(TokenKind::Else) {
            self.advance();
            // `else fail("...")` or `else <stmt>` — for v1, accept fail call.
            self.expect(TokenKind::Fail)?;
            self.expect(TokenKind::LParen)?;
            let arg = self.parse_expr()?;
            let close = self.expect(TokenKind::RParen)?.span;
            else_fail = Some(arg);
            return Ok(Verify {
                named: None,
                expr: Some(expr.clone()),
                else_fail,
                property_kw,
                span: start.merge(close),
            });
        }
        let span = start.merge(expr.span);
        // If it's a plain ident, also expose it as `named` for pretty-printer fidelity.
        let named = if let ExprKind::Ident(id) = &*expr.kind {
            Some(id.clone())
        } else {
            None
        };
        Ok(Verify {
            named,
            expr: Some(expr),
            else_fail,
            property_kw,
            span,
        })
    }

    fn parse_paren_expr_one(&mut self) -> Result<Expr, CompileError> {
        self.expect(TokenKind::LParen)?;
        let e = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        Ok(e)
    }

    fn parse_call_arg(&mut self) -> Result<CallArg, CompileError> {
        // Named: `name = expr` (LL(1) — peek IDENT then `=`).
        if matches!(self.peek_kind(), Some(TokenKind::Ident(_)))
            && matches!(self.peek2_kind(), Some(TokenKind::Eq))
        {
            let name = self.expect_ident()?;
            self.expect(TokenKind::Eq)?;
            let value = self.parse_expr()?;
            return Ok(CallArg::Named { name, value });
        }
        Ok(CallArg::Expr(self.parse_expr()?))
    }

    // ── Expressions (Pratt-style) ─────────────────────────────────────────────

    pub fn parse_expr(&mut self) -> Result<Expr, CompileError> {
        self.parse_expr_bp(0)
    }

    fn parse_expr_bp(&mut self, min_bp: u8) -> Result<Expr, CompileError> {
        let mut lhs = self.parse_unary()?;
        loop {
            // Treat `>` specially when inside type-arg list context.
            let op = match self.peek_kind() {
                Some(k) => k.clone(),
                None => break,
            };
            // Termination tokens — never an operator.
            if matches!(
                op,
                TokenKind::Comma
                    | TokenKind::RParen
                    | TokenKind::RBracket
                    | TokenKind::RBrace
                    | TokenKind::Semi
                    | TokenKind::End
                    | TokenKind::Else
                    | TokenKind::ElsIf
                    | TokenKind::Default
                    | TokenKind::With
                    | TokenKind::Branch
                    | TokenKind::JoinAll
                    | TokenKind::JoinAny
                    | TokenKind::JoinNone
                    | TokenKind::ColonSlash
            ) {
                break;
            }
            if self.no_angle && matches!(op, TokenKind::Gt | TokenKind::GtEq | TokenKind::Shr) {
                break;
            }
            // Postfix: `(`, `[`, `.`, `as`, `<-` — handled in parse_postfix already.
            // Here we handle infix operators only.
            let (l_bp, r_bp, op_kind) = match infix_bp(&op) {
                Some(x) => x,
                None => break,
            };
            if l_bp < min_bp {
                break;
            }
            // `..` produces a range literal rather than a normal binary node.
            if op == TokenKind::DotDot {
                self.advance();
                let rhs = self.parse_expr_bp(r_bp)?;
                let span = lhs.span.merge(rhs.span);
                lhs = Expr::new(
                    ExprKind::RangeLit {
                        lo: Some(lhs),
                        hi: Some(rhs),
                    },
                    span,
                );
                continue;
            }
            // Ternary `cond ? then : else`. Right-associative, lower
            // precedence than every other operator except implication.
            // The `then` branch is parsed with bp=0 — `:` doesn't appear
            // as an infix operator in expression context, so it cleanly
            // terminates the inner parse.
            if op == TokenKind::Question {
                self.advance();
                let then_branch = self.parse_expr_bp(0)?;
                self.expect(TokenKind::Colon)?;
                let else_branch = self.parse_expr_bp(r_bp)?;
                let span = lhs.span.merge(else_branch.span);
                lhs = Expr::new(
                    ExprKind::Ternary {
                        cond: lhs,
                        then_branch,
                        else_branch,
                    },
                    span,
                );
                continue;
            }
            self.advance();
            // Special handling for `##N expr` and `##[m:n] expr` — treat ## as a
            // binary connector with N as a "count" rather than a normal RHS.
            if matches!(op_kind, InfixOp::Binary(BinaryOp::AndKw)) && false {
                // unreachable — keep clippy happy
            }
            // `dist` directive after `t.size` — `t.size dist { ... }` — we don't
            // model `dist` as a normal infix; handled via Solve / DistDirective
            // only when seen as a leading keyword. Skip here.
            if op == TokenKind::Inside {
                // `e inside { ... }` membership — accept set literal as RHS.
                let set = self.parse_unary()?;
                let span = lhs.span.merge(set.span);
                lhs = Expr::new(ExprKind::Membership { expr: lhs, set }, span);
                continue;
            }
            if op == TokenKind::In {
                let set = self.parse_expr_bp(r_bp)?;
                let span = lhs.span.merge(set.span);
                lhs = Expr::new(ExprKind::Membership { expr: lhs, set }, span);
                continue;
            }
            // `dist` as operator: `t.size dist { ... }`
            if op == TokenKind::Dist {
                let entries = self.parse_dist_entries()?;
                let last_span = entries.last().map(|e| e.weight.span).unwrap_or(lhs.span);
                let span = lhs.span.merge(last_span);
                lhs = Expr::new(
                    ExprKind::DistDirective {
                        target: lhs,
                        entries,
                    },
                    span,
                );
                continue;
            }
            let rhs = self.parse_expr_bp(r_bp)?;
            let span = lhs.span.merge(rhs.span);
            lhs = match op_kind {
                InfixOp::Binary(b) => Expr::new(ExprKind::Binary { op: b, lhs, rhs }, span),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, CompileError> {
        let span0 = self.peek_span();
        match self.peek_kind() {
            Some(TokenKind::Minus) => {
                self.advance();
                let e = self.parse_unary()?;
                let s = span0.merge(e.span);
                Ok(Expr::new(
                    ExprKind::Unary {
                        op: UnaryOp::Neg,
                        expr: e,
                    },
                    s,
                ))
            }
            Some(TokenKind::Bang) => {
                self.advance();
                let e = self.parse_unary()?;
                let s = span0.merge(e.span);
                Ok(Expr::new(
                    ExprKind::Unary {
                        op: UnaryOp::Not,
                        expr: e,
                    },
                    s,
                ))
            }
            Some(TokenKind::Not) => {
                self.advance();
                let e = self.parse_unary()?;
                let s = span0.merge(e.span);
                Ok(Expr::new(
                    ExprKind::Unary {
                        op: UnaryOp::NotKw,
                        expr: e,
                    },
                    s,
                ))
            }
            Some(TokenKind::Tilde) => {
                self.advance();
                let e = self.parse_unary()?;
                let s = span0.merge(e.span);
                Ok(Expr::new(
                    ExprKind::Unary {
                        op: UnaryOp::BitNot,
                        expr: e,
                    },
                    s,
                ))
            }
            Some(TokenKind::HashHash) => {
                self.advance();
                let count = self.parse_hash_count()?;
                let body = self.parse_unary()?;
                let s = span0.merge(body.span);
                Ok(Expr::new(ExprKind::HashHash { count, expr: body }, s))
            }
            Some(TokenKind::Fork) => {
                self.advance();
                let call = self.parse_postfix()?;
                let s = span0.merge(call.span);
                Ok(Expr::new(ExprKind::ForkCall { call }, s))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_hash_count(&mut self) -> Result<HashCount, CompileError> {
        if self.check(TokenKind::LBracket) {
            self.advance();
            let lo = self.parse_expr()?;
            self.expect(TokenKind::Colon)?;
            let hi = self.parse_expr()?;
            self.expect(TokenKind::RBracket)?;
            Ok(HashCount::Range { lo, hi })
        } else {
            let e = self.parse_primary()?;
            Ok(HashCount::Const(e))
        }
    }

    /// Reserved width-method names that take a generic `<N>` width arg
    /// followed by `()` parens. Ported from arch-com's `is_method_name`
    /// (src/parser.rs:5757). When the parser sees `<recv>.<name><W>()`
    /// for one of these names, it lowers to a Call with the width as
    /// the first arg; codegen dispatches on the method name and emits
    /// the appropriate narrow/extend C++.
    fn is_width_method_static(name: &str) -> bool {
        matches!(name, "trunc" | "zext" | "sext" | "resize")
    }

    fn parse_postfix(&mut self) -> Result<Expr, CompileError> {
        let mut e = self.parse_primary()?;
        loop {
            match self.peek_kind() {
                Some(TokenKind::Dot) => {
                    self.advance();
                    let name = self.expect_field_name()?;
                    let span = e.span.merge(name.span);
                    e = Expr::new(
                        ExprKind::Field {
                            target: e,
                            name: name.clone(),
                        },
                        span,
                    );
                    // Width-method generic call: `.trunc<N>()`, `.zext<N>()`,
                    // `.sext<N>()`, `.resize<N>()`. Mirrors arch-com's
                    // surface (src/parser.rs:3368 over there). The `<...>`
                    // type-arg phase reuses the `no_angle` flag so `>` doesn't
                    // get mis-parsed as a comparison. Emitted as
                    // `Call { callee: Field{...}, args: [width_expr] }` — the
                    // codegen dispatches on the method name pattern and
                    // emits the corresponding C++ narrow/extend.
                    if Self::is_width_method_static(&name.name) && self.check(TokenKind::Lt) {
                        let lt_span = self.advance().unwrap().span;
                        let prev = self.no_angle;
                        self.no_angle = true;
                        let width_expr = self.parse_expr()?;
                        self.no_angle = prev;
                        self.expect_close_angle()?;
                        self.expect(TokenKind::LParen)?;
                        let close = self.expect(TokenKind::RParen)?.span;
                        let span = e.span.merge(close);
                        let _ = lt_span;
                        e = Expr::new(
                            ExprKind::Call {
                                callee: e,
                                args: vec![CallArg::Expr(width_expr)],
                            },
                            span,
                        );
                    }
                }
                Some(TokenKind::LParen) => {
                    // Only treat `(...)` as a function-call postfix when the
                    // LHS is callable (ident, field access, prior call). On
                    // a numeric/string/etc. primary the LParen is the start
                    // of a new expression — important for free-form blocks
                    // like `randomize(p) with` where each constraint is a
                    // separate expression and may begin with `(`.
                    let callable = matches!(
                        &*e.kind,
                        ExprKind::Ident(_) | ExprKind::Field { .. } | ExprKind::Call { .. }
                    );
                    if !callable {
                        break;
                    }
                    self.advance();
                    let mut args = Vec::new();
                    while !self.check(TokenKind::RParen) {
                        args.push(self.parse_call_arg()?);
                        if self.check(TokenKind::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    let close = self.expect(TokenKind::RParen)?.span;
                    let span = e.span.merge(close);
                    e = Expr::new(ExprKind::Call { callee: e, args }, span);
                }
                Some(TokenKind::LBracket) => {
                    self.advance();
                    // Could be `[i]`, `[m:n]`, `[m..n]`. Parse the first expression and check.
                    let first = self.parse_expr()?;
                    if self.check(TokenKind::Colon) {
                        self.advance();
                        let lo = self.parse_expr()?;
                        let close = self.expect(TokenKind::RBracket)?.span;
                        let span = e.span.merge(close);
                        e = Expr::new(
                            ExprKind::BitSlice {
                                target: e,
                                hi: first,
                                lo,
                            },
                            span,
                        );
                    } else if self.check(TokenKind::DotDot) {
                        self.advance();
                        let hi = self.parse_expr()?;
                        let close = self.expect(TokenKind::RBracket)?.span;
                        let range_span = first.span.merge(hi.span);
                        let range = Expr::new(
                            ExprKind::RangeLit {
                                lo: Some(first),
                                hi: Some(hi),
                            },
                            range_span,
                        );
                        let span = e.span.merge(close);
                        e = Expr::new(
                            ExprKind::Index {
                                target: e,
                                index: range,
                            },
                            span,
                        );
                    } else {
                        let close = self.expect(TokenKind::RBracket)?.span;
                        let span = e.span.merge(close);
                        e = Expr::new(
                            ExprKind::Index {
                                target: e,
                                index: first,
                            },
                            span,
                        );
                    }
                }
                Some(TokenKind::As) => {
                    self.advance();
                    let ty = self.parse_type_expr()?;
                    let span = e.span.merge(ty.span());
                    e = Expr::new(ExprKind::Cast { expr: e, ty }, span);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr, CompileError> {
        let span0 = self.peek_span();
        match self.peek_kind().cloned() {
            Some(TokenKind::DecLiteral(s)) => {
                self.advance();
                Ok(Expr::new(ExprKind::Int(s), span0))
            }
            Some(TokenKind::HexLiteral(s))
            | Some(TokenKind::BinLiteral(s))
            | Some(TokenKind::SizedLiteral(s)) => {
                self.advance();
                Ok(Expr::new(ExprKind::Int(s), span0))
            }
            Some(TokenKind::FloatLiteral(s)) => {
                self.advance();
                Ok(Expr::new(ExprKind::Float(s), span0))
            }
            Some(TokenKind::TimeLiteral(s)) => {
                self.advance();
                Ok(Expr::new(ExprKind::Time(s), span0))
            }
            Some(TokenKind::StringLit(s)) => {
                self.advance();
                Ok(Expr::new(ExprKind::String(s), span0))
            }
            Some(TokenKind::True) => {
                self.advance();
                Ok(Expr::new(ExprKind::Bool(true), span0))
            }
            Some(TokenKind::False) => {
                self.advance();
                Ok(Expr::new(ExprKind::Bool(false), span0))
            }
            Some(TokenKind::LParen) => {
                self.advance();
                let e = self.parse_expr()?;
                let close = self.expect(TokenKind::RParen)?.span;
                Ok(Expr::new(ExprKind::Paren(e), span0.merge(close)))
            }
            Some(TokenKind::LBrace) => {
                // Set literal `{a, b, c}`.
                self.advance();
                let mut items = Vec::new();
                while !self.check(TokenKind::RBrace) {
                    items.push(self.parse_expr()?);
                    if self.check(TokenKind::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                let close = self.expect(TokenKind::RBrace)?.span;
                Ok(Expr::new(ExprKind::SetLit(items), span0.merge(close)))
            }
            Some(TokenKind::LBracket) => {
                // Bracket-enclosed range literal: `[a..b]`, `[..b]`, `[a..]`,
                // or `[..]`. The body is parsed via the normal expression path
                // (where `..` is an infix producing RangeLit). Open-low form
                // (`[..b]`) is detected by an immediately-following `..`.
                self.advance();
                if self.check(TokenKind::DotDot) {
                    self.advance();
                    let hi = if self.check(TokenKind::RBracket) {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    let close = self.expect(TokenKind::RBracket)?.span;
                    return Ok(Expr::new(
                        ExprKind::RangeLit { lo: None, hi },
                        span0.merge(close),
                    ));
                }
                let inner = self.parse_expr()?;
                let close = self.expect(TokenKind::RBracket)?.span;
                let span = span0.merge(close);
                // Propagate the inner expression but with the enclosing-bracket span.
                Ok(Expr {
                    kind: inner.kind,
                    span,
                })
            }
            Some(TokenKind::Dot) => {
                // `.field` shorthand for list-comprehension predicates.
                self.advance();
                let name = self.expect_ident()?;
                let span = span0.merge(name.span);
                let target = Expr::new(ExprKind::ImplicitSelf, span0);
                Ok(Expr::new(ExprKind::Field { target, name }, span))
            }
            Some(TokenKind::Underscore) => {
                let s = self.advance().unwrap().span;
                let id = Ident {
                    name: "_".into(),
                    span: s,
                };
                Ok(Expr::new(ExprKind::Ident(id), s))
            }
            // `past` / `rose` / `fell` / `stable` are NOT keywords — they
            // parse as regular identifier-named function calls (matches
            // ARCH). The codegen recognises the names when lowering temporal
            // properties / SVA. Only `$clog2` keeps the `$` form since it's
            // a compile-time function shared with ARCH.
            Some(TokenKind::Clog2) => self.parse_system_call(SystemFn::Clog2),
            Some(TokenKind::SolveOrder) => self.parse_solve_order_directive(),
            Some(TokenKind::Dist) => {
                // Standalone `dist { ... }` — wraps as a directive without target.
                self.advance();
                let entries = self.parse_dist_entries()?;
                let last_span = entries.last().map(|e| e.weight.span).unwrap_or(span0);
                let placeholder = Expr::new(ExprKind::ImplicitSelf, span0);
                Ok(Expr::new(
                    ExprKind::DistDirective {
                        target: placeholder,
                        entries,
                    },
                    span0.merge(last_span),
                ))
            }
            Some(TokenKind::Randomize) => {
                // Expression-form randomize: `randomize(t)` — parsed as a call-like expression.
                self.advance();
                let target = self.parse_paren_expr_one()?;
                let mut with_body = Vec::new();
                if self.check(TokenKind::With) {
                    self.advance();
                    while !self.check_end_keyword() {
                        with_body.push(self.parse_constraint_expr()?);
                    }
                    self.expect_end_anon(TokenKind::Randomize)?;
                }
                let s = span0.merge(target.span);
                Ok(Expr::new(
                    ExprKind::Randomize {
                        blocking: false,
                        target,
                        with_body,
                    },
                    s,
                ))
            }
            Some(TokenKind::Ident(_)) => {
                let id = self.expect_ident()?;
                let span = id.span;
                Ok(Expr::new(ExprKind::Ident(id), span))
            }
            // Soft keywords usable as identifiers in expression position.
            // The construct keywords (env / agent / driver / monitor / scoreboard
            // / sequencer / bus / state / event / stream / buffer / queue / pseq /
            // sequence) are also conventional instance-field names in the spec
            // examples (e.g. `env.agent.monitor.txn`). When seen at the leading
            // edge of a primary expression, they identify a named value.
            Some(tok) if soft_keyword_to_ident(&tok).is_some() => {
                let name = soft_keyword_to_ident(&tok).unwrap();
                let span = self.advance().unwrap().span;
                let id = Ident {
                    name: name.into(),
                    span,
                };
                Ok(Expr::new(ExprKind::Ident(id), span))
            }
            Some(TokenKind::Semi) => Err(CompileError::unsupported_syntax(
                "`;` is not a statement separator",
                "statements are separated by newlines; put each on its own line",
                span0,
            )),
            Some(other) => Err(CompileError::unexpected_token(
                "expression",
                &other.to_string(),
                span0,
            )),
            None => Err(CompileError::UnexpectedEof),
        }
    }

    fn parse_system_call(&mut self, name: SystemFn) -> Result<Expr, CompileError> {
        let start = self.advance().unwrap().span;
        let mut args = Vec::new();
        if self.check(TokenKind::LParen) {
            self.advance();
            while !self.check(TokenKind::RParen) {
                args.push(self.parse_expr()?);
                if self.check(TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            let close = self.expect(TokenKind::RParen)?.span;
            return Ok(Expr::new(
                ExprKind::SystemCall { name, args },
                start.merge(close),
            ));
        }
        Ok(Expr::new(ExprKind::SystemCall { name, args }, start))
    }

    fn parse_solve_order_directive(&mut self) -> Result<Expr, CompileError> {
        let start = self.advance().unwrap().span;
        self.expect(TokenKind::LParen)?;
        let mut args = Vec::new();
        while !self.check(TokenKind::RParen) {
            args.push(self.parse_expr()?);
            if self.check(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let close = self.expect(TokenKind::RParen)?.span;
        Ok(Expr::new(ExprKind::SolveOrder { args }, start.merge(close)))
    }
}

// ── Helpers outside impl ──────────────────────────────────────────────────────

/// Extract the `---` … `---` YAML frontmatter sub-block from an
/// inner-doc string (the line-joined post-prefix-stripped text of a
/// leading `//!` block).
///
/// Returns the text *between* the fences with newlines preserved (no
/// trailing `\n`), or `None` if the inner-doc doesn't open with `---`.
/// The fence detection is line-exact: the first content line must be
/// exactly `---`, and the closing fence must be a line that's exactly
/// `---`. Anything before the opening fence (e.g. free-form prose)
/// disqualifies the inner-doc from having a frontmatter — matches
/// arch-com's lexical rule from `plan_arch_doc_comments.md` §2.3.
///
/// Empty body (`---` immediately followed by `---`) returns
/// `Some(String::new())` — distinct from "no frontmatter" via the
/// `None` return.
fn extract_frontmatter(inner_doc: &str) -> Option<String> {
    let mut lines = inner_doc.split('\n');
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut body_lines = Vec::new();
    for line in lines {
        if line.trim_end() == "---" {
            return Some(body_lines.join("\n"));
        }
        body_lines.push(line);
    }
    // No closing fence — treat as malformed; return None rather than
    // assuming the rest of the doc is frontmatter.
    None
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named { span, .. } | TypeExpr::Builtin { span, .. } => *span,
        }
    }
}

fn arg_span(a: &TypeArg) -> Span {
    match a {
        TypeArg::Type(t) => t.span(),
        TypeArg::Expr(e) => e.span,
        TypeArg::Named { name, value } => name.span.merge(value.span),
    }
}

#[derive(Debug, Clone, Copy)]
enum InfixOp {
    Binary(BinaryOp),
}

fn soft_keyword_to_ident(t: &TokenKind) -> Option<&'static str> {
    Some(match t {
        TokenKind::Env => "env",
        TokenKind::Agent => "agent",
        TokenKind::Sequencer => "sequencer",
        TokenKind::Scoreboard => "scoreboard",
        TokenKind::Bus => "bus",
        TokenKind::Bind => "bind",
        TokenKind::State => "state",
        TokenKind::Event => "event",
        TokenKind::Stream => "stream",
        TokenKind::Buffer => "buffer",
        TokenKind::Queue => "queue",
        TokenKind::Sequence => "sequence",
        TokenKind::Run => "run",
        TokenKind::Setup => "setup",
        TokenKind::Check => "check",
        TokenKind::Teardown => "teardown",
        TokenKind::Phase => "phase",
        TokenKind::Weight => "weight",
        TokenKind::Connect => "connect",
        TokenKind::Branch => "branch",
        TokenKind::Cross => "cross",
        TokenKind::Bins => "bins",
        TokenKind::Clocking => "clocking",
        TokenKind::Contract => "contract",
        TokenKind::Guarantee => "guarantee",
        TokenKind::Bound => "bound",
        TokenKind::To => "to",
        TokenKind::Default => "default",
        TokenKind::Fail => "fail",
        TokenKind::Stop => "stop",
        TokenKind::Fatal => "fatal",
        _ => return None,
    })
}

fn infix_bp(t: &TokenKind) -> Option<(u8, u8, InfixOp)> {
    use BinaryOp::*;
    use InfixOp::Binary as B;
    Some(match t {
        // Implication (lowest), right-associative.
        TokenKind::PipeImplies => (5, 4, B(PipeImplies)),
        TokenKind::PipeImpliesNext => (5, 4, B(PipeImpliesNext)),
        // Ternary `?:` — right-associative, just above implication. The
        // op_kind here is a placeholder; parse_expr_bp special-cases
        // `?` and never looks at the BinaryOp tag.
        TokenKind::Question => (7, 6, B(BitAnd)),
        // Logical or
        TokenKind::PipePipe | TokenKind::Or => (
            10,
            11,
            B(if matches!(t, TokenKind::Or) {
                OrKw
            } else {
                OrOr
            }),
        ),
        // Logical and
        TokenKind::AmpAmp | TokenKind::And => (
            12,
            13,
            B(if matches!(t, TokenKind::And) {
                AndKw
            } else {
                AndAnd
            }),
        ),
        // Bitwise OR / XOR / AND
        TokenKind::Pipe => (14, 15, B(BitOr)),
        TokenKind::Caret => (16, 17, B(BitXor)),
        TokenKind::Amp => (18, 19, B(BitAnd)),
        // Equality
        TokenKind::EqEq => (20, 21, B(Eq)),
        TokenKind::BangEq => (20, 21, B(Ne)),
        // Comparison + membership
        TokenKind::Lt => (22, 23, B(Lt)),
        TokenKind::LtEq => (22, 23, B(Le)),
        TokenKind::Gt => (22, 23, B(Gt)),
        TokenKind::GtEq => (22, 23, B(Ge)),
        TokenKind::In => (22, 23, B(In)),
        TokenKind::Inside => (22, 23, B(Inside)),
        // Temporal mid
        TokenKind::Throughout => (24, 25, B(Throughout)),
        TokenKind::Within => (24, 25, B(Within)),
        TokenKind::Intersect => (24, 25, B(Intersect)),
        // Dist directive — special-cased in parse_expr_bp; we still need to
        // tell the loop "yes, consume it" — pretend it has a precedence.
        TokenKind::Dist => (24, 25, B(BitAnd)), // op_kind unused (see special case)
        // Shifts
        TokenKind::Shl => (26, 27, B(Shl)),
        TokenKind::Shr => (26, 27, B(Shr)),
        // Additive
        TokenKind::Plus => (28, 29, B(Add)),
        TokenKind::Minus => (28, 29, B(Sub)),
        // Multiplicative
        TokenKind::Star => (30, 31, B(Mul)),
        TokenKind::Slash => (30, 31, B(Div)),
        TokenKind::Percent => (30, 31, B(Mod)),
        // Range literal: `0 .. n` — handled specially in parse_expr_bp.
        TokenKind::DotDot => (8, 9, B(Add)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Result<SourceFile, CompileError> {
        parse_source(src)
    }

    #[test]
    fn empty_file() {
        let f = parse("").unwrap();
        assert!(f.items.is_empty());
    }

    #[test]
    fn use_decl() {
        let f = parse("use arc.stdlib.BusAxi4").unwrap();
        assert_eq!(f.items.len(), 1);
        if let Item::Use(u) = &f.items[0] {
            assert_eq!(u.path.segments.len(), 3);
            assert_eq!(u.path.segments[0].name, "arc");
            assert_eq!(u.path.segments[2].name, "BusAxi4");
        } else {
            panic!("expected Use, got {:?}", f.items[0]);
        }
    }

    #[test]
    fn enum_decl() {
        let f = parse("enum BurstType { FIXED, INCR, WRAP }").unwrap();
        if let Item::Enum(e) = &f.items[0] {
            assert_eq!(e.variants.len(), 3);
            assert_eq!(e.variants[0].name, "FIXED");
        } else {
            panic!();
        }
    }

    #[test]
    fn transaction_simple() {
        let src = r#"transaction T
    addr : uint<64>
    !mode : Bool default true
    keep addr % 64 == 0
end transaction T"#;
        let f = parse(src).unwrap();
        if let Item::Transaction(t) = &f.items[0] {
            assert_eq!(t.body.len(), 3);
            if let TxnBodyItem::Field(f0) = &t.body[0] {
                assert!(!f0.non_random);
                assert_eq!(f0.name.name, "addr");
            }
            if let TxnBodyItem::Field(f1) = &t.body[1] {
                assert!(f1.non_random);
                assert_eq!(f1.name.name, "mode");
            }
            assert!(matches!(&t.body[2], TxnBodyItem::Keep(_)));
        } else {
            panic!();
        }
    }

    #[test]
    fn when_subtype() {
        let src = r#"transaction X
    op : Op
    when op == WRITE
        data : bits<32>
    end when
end transaction X"#;
        let f = parse(src).unwrap();
        if let Item::Transaction(t) = &f.items[0] {
            assert_eq!(t.body.len(), 2);
            assert!(matches!(&t.body[1], TxnBodyItem::When(_)));
        }
    }

    #[test]
    fn extend_decl() {
        let src = r#"package P
    extend AxiTxn
        keep len < 16
    end extend AxiTxn
end package P"#;
        parse(src).unwrap();
    }

    #[test]
    fn relation_alias_form() {
        let src = "relation R(t: AxiWrite) = t.addr % 64 == 0";
        let f = parse(src).unwrap();
        if let Item::Relation(r) = &f.items[0] {
            assert!(matches!(r.body, RelationBody::Alias(_)));
        }
    }

    #[test]
    fn tseq_basic() {
        let src = r#"tseq RandomTxns(n: int) -> TSeq<AxiTxn>
    for _ in 0 .. n
        let t : AxiTxn
        randomize(t)
        yield t
    end for
end tseq RandomTxns"#;
        parse(src).unwrap();
    }

    #[test]
    fn covergroup_simple() {
        let src = r#"covergroup G @(posedge clk)
    cp_op : cover dut.op
    cp_len : cover dut.len
        bins
            single = {1}
            short = [2..8]
        end bins
    cross cp_op, cp_len
end covergroup G"#;
        parse(src).unwrap();
    }

    #[test]
    fn covergroup_hook_trigger() {
        let src = r#"covergroup TxnCov @(mon.observed(t) post)
    cp_op : cover t.op
end covergroup TxnCov"#;
        let f = parse(src).unwrap();
        if let Item::Covergroup(g) = &f.items[0] {
            assert!(matches!(
                &g.trigger,
                Some(CoverTrigger::Hook {
                    side: HookSide::Post,
                    ..
                })
            ));
        } else {
            panic!("expected covergroup");
        }
    }

    #[test]
    fn assert_with_else_fail() {
        let src = r#"scoreboard SB
    on env.x(t)
        assert t == t else fail("oops")
    end on
end scoreboard SB"#;
        parse(src).unwrap();
    }

    #[test]
    fn dist_directive() {
        let src = r#"tseq Foo -> TSeq<int>
    let t : int
    randomize(t) with
        t dist { [0..2] :/ 100 }
    end randomize
end tseq Foo"#;
        parse(src).unwrap();
    }
}
