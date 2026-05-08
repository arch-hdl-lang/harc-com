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
        Self { tokens, pos: 0, no_angle: false, source: source.to_string() }
    }

    /// True if a newline appears in the source between `prev_end` (the end
    /// of a known-just-consumed token) and the next non-doc token's start.
    /// Used at sites where `(` could be a parameter list opener or the
    /// start of a body expression.
    fn newline_before_peek(&self, prev_end: usize) -> bool {
        let next = self.peek_span().start;
        if next <= prev_end || next > self.source.len() { return false; }
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
            Some(k) => Err(CompileError::unexpected_token(&kind.to_string(), &k.to_string(), span)),
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
            Some(other) => Err(CompileError::unexpected_token("identifier", &other.to_string(), span)),
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
        if lines.is_empty() { None } else { Some(lines.join("\n")) }
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
        if lines.is_empty() { None } else { Some(lines.join("\n")) }
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
        let mut items = Vec::new();
        while !self.at_end() {
            items.push(self.parse_item()?);
        }
        Ok(SourceFile { items, inner_doc })
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
            Some(TokenKind::Driver) => self.parse_component(ComponentKind::Driver, doc).map(Item::Driver),
            Some(TokenKind::Monitor) => self.parse_component(ComponentKind::Monitor, doc).map(Item::Monitor),
            Some(TokenKind::Env) => self.parse_component(ComponentKind::Env, doc).map(Item::Env),
            Some(TokenKind::Scoreboard) => self.parse_component(ComponentKind::Scoreboard, doc).map(Item::Scoreboard),
            Some(TokenKind::Sequencer) => self.parse_component(ComponentKind::Sequencer, doc).map(Item::Sequencer),
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
            Some(TokenKind::Apply) => self.parse_apply().map(Item::Apply),
            Some(other) => Err(CompileError::unexpected_token(
                "use, package, const, struct, enum, transaction, relation, tseq, agent, driver, monitor, env, scoreboard, sequencer, test, extend, covergroup, property, pseq, cover sequence, module, function, or apply",
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
        Ok(Path { segments, span: start.merge(end) })
    }

    fn parse_package(&mut self, doc: Option<String>) -> Result<PackageDecl, CompileError> {
        let start = self.expect(TokenKind::Package)?.span;
        let name = self.expect_ident()?;
        let mut items = Vec::new();
        while !self.check_end_keyword() {
            items.push(self.parse_item()?);
        }
        let end = self.expect_end(TokenKind::Package, &name.name)?;
        Ok(PackageDecl { name, items, span: start.merge(end), doc })
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
        Ok(DomainDecl { name, fields, span: start.merge(end), doc })
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
        Ok(ConstDecl { name, ty, value, span, doc })
    }

    // ── Struct / Enum ─────────────────────────────────────────────────────────

    fn parse_struct(&mut self, doc: Option<String>) -> Result<StructDecl, CompileError> {
        let start = self.expect(TokenKind::Struct)?.span;
        let name = self.expect_ident()?;
        let mut fields = Vec::new();
        while !self.check_end_keyword() {
            let f_doc = self.consume_outer_doc();
            fields.push(self.parse_field(f_doc)?);
        }
        let end = self.expect_end(TokenKind::Struct, &name.name)?;
        Ok(StructDecl { name, fields, span: start.merge(end), doc })
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
        Ok(EnumDecl { name, variants, span: start.merge(end), doc })
    }

    // ── Transaction (§3.1, §3.3) ──────────────────────────────────────────────

    fn parse_transaction(&mut self, doc: Option<String>) -> Result<TransactionDecl, CompileError> {
        let start = self.expect(TokenKind::Transaction)?.span;
        let name = self.expect_ident()?;
        let params = self.parse_optional_generic_params()?;
        let body = self.parse_txn_body_until_end()?;
        let end = self.expect_end(TokenKind::Transaction, &name.name)?;
        Ok(TransactionDecl { name, params, body, span: start.merge(end), doc })
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
        let end_span = attrs.last().map(|a| a.span).or(default.as_ref().map(|e| e.span)).unwrap_or(ty.span());
        Ok(Field { name, non_random, ty, default, attrs, span: start.merge(end_span), doc })
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
        Ok(Attr { name, args, span: start.merge(end) })
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
            other => return Err(CompileError::unexpected_token("scope name", &other.to_string(), span)),
        };
        Ok(Ident { name, span })
    }

    /// Accept any plain identifier, plus soft keywords usable as member names.
    fn expect_field_name(&mut self) -> Result<Ident, CompileError> {
        let span = self.peek_span();
        if let Some(name) = self.peek_kind().and_then(soft_keyword_to_ident) {
            self.advance();
            return Ok(Ident { name: name.into(), span });
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
                    return Err(CompileError::unexpected_token(
                        "bin name",
                        &s,
                        span,
                    ));
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
                return Err(CompileError::unexpected_token("identifier", &other.to_string(), span));
            }
        };
        Ok(Ident { name, span })
    }

    fn parse_keep(&mut self) -> Result<Keep, CompileError> {
        let start = self.expect(TokenKind::Keep)?.span;
        let expr = self.parse_expr()?;
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
        Ok(WhenSubtype { discriminant, items, span: start.merge(end) })
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
        Ok(RelationDecl { name, params, body, span: start.merge(end_span), doc })
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
        // Body is a normal block, terminated by `end tseq Name`.
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end(TokenKind::Tseq, &name.name)?;
        let body = Block { stmts, span: body_start.merge(end) };
        Ok(TseqDecl { name, params, return_ty, body, span: start.merge(end), doc })
    }

    // ── Component declarations ────────────────────────────────────────────────

    fn parse_component(&mut self, kind: ComponentKind, doc: Option<String>) -> Result<ComponentDecl, CompileError> {
        let start_kw = match kind {
            ComponentKind::Agent => TokenKind::Agent,
            ComponentKind::Driver => TokenKind::Driver,
            ComponentKind::Monitor => TokenKind::Monitor,
            ComponentKind::Env => TokenKind::Env,
            ComponentKind::Scoreboard => TokenKind::Scoreboard,
            ComponentKind::Sequencer => TokenKind::Sequencer,
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
        let mut items = Vec::new();
        while !self.check_end_keyword() {
            items.push(self.parse_component_item()?);
        }
        let end = self.expect_end(start_kw, &name.name)?;
        Ok(ComponentDecl { kind, name, params, bound_to, items, span: start.merge(end), doc })
    }

    fn parse_component_item(&mut self) -> Result<ComponentItem, CompileError> {
        let doc = self.consume_outer_doc();
        match self.peek_kind() {
            Some(TokenKind::Connect) => Ok(ComponentItem::Connect(self.parse_connect_block()?)),
            Some(TokenKind::On) => Ok(ComponentItem::OnHandler(self.parse_on_handler()?)),
            Some(TokenKind::Hookable) => Ok(ComponentItem::Hookable(self.parse_hookable()?)),
            Some(TokenKind::Apply) => Ok(ComponentItem::Apply(self.parse_apply()?)),
            _ => Ok(ComponentItem::Field(self.parse_component_field(doc)?)),
        }
    }

    fn parse_component_field(&mut self, doc: Option<String>) -> Result<ComponentField, CompileError> {
        let start = self.peek_span();
        let name = self.expect_field_name()?;
        self.expect(TokenKind::Colon)?;
        // Direction: `in` / `out` / `inout` (kw `in` / ident `out`/`inout`).
        let direction = match self.peek_kind() {
            Some(TokenKind::In) => { self.advance(); Some(Direction::In) }
            Some(TokenKind::Ident(s)) if s == "out" => { self.advance(); Some(Direction::Out) }
            Some(TokenKind::Ident(s)) if s == "inout" => { self.advance(); Some(Direction::InOut) }
            _ => None,
        };
        let ty = self.parse_type_expr()?;
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
        let end = default.as_ref().map(|e| e.span)
            .or(bound_to.as_ref().map(|t| t.span()))
            .unwrap_or(ty.span());
        Ok(ComponentField { name, direction, ty, bound_to, default, span: start.merge(end), doc })
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
        Ok(ConnectBlock { edges, span: start.merge(end) })
    }

    fn parse_on_handler(&mut self) -> Result<OnHandler, CompileError> {
        let start = self.expect(TokenKind::On)?.span;
        let event = self.parse_expr()?;
        let hook = match self.peek_kind() {
            Some(TokenKind::Pre) => { self.advance(); Some(HookSide::Pre) }
            Some(TokenKind::Post) => { self.advance(); Some(HookSide::Post) }
            _ => None,
        };
        // Optional edge-mode keyword for cycle-trigger form: `rising` /
        // `falling` / `level`. Ident-tokens, not reserved keywords (so a
        // user can still name a variable `level` outside trigger context).
        let edge = if self.check_ident("rising") {
            self.advance(); EdgeMode::Rising
        } else if self.check_ident("falling") {
            self.advance(); EdgeMode::Falling
        } else if self.check_ident("level") {
            self.advance(); EdgeMode::Level
        } else {
            EdgeMode::Rising
        };
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end_anon(TokenKind::On)?;
        let body = Block { stmts, span: body_start.merge(end) };
        Ok(OnHandler { event, hook, edge, body, span: start.merge(end) })
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
            body: Block { stmts, span: body_start.merge(end) },
            span: start.merge(end),
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
        let mut items = Vec::new();
        while !self.check_end_keyword() {
            items.push(self.parse_test_item()?);
        }
        let end = self.expect_end(TokenKind::Test, &name.name)?;
        Ok(TestDecl { name, params, items, span: start.merge(end), doc })
    }

    fn parse_test_item(&mut self) -> Result<TestItem, CompileError> {
        match self.peek_kind() {
            Some(TokenKind::Apply) => Ok(TestItem::Apply(self.parse_apply()?)),
            Some(TokenKind::Let) => Ok(TestItem::Let(self.parse_let_stmt()?)),
            Some(TokenKind::Scope) => Ok(TestItem::Scope(self.parse_scope()?)),
            Some(TokenKind::Use) => Ok(TestItem::Use(self.parse_use(None)?)),
            Some(TokenKind::ClockGen) => Ok(TestItem::Clock(self.parse_clock_decl()?)),
            // Anything else: a bare statement, treated as implicit `run`.
            Some(_) => Ok(TestItem::Stmt(self.parse_stmt()?)),
            None => Err(CompileError::UnexpectedEof),
        }
    }

    fn parse_clock_decl(&mut self) -> Result<ClockDecl, CompileError> {
        let start = self.expect(TokenKind::ClockGen)?.span;
        let name = self.expect_field_name()?;
        self.expect(TokenKind::Eq)?;
        let period = self.parse_expr()?;
        let span = start.merge(period.span);
        Ok(ClockDecl { name, period, span, doc: None })
    }

    fn parse_apply(&mut self) -> Result<ApplyDecl, CompileError> {
        let start = self.expect(TokenKind::Apply)?.span;
        let path = self.parse_dotted_path()?;
        let span = start.merge(path.span);
        Ok(ApplyDecl { path, span })
    }

    fn parse_scope(&mut self) -> Result<ScopeDecl, CompileError> {
        let start = self.expect(TokenKind::Scope)?.span;
        let name = self.expect_ident()?;
        let mut setup = None;
        let mut run = None;
        let mut check = None;
        let mut teardown = None;
        while !self.check_end_keyword() {
            match self.peek_kind() {
                Some(TokenKind::Setup) => {
                    self.advance();
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end = self.expect_end_anon(TokenKind::Setup)?;
                    setup = Some(Block { stmts, span: end });
                }
                Some(TokenKind::Run) => {
                    self.advance();
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end = self.expect_end_anon(TokenKind::Run)?;
                    run = Some(Block { stmts, span: end });
                }
                Some(TokenKind::Check) => {
                    self.advance();
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end = self.expect_end_anon(TokenKind::Check)?;
                    check = Some(Block { stmts, span: end });
                }
                Some(TokenKind::Teardown) => {
                    self.advance();
                    let stmts = self.parse_stmt_list_until_end()?;
                    let end = self.expect_end_anon(TokenKind::Teardown)?;
                    teardown = Some(Block { stmts, span: end });
                }
                Some(other) => {
                    return Err(CompileError::unexpected_token(
                        "setup, run, check, or teardown",
                        &other.to_string(),
                        self.peek_span(),
                    ));
                }
                None => return Err(CompileError::UnexpectedEof),
            }
        }
        let end = self.expect_end(TokenKind::Scope, &name.name)?;
        Ok(ScopeDecl { name, setup, run, check, teardown, span: start.merge(end) })
    }

    // ── Extend ────────────────────────────────────────────────────────────────

    fn parse_extend(&mut self, doc: Option<String>) -> Result<ExtendDecl, CompileError> {
        let start = self.expect(TokenKind::Extend)?.span;
        let target = self.parse_dotted_path()?;
        // Pick the body grammar from the first body token. Test-style extends
        // start with `scope`/`apply`/`use`; component-style start with
        // `connect`/`on`/`hookable`; everything else is txn/struct-style. All
        // three are unambiguous at one-token lookahead.
        let body = match self.peek_kind() {
            // Test-style extend: items are scope decls / applies / uses /
            // statements (incl. `assert`/`assume`/`cover`/`log`/`wait`/etc.)
            Some(TokenKind::Scope) | Some(TokenKind::Apply) | Some(TokenKind::Use)
            | Some(TokenKind::Let)
            | Some(TokenKind::Assert) | Some(TokenKind::Assume) | Some(TokenKind::Cover)
            | Some(TokenKind::Log) | Some(TokenKind::LogF) | Some(TokenKind::Wait)
            | Some(TokenKind::For) | Some(TokenKind::Repeat) | Some(TokenKind::Loop)
            | Some(TokenKind::While) | Some(TokenKind::Break) | Some(TokenKind::Continue)
            | Some(TokenKind::If) | Some(TokenKind::Fork) | Some(TokenKind::Randomize)
            | Some(TokenKind::On) | Some(TokenKind::Emit) => {
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
                    items.push(self.parse_component_item()?);
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
            return Err(CompileError::mismatched_kind("extend", &kw_tok.kind.to_string(), kw_tok.span));
        }
        let _close_path = self.parse_dotted_path()?;
        let span = start.merge(end_tok.span);
        Ok(ExtendDecl { target, body, span, doc })
    }

    // ── Covergroup ────────────────────────────────────────────────────────────

    fn parse_covergroup(&mut self, doc: Option<String>) -> Result<CovergroupDecl, CompileError> {
        let start = self.expect(TokenKind::Covergroup)?.span;
        let name = self.expect_ident()?;
        let clocking = if self.check(TokenKind::At) {
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
                    ExprKind::Call { callee, args: vec![CallArg::Expr(arg)] },
                    span,
                )
            } else {
                self.parse_expr()?
            };
            self.expect(TokenKind::RParen)?;
            Some(e)
        } else {
            None
        };
        let mut items = Vec::new();
        while !self.check_end_keyword() {
            items.push(self.parse_cover_item()?);
        }
        let end = self.expect_end(TokenKind::Covergroup, &name.name)?;
        Ok(CovergroupDecl { name, clocking, items, span: start.merge(end), doc })
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
            Ok(CoverItem::Cross(CoverCross { points, span: start.merge(end) }))
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
                    bins.push(CoverBin { name: bn, spec, span });
                }
                self.expect_end_anon(TokenKind::Bins)?;
            }
            let end = bins.last().map(|b| b.span).unwrap_or(target.span);
            Ok(CoverItem::Point(CoverPoint { name, target, bins, span: start.merge(end) }))
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
        let body = self.parse_expr()?;
        let end = self.expect_end(TokenKind::Property, &name.name)?;
        Ok(PropertyDecl { name, params, body, span: start.merge(end), doc })
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
        let body = self.parse_expr()?;
        let end = self.expect_end(TokenKind::Pseq, &name.name)?;
        Ok(PseqDecl { name, params, body, span: start.merge(end), doc })
    }

    fn parse_cover_sequence(&mut self, doc: Option<String>) -> Result<CoverSequenceDecl, CompileError> {
        let start = self.expect(TokenKind::Cover)?.span;
        self.expect(TokenKind::Sequence)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::Eq)?;
        let pattern = self.parse_expr()?;
        let span = start.merge(pattern.span);
        Ok(CoverSequenceDecl { name, pattern, span, doc })
    }

    // ── External (Verilator-bound) module ─────────────────────────────────────

    fn parse_external_module(&mut self, doc: Option<String>) -> Result<ExternalModuleDecl, CompileError> {
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
            fields.push(ExternalField { name: fname, value, span });
        }
        let end = self.expect_end(TokenKind::Module, &name.name)?;
        Ok(ExternalModuleDecl { name, kind, fields, span: start.merge(end), doc })
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
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end(TokenKind::Function, &name.name)?;
        Ok(FunctionDecl { name, params, return_ty, body: Block { stmts, span: body_start.merge(end) }, span: start.merge(end), doc })
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
        let name = self.expect_ident()?;
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
        let end = default.as_ref().map(|e| e.span)
            .or(ty.as_ref().map(|t| t.span()))
            .unwrap_or(start);
        Ok(Param { name, ty, default, span: start.merge(end) })
    }

    // ── Type expressions ──────────────────────────────────────────────────────

    pub fn parse_type_expr(&mut self) -> Result<TypeExpr, CompileError> {
        let span0 = self.peek_span();
        match self.peek_kind() {
            Some(TokenKind::UIntKw) => self.parse_builtin_ty(BuiltinTy::UInt, TokenKind::UIntKw, true),
            Some(TokenKind::SIntKw) => self.parse_builtin_ty(BuiltinTy::SInt, TokenKind::SIntKw, true),
            Some(TokenKind::BitsKw) => self.parse_builtin_ty(BuiltinTy::Bits, TokenKind::BitsKw, true),
            Some(TokenKind::UInt) => self.parse_builtin_ty(BuiltinTy::UIntCap, TokenKind::UInt, true),
            Some(TokenKind::SInt) => self.parse_builtin_ty(BuiltinTy::SIntCap, TokenKind::SInt, true),
            Some(TokenKind::Bool) => self.consume_atomic_ty(BuiltinTy::Bool, TokenKind::Bool),
            Some(TokenKind::BoolLower) => self.consume_atomic_ty(BuiltinTy::BoolLower, TokenKind::BoolLower),
            Some(TokenKind::Bit) => self.consume_atomic_ty(BuiltinTy::Bit, TokenKind::Bit),
            Some(TokenKind::Int) => self.consume_atomic_ty(BuiltinTy::Int, TokenKind::Int),
            Some(TokenKind::Time) => self.consume_atomic_ty(BuiltinTy::Time, TokenKind::Time),
            Some(TokenKind::Prop) => self.consume_atomic_ty(BuiltinTy::Prop, TokenKind::Prop),
            Some(TokenKind::Pseq) => self.consume_atomic_ty(BuiltinTy::Pseq, TokenKind::Pseq),
            Some(TokenKind::SeverityTy) => self.consume_atomic_ty(BuiltinTy::Severity, TokenKind::SeverityTy),
            Some(TokenKind::LoggerTy) => self.consume_atomic_ty(BuiltinTy::Logger, TokenKind::LoggerTy),
            Some(TokenKind::StringTy) => self.consume_atomic_ty(BuiltinTy::String, TokenKind::StringTy),
            Some(TokenKind::Clock) => self.consume_atomic_ty(BuiltinTy::Clock, TokenKind::Clock),
            Some(TokenKind::Reset) => self.consume_atomic_ty(BuiltinTy::Reset, TokenKind::Reset),
            Some(TokenKind::KwVec) => self.parse_builtin_ty(BuiltinTy::Vec, TokenKind::KwVec, true),
            Some(TokenKind::TSeqTy) => self.parse_builtin_ty(BuiltinTy::TSeq, TokenKind::TSeqTy, true),
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
                Ok(TypeExpr::Builtin { name: kind, args, span: span0.merge(end) })
            }
            Some(TokenKind::Buffer) => self.parse_builtin_ty(BuiltinTy::Buffer, TokenKind::Buffer, true),
            Some(TokenKind::Stream) => self.parse_builtin_ty(BuiltinTy::Stream, TokenKind::Stream, true),
            Some(TokenKind::State) => self.parse_builtin_ty(BuiltinTy::State, TokenKind::State, true),
            Some(TokenKind::Queue) => self.parse_builtin_ty(BuiltinTy::Queue, TokenKind::Queue, true),
            Some(TokenKind::Ident(_)) => {
                let path = self.parse_dotted_path()?;
                let mut generics = Vec::new();
                let mut span = path.span;
                if self.check(TokenKind::Hash) && matches!(self.peek2_kind(), Some(TokenKind::LParen)) {
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
                Ok(TypeExpr::Named { name: path, generics, span })
            }
            Some(other) => Err(CompileError::unexpected_token("type", &other.to_string(), span0)),
            None => Err(CompileError::UnexpectedEof),
        }
    }

    fn consume_atomic_ty(&mut self, name: BuiltinTy, tok: TokenKind) -> Result<TypeExpr, CompileError> {
        let span = self.expect(tok)?.span;
        Ok(TypeExpr::Builtin { name, args: Vec::new(), span })
    }

    fn parse_builtin_ty(&mut self, name: BuiltinTy, tok: TokenKind, _angle: bool) -> Result<TypeExpr, CompileError> {
        let start = self.expect(tok)?.span;
        let args = if self.check(TokenKind::Lt) {
            self.parse_type_arg_list()?
        } else {
            Vec::new()
        };
        let end = args.last().map(arg_span).unwrap_or(start);
        Ok(TypeExpr::Builtin { name, args, span: start.merge(end) })
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
                ">", &other.to_string(), self.peek_span(),
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
            Some(TokenKind::UIntKw | TokenKind::SIntKw | TokenKind::BitsKw
                | TokenKind::UInt | TokenKind::SInt | TokenKind::Bool | TokenKind::Bit | TokenKind::Int
                | TokenKind::BoolLower | TokenKind::Time | TokenKind::Prop | TokenKind::Pseq
                | TokenKind::SeverityTy | TokenKind::LoggerTy | TokenKind::StringTy | TokenKind::Clock
                | TokenKind::Reset | TokenKind::KwVec | TokenKind::Event | TokenKind::Buffer
                | TokenKind::Stream | TokenKind::State | TokenKind::Queue | TokenKind::TSeqTy)
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
            Some(TokenKind::JoinAll) | Some(TokenKind::JoinAny) | Some(TokenKind::JoinNone) => true,
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
                Ok(Stmt { kind: StmtKind::Let(l.clone()), span: l.span })
            }
            Some(TokenKind::For) => {
                let s = self.parse_for_stmt()?;
                let span = s.span;
                Ok(Stmt { kind: StmtKind::For(s), span })
            }
            Some(TokenKind::Repeat) => {
                let s = self.parse_repeat_stmt()?;
                let span = s.span;
                Ok(Stmt { kind: StmtKind::Repeat(s), span })
            }
            Some(TokenKind::Loop) => {
                self.advance();
                let body_start = self.peek_span();
                let stmts = self.parse_stmt_list_until_end()?;
                let end = self.expect_end_anon(TokenKind::Loop)?;
                Ok(Stmt {
                    kind: StmtKind::Loop(Block { stmts, span: body_start.merge(end) }),
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
                        body: Block { stmts, span: body_start.merge(end) },
                        span,
                    },
                    span,
                })
            }
            Some(TokenKind::Break) => {
                let s = self.expect(TokenKind::Break)?.span;
                Ok(Stmt { kind: StmtKind::Break { span: s }, span: s })
            }
            Some(TokenKind::Continue) => {
                let s = self.expect(TokenKind::Continue)?.span;
                Ok(Stmt { kind: StmtKind::Continue { span: s }, span: s })
            }
            Some(TokenKind::If) => {
                let s = self.parse_if_stmt()?;
                let span = s.span;
                Ok(Stmt { kind: StmtKind::If(s), span })
            }
            Some(TokenKind::Fork) => {
                let s = self.parse_fork_stmt()?;
                let span = s.span;
                Ok(Stmt { kind: StmtKind::Fork(s), span })
            }
            Some(TokenKind::Parallel) => {
                self.advance();
                let mut branches = Vec::new();
                while !self.check_end_keyword() {
                    branches.push(self.parse_inline_block_until_terminator()?);
                }
                let end = self.expect_end_anon(TokenKind::Parallel)?;
                Ok(Stmt { kind: StmtKind::Parallel(branches), span: start.merge(end) })
            }
            Some(TokenKind::Schedule) => {
                self.advance();
                let mut branches = Vec::new();
                while !self.check_end_keyword() {
                    branches.push(self.parse_inline_block_until_terminator()?);
                }
                let end = self.expect_end_anon(TokenKind::Schedule)?;
                Ok(Stmt { kind: StmtKind::Schedule(branches), span: start.merge(end) })
            }
            Some(TokenKind::Select) => {
                self.advance();
                let mut arms = Vec::new();
                while !self.check_end_keyword() {
                    let event = self.parse_expr()?;
                    self.expect(TokenKind::FatArrow)?;
                    let action_stmt = self.parse_stmt()?;
                    let span = event.span.merge(action_stmt.span);
                    let action = Block { stmts: vec![action_stmt], span };
                    arms.push(SelectArm { event, action, span });
                }
                let end = self.expect_end_anon(TokenKind::Select)?;
                Ok(Stmt { kind: StmtKind::Select(arms), span: start.merge(end) })
            }
            Some(TokenKind::On) => {
                let h = self.parse_on_handler()?;
                let span = h.span;
                Ok(Stmt { kind: StmtKind::On(h), span })
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
                Ok(Stmt { kind: StmtKind::Emit { name: path, args, span }, span })
            }
            Some(TokenKind::Yield) => {
                self.advance();
                let e = self.parse_expr()?;
                let span = start.merge(e.span);
                Ok(Stmt { kind: StmtKind::Yield(e), span })
            }
            Some(TokenKind::Return) => {
                self.advance();
                if self.is_block_terminator() {
                    Ok(Stmt { kind: StmtKind::Return(None), span: start })
                } else {
                    let e = self.parse_expr()?;
                    let span = start.merge(e.span);
                    Ok(Stmt { kind: StmtKind::Return(Some(e)), span })
                }
            }
            Some(TokenKind::Apply) => {
                let a = self.parse_apply()?;
                let span = a.span;
                Ok(Stmt { kind: StmtKind::Apply(a), span })
            }
            Some(TokenKind::Assert) => {
                self.advance();
                // `property` keyword is allowed in all three roles per spec
                // §5 (`assert property`, `assume property`, `cover property`).
                let v = self.parse_verify(true)?;
                let span = start.merge(v.span);
                Ok(Stmt { kind: StmtKind::Assert(v), span })
            }
            Some(TokenKind::Assume) => {
                self.advance();
                let v = self.parse_verify(true)?;
                let span = start.merge(v.span);
                Ok(Stmt { kind: StmtKind::Assume(v), span })
            }
            Some(TokenKind::Cover) => {
                self.advance();
                let v = self.parse_verify(true)?;
                let span = start.merge(v.span);
                Ok(Stmt { kind: StmtKind::Cover(v), span })
            }
            Some(TokenKind::Randomize) => {
                self.advance();
                let target_e = self.parse_paren_expr_one()?;
                let mut with_body = Vec::new();
                if self.check(TokenKind::With) {
                    self.advance();
                    while !self.check_end_keyword() {
                        with_body.push(self.parse_expr()?);
                    }
                    self.expect_end_anon(TokenKind::Randomize)?;
                }
                let span = start.merge(target_e.span);
                Ok(Stmt { kind: StmtKind::Randomize { blocking: false, target: target_e, with_body }, span })
            }
            Some(TokenKind::Blocking) => {
                self.advance();
                self.expect(TokenKind::Randomize)?;
                let target_e = self.parse_paren_expr_one()?;
                let mut with_body = Vec::new();
                if self.check(TokenKind::With) {
                    self.advance();
                    while !self.check_end_keyword() {
                        with_body.push(self.parse_expr()?);
                    }
                    self.expect_end_anon(TokenKind::Randomize)?;
                }
                let span = start.merge(target_e.span);
                Ok(Stmt { kind: StmtKind::Randomize { blocking: true, target: target_e, with_body }, span })
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
                        body: Block { stmts, span: body_start.merge(end) },
                        span: start.merge(end),
                    },
                    span: start.merge(end),
                })
            }
            Some(TokenKind::Wait) => {
                self.advance();
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
                Ok(Stmt { kind: StmtKind::Wait { duration: dur, clock, span }, span })
            }
            _ => {
                // Expression-or-assignment statement.
                let lhs = self.parse_expr()?;
                if self.check(TokenKind::Eq) {
                    self.advance();
                    let rhs = self.parse_expr()?;
                    let span = lhs.span.merge(rhs.span);
                    Ok(Stmt { kind: StmtKind::Assign { target: lhs, value: rhs }, span })
                } else if self.check(TokenKind::LArrow) {
                    self.advance();
                    let rhs = self.parse_expr()?;
                    let span = lhs.span.merge(rhs.span);
                    Ok(Stmt { kind: StmtKind::Send { target: lhs, value: rhs }, span })
                } else {
                    let span = lhs.span;
                    Ok(Stmt { kind: StmtKind::Expr(lhs), span })
                }
            }
        }
    }

    fn parse_inline_block_until_terminator(&mut self) -> Result<Block, CompileError> {
        // For `parallel`/`schedule` whose branches are sub-statements without `branch` markers.
        // Each "branch" is a single statement at this level, lifted to a Block.
        let s = self.parse_stmt()?;
        let span = s.span;
        Ok(Block { stmts: vec![s], span })
    }

    fn parse_let_stmt(&mut self) -> Result<LetStmt, CompileError> {
        let start = self.expect(TokenKind::Let)?.span;
        let name = self.expect_field_name()?;
        let ty = if self.check(TokenKind::Colon) {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        let mut value = None;
        let mut bind = false;
        if self.check(TokenKind::Eq) {
            self.advance();
            if self.check(TokenKind::Bind) {
                self.advance();
                bind = true;
                // Bind value can be a free expression: `bind dut.s_axi`.
                value = Some(self.parse_expr()?);
            } else {
                value = Some(self.parse_expr()?);
            }
        }
        let end = value.as_ref().map(|e| e.span).or(ty.as_ref().map(|t| t.span())).unwrap_or(name.span);
        Ok(LetStmt { name, ty, value, bind, span: start.merge(end) })
    }

    fn parse_for_stmt(&mut self) -> Result<ForStmt, CompileError> {
        let start = self.expect(TokenKind::For)?.span;
        let var = if self.check(TokenKind::Underscore) {
            let s = self.advance().unwrap().span;
            Ident { name: "_".into(), span: s }
        } else {
            self.expect_ident()?
        };
        self.expect(TokenKind::In)?;
        let iter = self.parse_expr()?;
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end_anon(TokenKind::For)?;
        Ok(ForStmt { var, iter, body: Block { stmts, span: body_start.merge(end) }, span: start.merge(end) })
    }

    fn parse_repeat_stmt(&mut self) -> Result<RepeatStmt, CompileError> {
        let start = self.expect(TokenKind::Repeat)?.span;
        let count = self.parse_expr()?;
        let body_start = self.peek_span();
        let stmts = self.parse_stmt_list_until_end()?;
        let end = self.expect_end_anon(TokenKind::Repeat)?;
        Ok(RepeatStmt { count, body: Block { stmts, span: body_start.merge(end) }, span: start.merge(end) })
    }

    fn parse_if_stmt(&mut self) -> Result<IfStmt, CompileError> {
        let start = self.expect(TokenKind::If)?.span;
        let cond = self.parse_expr()?;
        let then_start = self.peek_span();
        let mut then_stmts = Vec::new();
        while !matches!(self.peek_kind(), Some(TokenKind::ElsIf | TokenKind::Else | TokenKind::End)) {
            then_stmts.push(self.parse_stmt()?);
        }
        let then_block = Block { stmts: then_stmts, span: then_start };
        let mut elsifs = Vec::new();
        while self.check(TokenKind::ElsIf) {
            self.advance();
            let c = self.parse_expr()?;
            let block_start = self.peek_span();
            let mut block_stmts = Vec::new();
            while !matches!(self.peek_kind(), Some(TokenKind::ElsIf | TokenKind::Else | TokenKind::End)) {
                block_stmts.push(self.parse_stmt()?);
            }
            elsifs.push((c, Block { stmts: block_stmts, span: block_start }));
        }
        let else_block = if self.check(TokenKind::Else) {
            self.advance();
            let block_start = self.peek_span();
            let mut block_stmts = Vec::new();
            while !self.check(TokenKind::End) {
                block_stmts.push(self.parse_stmt()?);
            }
            Some(Block { stmts: block_stmts, span: block_start })
        } else {
            None
        };
        let end = self.expect_end_anon(TokenKind::If)?;
        Ok(IfStmt { cond, then_block, elsifs, else_block, span: start.merge(end) })
    }

    fn parse_fork_stmt(&mut self) -> Result<ForkStmt, CompileError> {
        let start = self.expect(TokenKind::Fork)?.span;
        let mut branches = Vec::new();
        while self.check(TokenKind::Branch) {
            self.advance();
            let body_start = self.peek_span();
            let stmts = self.parse_stmt_list_until_end()?;
            let end = self.expect_end_anon(TokenKind::Branch)?;
            branches.push(Block { stmts, span: body_start.merge(end) });
        }
        let (join, end) = match self.peek_kind() {
            Some(TokenKind::JoinAll) => (ForkJoin::All, self.advance().unwrap().span),
            Some(TokenKind::JoinAny) => (ForkJoin::Any, self.advance().unwrap().span),
            Some(TokenKind::JoinNone) => (ForkJoin::None, self.advance().unwrap().span),
            _ => return Err(CompileError::unexpected_token("join_all, join_any, or join_none", &self.peek_kind().map(|k| k.to_string()).unwrap_or("EOF".into()), self.peek_span())),
        };
        Ok(ForkStmt { branches, join, span: start.merge(end) })
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
            if matches!(op,
                TokenKind::Comma | TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace
                | TokenKind::Semi | TokenKind::End | TokenKind::Else | TokenKind::ElsIf
                | TokenKind::Default | TokenKind::With | TokenKind::Branch
                | TokenKind::JoinAll | TokenKind::JoinAny | TokenKind::JoinNone
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
                    ExprKind::RangeLit { lo: Some(lhs), hi: Some(rhs) },
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
            if matches!(op_kind, InfixOp::Binary(BinaryOp::AndKw))
                && false
            {
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
                lhs = Expr::new(ExprKind::DistDirective { target: lhs, entries }, span);
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
                Ok(Expr::new(ExprKind::Unary { op: UnaryOp::Neg, expr: e }, s))
            }
            Some(TokenKind::Bang) => {
                self.advance();
                let e = self.parse_unary()?;
                let s = span0.merge(e.span);
                Ok(Expr::new(ExprKind::Unary { op: UnaryOp::Not, expr: e }, s))
            }
            Some(TokenKind::Not) => {
                self.advance();
                let e = self.parse_unary()?;
                let s = span0.merge(e.span);
                Ok(Expr::new(ExprKind::Unary { op: UnaryOp::NotKw, expr: e }, s))
            }
            Some(TokenKind::Tilde) => {
                self.advance();
                let e = self.parse_unary()?;
                let s = span0.merge(e.span);
                Ok(Expr::new(ExprKind::Unary { op: UnaryOp::BitNot, expr: e }, s))
            }
            Some(TokenKind::HashHash) => {
                self.advance();
                let count = self.parse_hash_count()?;
                let body = self.parse_unary()?;
                let s = span0.merge(body.span);
                Ok(Expr::new(ExprKind::HashHash { count, expr: body }, s))
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

    fn parse_postfix(&mut self) -> Result<Expr, CompileError> {
        let mut e = self.parse_primary()?;
        loop {
            match self.peek_kind() {
                Some(TokenKind::Dot) => {
                    self.advance();
                    let name = self.expect_field_name()?;
                    let span = e.span.merge(name.span);
                    e = Expr::new(ExprKind::Field { target: e, name }, span);
                }
                Some(TokenKind::LParen) => {
                    // Only treat `(...)` as a function-call postfix when the
                    // LHS is callable (ident, field access, prior call). On
                    // a numeric/string/etc. primary the LParen is the start
                    // of a new expression — important for free-form blocks
                    // like `randomize(p) with` where each constraint is a
                    // separate expression and may begin with `(`.
                    let callable = matches!(&*e.kind,
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
                        e = Expr::new(ExprKind::BitSlice { target: e, hi: first, lo }, span);
                    } else if self.check(TokenKind::DotDot) {
                        self.advance();
                        let hi = self.parse_expr()?;
                        let close = self.expect(TokenKind::RBracket)?.span;
                        let range_span = first.span.merge(hi.span);
                        let range = Expr::new(ExprKind::RangeLit { lo: Some(first), hi: Some(hi) }, range_span);
                        let span = e.span.merge(close);
                        e = Expr::new(ExprKind::Index { target: e, index: range }, span);
                    } else {
                        let close = self.expect(TokenKind::RBracket)?.span;
                        let span = e.span.merge(close);
                        e = Expr::new(ExprKind::Index { target: e, index: first }, span);
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
            Some(TokenKind::HexLiteral(s)) | Some(TokenKind::BinLiteral(s))
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
                    let hi = if self.check(TokenKind::RBracket) { None } else { Some(self.parse_expr()?) };
                    let close = self.expect(TokenKind::RBracket)?.span;
                    return Ok(Expr::new(ExprKind::RangeLit { lo: None, hi }, span0.merge(close)));
                }
                let inner = self.parse_expr()?;
                let close = self.expect(TokenKind::RBracket)?.span;
                let span = span0.merge(close);
                // Propagate the inner expression but with the enclosing-bracket span.
                Ok(Expr { kind: inner.kind, span })
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
                let id = Ident { name: "_".into(), span: s };
                Ok(Expr::new(ExprKind::Ident(id), s))
            }
            // `past` / `rose` / `fell` / `stable` are NOT keywords — they
            // parse as regular identifier-named function calls (matches
            // ARCH). The codegen recognises the names when lowering temporal
            // properties / SVA. Only `$clog2` keeps the `$` form since it's
            // a compile-time function shared with ARCH.
            Some(TokenKind::Clog2) => self.parse_system_call(SystemFn::Clog2),
            Some(TokenKind::SolveBefore) => self.parse_solve_directive(SolveKind::Before),
            Some(TokenKind::SolveAfter) => self.parse_solve_directive(SolveKind::After),
            Some(TokenKind::Dist) => {
                // Standalone `dist { ... }` — wraps as a directive without target.
                self.advance();
                let entries = self.parse_dist_entries()?;
                let last_span = entries.last().map(|e| e.weight.span).unwrap_or(span0);
                let placeholder = Expr::new(ExprKind::ImplicitSelf, span0);
                Ok(Expr::new(
                    ExprKind::DistDirective { target: placeholder, entries },
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
                        with_body.push(self.parse_expr()?);
                    }
                    self.expect_end_anon(TokenKind::Randomize)?;
                }
                let s = span0.merge(target.span);
                Ok(Expr::new(ExprKind::Randomize { blocking: false, target, with_body }, s))
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
                let id = Ident { name: name.into(), span };
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
            return Ok(Expr::new(ExprKind::SystemCall { name, args }, start.merge(close)));
        }
        Ok(Expr::new(ExprKind::SystemCall { name, args }, start))
    }

    fn parse_solve_directive(&mut self, kind: SolveKind) -> Result<Expr, CompileError> {
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
        Ok(Expr::new(ExprKind::Solve { kind, args }, start.merge(close)))
    }
}

// ── Helpers outside impl ──────────────────────────────────────────────────────

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
        TokenKind::Driver => "driver",
        TokenKind::Monitor => "monitor",
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
        TokenKind::PipePipe | TokenKind::Or => (10, 11, B(if matches!(t, TokenKind::Or) { OrKw } else { OrOr })),
        // Logical and
        TokenKind::AmpAmp | TokenKind::And => (12, 13, B(if matches!(t, TokenKind::And) { AndKw } else { AndAnd })),
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
