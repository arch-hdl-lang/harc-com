use logos::Logos;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// Byte range into a source file.
///
/// `u32` rather than `usize`: a `Span` is embedded in nearly every AST node —
/// every `Ident`, every `Expr`, every `Stmt` — so its width multiplies across
/// the whole tree. Halving it takes `Expr` from 24 bytes to 16 and `Ident`
/// from 40 to 32. The cap is 4 GiB per source file, which is several orders
/// of magnitude above anything the parser can handle in memory anyway;
/// `Span::new` saturates rather than wrapping so an over-long file yields a
/// clamped span instead of a nonsensical one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self {
            start: start.min(u32::MAX as usize) as u32,
            end: end.min(u32::MAX as usize) as u32,
        }
    }

    /// Byte offset of the first character, as a slice index.
    pub fn start_usize(self) -> usize {
        self.start as usize
    }

    /// Byte offset one past the last character, as a slice index.
    pub fn end_usize(self) -> usize {
        self.end as usize
    }

    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

fn parse_outer_doc(lex: &mut logos::Lexer<TokenKind>) -> logos::Filter<String> {
    let s = lex.slice();
    debug_assert!(s.starts_with("///"));
    let rest = &s[3..];
    if rest.starts_with('/') {
        return logos::Filter::Skip;
    }
    let body = rest.strip_prefix(' ').unwrap_or(rest);
    logos::Filter::Emit(body.to_string())
}

fn parse_inner_doc(lex: &mut logos::Lexer<TokenKind>) -> String {
    let s = lex.slice();
    debug_assert!(s.starts_with("//!"));
    let rest = &s[3..];
    rest.strip_prefix(' ').unwrap_or(rest).to_string()
}

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(skip r"//[^\n]*")]
#[logos(skip r"/\*([^*]|\*[^/])*\*/")]
pub enum TokenKind {
    // ── Doc comments ──────────────────────────────────────────────────────────
    #[regex(r"///[^\n]*", parse_outer_doc, priority = 5)]
    DocOuter(String),
    #[regex(r"//![^\n]*", parse_inner_doc, priority = 5)]
    DocInner(String),

    // ── ARCH-shared keywords (§2) ─────────────────────────────────────────────
    #[token("module")]
    Module,
    #[token("end")]
    End,
    #[token("param")]
    Param,
    #[token("port")]
    Port,
    #[token("let")]
    Let,
    #[token("reg")]
    Reg,
    #[token("comb")]
    Comb,
    #[token("seq")]
    Seq,
    #[token("use")]
    Use,
    #[token("function")]
    Function,
    #[token("extern")]
    Extern,
    #[token("package")]
    Package,
    #[token("domain")]
    Domain,
    #[token("Clock")]
    Clock,
    #[token("clock")]
    ClockGen,
    #[token("Reset")]
    Reset,
    #[token("thread")]
    Thread,
    #[token("wait")]
    Wait,
    #[token("lock")]
    Lock,
    #[token("testbench")]
    Testbench,
    #[token("task")]
    Task,
    #[token("init")]
    Init,
    #[token("repeat")]
    Repeat,
    #[token("log")]
    Log,
    #[token("logf")]
    LogF,
    #[token("bus")]
    Bus,
    #[token("struct")]
    Struct,
    #[token("enum")]
    Enum,
    #[token("const")]
    Const,
    #[token("type")]
    Type,
    #[token("kind")]
    Kind,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token("elsif")]
    ElsIf,
    #[token("for")]
    For,
    #[token("in")]
    In,
    #[token("loop")]
    Loop,
    #[token("while")]
    While,
    #[token("break")]
    Break,
    #[token("continue")]
    Continue,
    #[token("return")]
    Return,
    #[token("yield")]
    Yield,
    #[token("true")]
    True,
    #[token("false")]
    False,
    #[token("not")]
    Not,
    #[token("and")]
    And,
    #[token("or")]
    Or,
    #[token("as")]
    As,

    // ── HARC-only verification keywords (§2) ──────────────────────────────────
    #[token("assert")]
    Assert,
    #[token("assume")]
    Assume,
    #[token("cover")]
    Cover,
    #[token("property")]
    Property,
    #[token("pseq")]
    Pseq,
    #[token("solve_order")]
    SolveOrder,
    #[token("dist")]
    Dist,
    #[token("transaction")]
    Transaction,
    #[token("agent")]
    Agent,
    #[token("env")]
    Env,
    /// Transactor — synthesizable BFM declaration (spec §8.1).
    /// Subsumes the legacy `driver`/`monitor` constructs (removed in
    /// PR-B): the always-present body holds the observation half plus
    /// shared protocol state, the optional `when active` block holds
    /// the stimulus half. Mode subtyping at instantiation
    /// (`let xact : T active = bind axil`) selects which body is
    /// synthesized.
    #[token("transactor")]
    Transactor,
    /// Mode tokens for transactor instantiation (`let xact : T active
    /// = bind axil`) and for the in-body `when active` block. Both
    /// `active` and `passive` reserve as hard keywords — neither is
    /// used as an identifier in any existing fixture, and the
    /// alternative (soft-ident matching) would mean two extra
    /// `check_ident` sites per parser path. v0 fixtures don't break.
    #[token("active")]
    Active,
    #[token("passive")]
    Passive,
    /// `probe` declares a hierarchical DUT-internal signal observation
    /// on a `let dut : T` declaration. `at` introduces the dotted path
    /// inside the DUT. See docs/probe-signals.md.
    #[token("probe")]
    Probe,
    #[token("at")]
    At,
    /// `probe force NAME : T at PATH` — fault-injection variant that
    /// lets the test write to an internal DUT signal via SV
    /// `force` / `release`. See docs/probe-signals.md.
    #[token("force")]
    Force,
    /// `release dut.<probe>` — statement that disables the active
    /// force on a `probe force` signal. Pairs with `probe force`.
    #[token("release")]
    Release,
    /// Register Abstraction Layer (docs/ral-support.md). `regblock`
    /// declares a typed register block; `register` declares one
    /// register inside it; `via` names the helper transactor whose
    /// `write(addr, data)` / `read(addr) -> data` methods route bus
    /// traffic.
    #[token("regblock")]
    Regblock,
    #[token("register")]
    Register,
    #[token("via")]
    Via,
    /// `field <name> : <ty> @ <bit_pos> [reset <v>] [access <policy>]`
    /// — bit-slice inside a `register` block. See docs/ral-support.md.
    #[token("field")]
    Field,
    /// `addrmap` is the chip-level container holding multiple
    /// regblock instances at different base addresses. `instance NAME
    /// : RegblockType @ BASE` decls populate the body. See
    /// docs/ral-support.md §4.
    #[token("addrmap")]
    Addrmap,
    #[token("instance")]
    Instance,
    #[token("sequencer")]
    Sequencer,
    #[token("tseq")]
    Tseq,
    #[token("scoreboard")]
    Scoreboard,
    #[token("ref")]
    Ref,
    #[token("phase")]
    Phase,
    #[token("weight")]
    Weight,
    #[token("on")]
    On,
    #[token("after")]
    After,
    #[token("fork")]
    Fork,
    #[token("join_any")]
    JoinAny,
    #[token("join_all")]
    JoinAll,
    #[token("join_none")]
    JoinNone,
    #[token("emit")]
    Emit,
    #[token("scope")]
    Scope,
    #[token("impl")]
    Impl,
    #[token("setup")]
    Setup,
    #[token("run")]
    Run,
    #[token("check")]
    Check,
    #[token("teardown")]
    Teardown,
    #[token("test")]
    Test,
    #[token("blocking")]
    Blocking,
    #[token("across")]
    Across,
    #[token("with")]
    With,
    #[token("default")]
    Default,
    #[token("keep")]
    Keep,
    #[token("extend")]
    Extend,
    #[token("when")]
    When,
    #[token("hookable")]
    Hookable,
    #[token("watchdog")]
    Watchdog,
    #[token("pre")]
    Pre,
    #[token("post")]
    Post,
    #[token("apply")]
    Apply,
    #[token("relation")]
    Relation,
    #[token("randomize")]
    Randomize,
    #[token("covergroup")]
    Covergroup,
    #[token("cross")]
    Cross,
    #[token("bins")]
    Bins,
    #[token("clocking")]
    Clocking,
    #[token("contract")]
    Contract,
    #[token("guarantee")]
    Guarantee,
    #[token("connect")]
    Connect,
    #[token("bound")]
    Bound,
    #[token("to")]
    To,
    #[token("bind")]
    Bind,
    #[token("branch")]
    Branch,
    #[token("parallel")]
    Parallel,
    #[token("schedule")]
    Schedule,
    #[token("select")]
    Select,
    #[token("sequence")]
    Sequence, // only meaningful after `cover` (§2)
    #[token("buffer")]
    Buffer,
    #[token("stream")]
    Stream,
    #[token("state")]
    State,
    #[token("event")]
    Event,
    #[token("queue")]
    Queue,
    #[token("throughout")]
    Throughout,
    #[token("within")]
    Within,
    #[token("intersect")]
    Intersect,
    #[token("inside")]
    Inside,
    #[token("unique")]
    Unique,
    #[token("fail")]
    Fail,
    #[token("stop")]
    Stop,
    #[token("fatal")]
    Fatal,
    // `dut` and `bus` per §2 — `bus` is shared with ARCH; `dut` is conventional, not reserved

    // ── Type keywords ─────────────────────────────────────────────────────────
    #[token("uint")]
    UIntKw,
    #[token("sint")]
    SIntKw,
    #[token("bits")]
    BitsKw,
    #[token("UInt")]
    UInt, // capitalized form for ARCH compatibility
    #[token("SInt")]
    SInt,
    #[token("Bool")]
    Bool,
    #[token("Bit")]
    Bit,
    #[token("Vec")]
    KwVec,
    #[token("int")]
    Int,
    #[token("bool")]
    BoolLower,
    #[token("time")]
    Time,
    #[token("prop")]
    Prop,
    #[token("Severity")]
    SeverityTy,
    #[token("Logger")]
    LoggerTy,
    #[token("String")]
    StringTy,
    #[token("TSeq")]
    TSeqTy,

    // ── SVA / temporal operators ──────────────────────────────────────────────
    #[token("|->")]
    PipeImplies,
    #[token("|=>")]
    PipeImpliesNext,
    #[token("##")]
    HashHash,
    // Compile-time function — kept with `$` since it's an ARCH-shared
    // compile-time function (see arch §). Other temporal helpers
    // (`past` / `rose` / `fell` / `stable`) are written without `$` to
    // match ARCH; they're regular identifier-named calls and the codegen
    // treats them specially when lowering to SVA.
    #[token("$clog2")]
    Clog2,

    // ── Operators and punctuation ─────────────────────────────────────────────
    #[token("+%")]
    PlusPercent,
    #[token("+")]
    Plus,
    #[token("-%")]
    MinusPercent,
    #[token("-")]
    Minus,
    #[token("*%")]
    StarPercent,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("==")]
    EqEq,
    #[token("!=")]
    BangEq,
    #[token("<=")]
    LtEq,
    #[token(">=")]
    GtEq,
    #[token("<-")]
    LArrow,
    #[token("->")]
    RArrow,
    #[token("=>")]
    FatArrow,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("=")]
    Eq,
    #[token("&&")]
    AmpAmp,
    #[token("||")]
    PipePipe,
    #[token("&")]
    Amp,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,
    #[token("~")]
    Tilde,
    #[token("<<")]
    Shl,
    #[token(">>")]
    Shr,
    #[token("::")]
    ColonColon,
    #[token("..")]
    DotDot,
    #[token(".")]
    Dot,
    #[token(":")]
    Colon,
    // `:/` distribution weight separator (e.g. `[0..0xFF] :/ 80`)
    #[token(":/")]
    ColonSlash,
    #[token(";")]
    Semi,
    #[token(",")]
    Comma,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("!")]
    Bang,
    #[token("?")]
    Question,
    #[token("#")]
    Hash,
    #[token("@")]
    AtSign,
    #[token("_")]
    Underscore,

    // ── Literals ──────────────────────────────────────────────────────────────
    // Time literals: `100ns`, `5ps`, `3us`, `4cycles`, `2ms`, `1s` (§3.2)
    #[regex(r"[0-9][0-9_]*(ns|ps|us|ms|cycles|s)", priority = 4, callback = |lex| lex.slice().to_string())]
    TimeLiteral(String),

    #[regex(r"0x[0-9a-fA-F][0-9a-fA-F_]*", |lex| lex.slice().to_string())]
    HexLiteral(String),

    #[regex(r"0b[01][01_]*", |lex| lex.slice().to_string())]
    BinLiteral(String),

    #[regex(r"[0-9]+'[bhd][0-9a-fA-F_]+", |lex| lex.slice().to_string())]
    SizedLiteral(String),

    // Floating-point (used in coverage thresholds like `> 95.0`)
    #[regex(r"[0-9]+\.[0-9]+", priority = 3, callback = |lex| lex.slice().to_string())]
    FloatLiteral(String),

    #[regex(r"[0-9][0-9_]*", priority = 2, callback = |lex| lex.slice().to_string())]
    DecLiteral(String),

    // String literal "..." with escape support, including `${expr}` interpolation
    #[regex(r#""([^"\\]|\\.)*""#, |lex| { let s = lex.slice(); s[1..s.len()-1].to_string() })]
    StringLit(String),

    // Identifier
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", priority = 1, callback = |lex| lex.slice().to_string())]
    Ident(String),
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use TokenKind::*;
        match self {
            DocOuter(_) => write!(f, "///"),
            DocInner(_) => write!(f, "//!"),
            Module => write!(f, "module"),
            End => write!(f, "end"),
            Param => write!(f, "param"),
            Port => write!(f, "port"),
            Let => write!(f, "let"),
            Reg => write!(f, "reg"),
            Comb => write!(f, "comb"),
            Seq => write!(f, "seq"),
            Use => write!(f, "use"),
            Function => write!(f, "function"),
            Extern => write!(f, "extern"),
            Package => write!(f, "package"),
            Domain => write!(f, "domain"),
            Clock => write!(f, "Clock"),
            ClockGen => write!(f, "clock"),
            Reset => write!(f, "Reset"),
            Thread => write!(f, "thread"),
            Wait => write!(f, "wait"),
            Lock => write!(f, "lock"),
            Testbench => write!(f, "testbench"),
            Task => write!(f, "task"),
            Init => write!(f, "init"),
            Repeat => write!(f, "repeat"),
            Log => write!(f, "log"),
            LogF => write!(f, "logf"),
            Bus => write!(f, "bus"),
            Struct => write!(f, "struct"),
            Enum => write!(f, "enum"),
            Const => write!(f, "const"),
            Type => write!(f, "type"),
            Kind => write!(f, "kind"),
            If => write!(f, "if"),
            Else => write!(f, "else"),
            ElsIf => write!(f, "elsif"),
            For => write!(f, "for"),
            In => write!(f, "in"),
            Loop => write!(f, "loop"),
            While => write!(f, "while"),
            Break => write!(f, "break"),
            Continue => write!(f, "continue"),
            Return => write!(f, "return"),
            Yield => write!(f, "yield"),
            True => write!(f, "true"),
            False => write!(f, "false"),
            Not => write!(f, "not"),
            And => write!(f, "and"),
            Or => write!(f, "or"),
            As => write!(f, "as"),
            Assert => write!(f, "assert"),
            Assume => write!(f, "assume"),
            Cover => write!(f, "cover"),
            Property => write!(f, "property"),
            Pseq => write!(f, "pseq"),
            SolveOrder => write!(f, "solve_order"),
            Dist => write!(f, "dist"),
            Transaction => write!(f, "transaction"),
            Agent => write!(f, "agent"),
            Env => write!(f, "env"),
            Transactor => write!(f, "transactor"),
            Active => write!(f, "active"),
            Passive => write!(f, "passive"),
            Probe => write!(f, "probe"),
            At => write!(f, "at"),
            Force => write!(f, "force"),
            Release => write!(f, "release"),
            Regblock => write!(f, "regblock"),
            Register => write!(f, "register"),
            Via => write!(f, "via"),
            Field => write!(f, "field"),
            Addrmap => write!(f, "addrmap"),
            Instance => write!(f, "instance"),
            Sequencer => write!(f, "sequencer"),
            Tseq => write!(f, "tseq"),
            Scoreboard => write!(f, "scoreboard"),
            Ref => write!(f, "ref"),
            Phase => write!(f, "phase"),
            Weight => write!(f, "weight"),
            On => write!(f, "on"),
            After => write!(f, "after"),
            Fork => write!(f, "fork"),
            JoinAny => write!(f, "join_any"),
            JoinAll => write!(f, "join_all"),
            JoinNone => write!(f, "join_none"),
            Emit => write!(f, "emit"),
            Scope => write!(f, "scope"),
            Impl => write!(f, "impl"),
            Setup => write!(f, "setup"),
            Run => write!(f, "run"),
            Check => write!(f, "check"),
            Teardown => write!(f, "teardown"),
            Test => write!(f, "test"),
            Blocking => write!(f, "blocking"),
            Across => write!(f, "across"),
            With => write!(f, "with"),
            Default => write!(f, "default"),
            Keep => write!(f, "keep"),
            Extend => write!(f, "extend"),
            When => write!(f, "when"),
            Hookable => write!(f, "hookable"),
            Watchdog => write!(f, "watchdog"),
            Pre => write!(f, "pre"),
            Post => write!(f, "post"),
            Apply => write!(f, "apply"),
            Relation => write!(f, "relation"),
            Randomize => write!(f, "randomize"),
            Covergroup => write!(f, "covergroup"),
            Cross => write!(f, "cross"),
            Bins => write!(f, "bins"),
            Clocking => write!(f, "clocking"),
            Contract => write!(f, "contract"),
            Guarantee => write!(f, "guarantee"),
            Connect => write!(f, "connect"),
            Bound => write!(f, "bound"),
            To => write!(f, "to"),
            Bind => write!(f, "bind"),
            Branch => write!(f, "branch"),
            Parallel => write!(f, "parallel"),
            Schedule => write!(f, "schedule"),
            Select => write!(f, "select"),
            Sequence => write!(f, "sequence"),
            Buffer => write!(f, "buffer"),
            Stream => write!(f, "stream"),
            State => write!(f, "state"),
            Event => write!(f, "event"),
            Queue => write!(f, "queue"),
            Throughout => write!(f, "throughout"),
            Within => write!(f, "within"),
            Intersect => write!(f, "intersect"),
            Inside => write!(f, "inside"),
            Unique => write!(f, "unique"),
            Fail => write!(f, "fail"),
            Stop => write!(f, "stop"),
            Fatal => write!(f, "fatal"),
            UIntKw => write!(f, "uint"),
            SIntKw => write!(f, "sint"),
            BitsKw => write!(f, "bits"),
            UInt => write!(f, "UInt"),
            SInt => write!(f, "SInt"),
            Bool => write!(f, "Bool"),
            Bit => write!(f, "Bit"),
            KwVec => write!(f, "Vec"),
            Int => write!(f, "int"),
            BoolLower => write!(f, "bool"),
            Time => write!(f, "time"),
            Prop => write!(f, "prop"),
            SeverityTy => write!(f, "Severity"),
            LoggerTy => write!(f, "Logger"),
            StringTy => write!(f, "String"),
            TSeqTy => write!(f, "TSeq"),
            PipeImplies => write!(f, "|->"),
            PipeImpliesNext => write!(f, "|=>"),
            HashHash => write!(f, "##"),
            Clog2 => write!(f, "$clog2"),
            PlusPercent => write!(f, "+%"),
            Plus => write!(f, "+"),
            MinusPercent => write!(f, "-%"),
            Minus => write!(f, "-"),
            StarPercent => write!(f, "*%"),
            Star => write!(f, "*"),
            Slash => write!(f, "/"),
            Percent => write!(f, "%"),
            EqEq => write!(f, "=="),
            BangEq => write!(f, "!="),
            LtEq => write!(f, "<="),
            GtEq => write!(f, ">="),
            LArrow => write!(f, "<-"),
            RArrow => write!(f, "->"),
            FatArrow => write!(f, "=>"),
            Lt => write!(f, "<"),
            Gt => write!(f, ">"),
            Eq => write!(f, "="),
            AmpAmp => write!(f, "&&"),
            PipePipe => write!(f, "||"),
            Amp => write!(f, "&"),
            Pipe => write!(f, "|"),
            Caret => write!(f, "^"),
            Tilde => write!(f, "~"),
            Shl => write!(f, "<<"),
            Shr => write!(f, ">>"),
            ColonColon => write!(f, "::"),
            DotDot => write!(f, ".."),
            Dot => write!(f, "."),
            Colon => write!(f, ":"),
            ColonSlash => write!(f, ":/"),
            Semi => write!(f, ";"),
            Comma => write!(f, ","),
            LParen => write!(f, "("),
            RParen => write!(f, ")"),
            LBracket => write!(f, "["),
            RBracket => write!(f, "]"),
            LBrace => write!(f, "{{"),
            RBrace => write!(f, "}}"),
            Bang => write!(f, "!"),
            Question => write!(f, "?"),
            Hash => write!(f, "#"),
            AtSign => write!(f, "@"),
            Underscore => write!(f, "_"),
            TimeLiteral(s) | HexLiteral(s) | BinLiteral(s) | SizedLiteral(s) | FloatLiteral(s)
            | DecLiteral(s) => write!(f, "{s}"),
            StringLit(s) => write!(f, "\"{s}\""),
            Ident(s) => write!(f, "{s}"),
        }
    }
}

pub fn tokenize(source: &str) -> Result<Vec<Token>, Vec<Span>> {
    let mut tokens = Vec::new();
    let mut errors = Vec::new();
    let mut lex = TokenKind::lexer(source);

    while let Some(result) = lex.next() {
        let span = lex.span();
        let span = Span::new(span.start, span.end);
        match result {
            Ok(kind) => tokens.push(Token { kind, span }),
            Err(()) => errors.push(span),
        }
    }

    if errors.is_empty() {
        Ok(tokens)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_lex() {
        let src = "transaction agent env transactor scoreboard tseq pseq";
        let toks = tokenize(src).unwrap();
        assert_eq!(toks[0].kind, TokenKind::Transaction);
        assert_eq!(toks[1].kind, TokenKind::Agent);
        assert_eq!(toks[2].kind, TokenKind::Env);
        assert_eq!(toks[3].kind, TokenKind::Transactor);
        assert_eq!(toks[4].kind, TokenKind::Scoreboard);
        assert_eq!(toks[5].kind, TokenKind::Tseq);
        assert_eq!(toks[6].kind, TokenKind::Pseq);
    }

    #[test]
    fn temporal_operators() {
        let toks = tokenize("a |-> b |=> c ##5 d").unwrap();
        assert_eq!(toks[1].kind, TokenKind::PipeImplies);
        assert_eq!(toks[3].kind, TokenKind::PipeImpliesNext);
        assert_eq!(toks[5].kind, TokenKind::HashHash);
    }

    #[test]
    fn wrapping_arithmetic_operators_tokenize_as_single_tokens() {
        let toks = tokenize("a +% b -% c *% d").unwrap();
        assert_eq!(toks[1].kind, TokenKind::PlusPercent);
        assert_eq!(toks[3].kind, TokenKind::MinusPercent);
        assert_eq!(toks[5].kind, TokenKind::StarPercent);
    }

    #[test]
    fn time_literals() {
        let toks = tokenize("100ns 5ps 4cycles").unwrap();
        assert!(matches!(&toks[0].kind, TokenKind::TimeLiteral(s) if s == "100ns"));
        assert!(matches!(&toks[1].kind, TokenKind::TimeLiteral(s) if s == "5ps"));
        assert!(matches!(&toks[2].kind, TokenKind::TimeLiteral(s) if s == "4cycles"));
    }

    #[test]
    fn distribution_separator() {
        let toks = tokenize("[0..0xFF] :/ 80").unwrap();
        assert_eq!(toks[0].kind, TokenKind::LBracket);
        assert_eq!(toks[5].kind, TokenKind::ColonSlash);
    }

    #[test]
    fn doc_outer_strip_space() {
        let toks = tokenize("/// hello\n").unwrap();
        assert!(matches!(&toks[0].kind, TokenKind::DocOuter(s) if s == "hello"));
    }

    #[test]
    fn float_vs_dec_vs_dotdot() {
        let toks = tokenize("3.14 42 1..10").unwrap();
        assert!(matches!(&toks[0].kind, TokenKind::FloatLiteral(s) if s == "3.14"));
        assert!(matches!(&toks[1].kind, TokenKind::DecLiteral(s) if s == "42"));
        assert!(matches!(&toks[2].kind, TokenKind::DecLiteral(s) if s == "1"));
        assert_eq!(toks[3].kind, TokenKind::DotDot);
        assert!(matches!(&toks[4].kind, TokenKind::DecLiteral(s) if s == "10"));
    }
}
