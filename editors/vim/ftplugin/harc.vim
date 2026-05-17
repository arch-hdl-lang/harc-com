" Vim filetype plugin for the HARC verification language

if exists("b:did_ftplugin")
  finish
endif
let b:did_ftplugin = 1

" Indentation: 4 spaces, no tabs (matches pretty.rs INDENT)
setlocal expandtab
setlocal shiftwidth=4
setlocal softtabstop=4
setlocal tabstop=4

" Line comments. HARC has three slash-prefixed comment forms:
"   //   line comment
"   ///  outer doc comment (attached to following item)
"   //!  inner doc comment (attached to enclosing item)
" The commentstring covers the default editor commenting flow; the
" syntax file colors all three forms.
setlocal commentstring=//\ %s
setlocal comments=://!,:///,://

" Don't wrap long lines (testbenches often have wide tabular asserts)
setlocal textwidth=0

" Matching pairs: angle brackets for types like uint<32> and .trunc<8>()
setlocal matchpairs+=<:>

" Fold on construct blocks (end keyword Name)
setlocal foldmethod=syntax

let b:undo_ftplugin = "setl et< sw< sts< ts< cms< com< tw< mps< fdm<"
