" Vim syntax file for the HARC verification language
" Language:    HARC (harc-com compiler)
" Maintainer:  arch-hdl-lang project
" File types:  *.harc

if exists("b:current_syntax")
  finish
endif

" ── Top-level construct keywords ─────────────────────────────────────────────
" These open a named block: `keyword Name ... end keyword Name`
syn keyword harcConstruct  test testbench impl transactor agent env scoreboard
syn keyword harcConstruct  sequencer tseq transaction covergroup property pseq
syn keyword harcConstruct  bus regblock addrmap struct enum domain package
syn keyword harcConstruct  function relation contract extend
syn keyword harcConstruct  nextgroup=harcConstructName skipwhite

" `end` closes a block: `end test Name`, `end function`, etc.
syn keyword harcEnd        end

" ── Block-structure keywords ──────────────────────────────────────────────────
" Appear inside construct bodies to declare sub-sections
syn keyword harcBlock      param port let reg comb seq state field instance
syn keyword harcBlock      register hookable watchdog connect emit yield apply
syn keyword harcBlock      run setup check teardown phase scope
syn keyword harcBlock      keep solve_order dist with default bins
syn keyword harcBlock      use thread task init lock weight ref
syn keyword harcBlock      const type kind via at fork branch parallel schedule
syn keyword harcBlock      select sequence guarantee buffer event queue stream
syn keyword harcBlock      sequence clocking cross randomize

" ── Control flow keywords ─────────────────────────────────────────────────────
syn keyword harcControl    if elsif else for in loop while repeat break continue
syn keyword harcControl    return on when after unique match within throughout
syn keyword harcControl    intersect inside wait until across
syn keyword harcControl    join_any join_all join_none
syn keyword harcControl    assert assume cover assume

" ── Verification semantics ────────────────────────────────────────────────────
syn keyword harcVerif      fail stop fatal log logf
syn keyword harcVerif      pre post hookable

" ── Transactor / agent mode + binding ────────────────────────────────────────
syn keyword harcMode       active passive bound to bind blocking

" ── Probe / force / release (DUT-internal access) ─────────────────────────────
syn keyword harcProbe      probe force release

" ── Width-changing method-like keywords ──────────────────────────────────────
" These are method-call syntax, but worth coloring distinctly:
"   <expr>.trunc<N>() / .zext<N>() / .sext<N>() / .resize<N>()
syn match   harcWidthOp    /\.\(trunc\|zext\|sext\|resize\)\>/

" ── Built-in types ────────────────────────────────────────────────────────────
syn keyword harcType       uint sint bits bool bit int prop time
syn keyword harcType       UInt SInt Bool Bit Vec
syn keyword harcType       Clock Reset event buffer state stream queue
syn keyword harcType       Severity Logger String TSeq

" ── Boolean literals ─────────────────────────────────────────────────────────
syn keyword harcBool       true false

" ── Boolean operators (word-form) ─────────────────────────────────────────────
syn keyword harcBoolOp     and or not

" ── Cast operator (postfix `expr as Type`) ────────────────────────────────────
syn keyword harcCast       as

" ── Built-in / system functions ──────────────────────────────────────────────
syn match   harcBuiltinFn  /\$clog2\|\$past\|\$rose\|\$fell\|\$stable/
syn keyword harcBuiltinFn  log logf fail

" ── Operators ────────────────────────────────────────────────────────────────
" Assignment / connection arrows / SVA delays / implications
syn match   harcOp         /<=\|<-\|->\|=>\|::\||->\||=>\|##/
" Arithmetic, bitwise, comparison
syn match   harcOp         /[+\-*\/&|^~%]/
syn match   harcOp         /[=!<>]=\|[<>]/

" ── Numeric literals ─────────────────────────────────────────────────────────
" Sized Verilog-style: 8'd255  16'hFF  4'b1010 (also lower-case h/d/b/o)
syn match   harcSizedLit   /\<[0-9]\+'[bdhoBDHO][0-9a-fA-F_xXzZ]\+\>/
" Hex: 0xFF (any width — wide hex literals are HARC-specific)
syn match   harcHexLit     /\<0[xX][0-9a-fA-F_]\+\>/
" Binary: 0b1010
syn match   harcBinLit     /\<0[bB][01_]\+\>/
" Plain decimal
syn match   harcDecLit     /\<[0-9][0-9_]*\>/

" ── Width parameters in angle brackets: uint<32>, .trunc<8>() ────────────────
syn match   harcAngleNum   /<[0-9][0-9_]*>/  contained
syn region  harcTypeParam  start=/</ end=/>/ contains=harcAngleNum,harcType oneline

" ── Identifiers by naming convention ─────────────────────────────────────────
" UPPER_SNAKE → parameters / constants
syn match   harcParam      /\<[A-Z][A-Z0-9_]\{2,}\>/
" PascalCase → types, test/testbench/transactor/component names
syn match   harcTypeName   /\<[A-Z][a-zA-Z0-9]\+\>/

" ── Enum variant: Name::Variant ───────────────────────────────────────────────
syn match   harcEnumVariant  /\<[A-Z][a-zA-Z0-9]*::[A-Z][a-zA-Z0-9]*\>/

" ── String literals + `${expr}` interpolation ────────────────────────────────
" The interpolation expression isn't recursively syntax-highlighted (vim
" can't easily parse balanced ${...} inside strings without recursion).
" The whole `${...}` is colored as a "special character" inside the string.
syn match   harcStrInterp  /\${[^}]*}/  contained
syn region  harcString     start=/"/ end=/"/ skip=/\\"/ contains=harcStrInterp

" ── Comments (line, outer-doc, inner-doc) ─────────────────────────────────────
" Order matters: more-specific patterns first so they win over plain `//`.
syn match   harcDocOuter   "///.*$"
syn match   harcDocInner   "//!.*$"
syn match   harcLineComment "//.*$"
" Inside doc comments, highlight any TODO/FIXME/XXX/NOTE
syn keyword harcTodoTag    contained TODO FIXME XXX NOTE
syn cluster harcTodoCluster contains=harcTodoTag

" ── Highlight groups ─────────────────────────────────────────────────────────
hi def link harcConstruct     Keyword
hi def link harcEnd           Keyword
hi def link harcBlock         Statement
hi def link harcControl       Conditional
hi def link harcVerif         PreProc
hi def link harcMode          StorageClass
hi def link harcProbe         Special
hi def link harcWidthOp       Function
hi def link harcType          Type
hi def link harcBool          Boolean
hi def link harcBoolOp        Operator
hi def link harcCast          Operator
hi def link harcBuiltinFn     Function
hi def link harcOp            Operator
hi def link harcSizedLit      Number
hi def link harcHexLit        Number
hi def link harcBinLit        Number
hi def link harcDecLit        Number
hi def link harcParam         Constant
hi def link harcTypeName      Structure
hi def link harcEnumVariant   Constant
hi def link harcString        String
hi def link harcStrInterp     SpecialChar
hi def link harcDocOuter      Comment
hi def link harcDocInner      Comment
hi def link harcLineComment   Comment
hi def link harcTodoTag       Todo

let b:current_syntax = "harc"
