# HARC editor support

Syntax highlighting + filetype detection for HARC `.harc` files.
Mirrors the layout used by sister project arch-com's `editors/`
directory.

## Vim

Drop the three files into your vim runtime:

```sh
mkdir -p ~/.vim/{ftdetect,ftplugin,syntax}
cp editors/vim/ftdetect/harc.vim  ~/.vim/ftdetect/
cp editors/vim/ftplugin/harc.vim  ~/.vim/ftplugin/
cp editors/vim/syntax/harc.vim    ~/.vim/syntax/
```

Or symlink them so future updates from `git pull` flow through:

```sh
ln -s "$PWD/editors/vim/ftdetect/harc.vim" ~/.vim/ftdetect/
ln -s "$PWD/editors/vim/ftplugin/harc.vim" ~/.vim/ftplugin/
ln -s "$PWD/editors/vim/syntax/harc.vim"   ~/.vim/syntax/
```

Provides:
- Auto-detection on `*.harc`
- 4-space indentation (matches `harc fmt` output)
- Comment-string for `gc`/`commentary` plugins (`// ...`)
- Highlighting for HARC's construct / control / mode / verif keywords,
  built-in types (`uint<N>` / `sint<N>` / `bits<N>` / `Clock` / etc.),
  numeric literals (sized `8'd255`, hex `0xFF`, binary `0b1010`),
  width-method calls (`.trunc<8>()` / `.zext<N>()` / `.sext<N>()` /
  `.resize<N>()`), and `${...}` interpolation inside strings.

## VSCode

Install via the local extensions folder:

```sh
cp -R editors/vscode ~/.vscode/extensions/harc-0.1.0
```

Or package + install with `vsce`:

```sh
cd editors/vscode
vsce package
code --install-extension harc-0.1.0.vsix
```

Restart VSCode after install. Same highlighting taxonomy as the Vim
syntax, plus the language-configuration's auto-closing brackets and
angle-bracket pairing for generic types.

## Maintenance

These syntax files are hand-maintained against `src/lexer.rs`'s
keyword set. When a new keyword lands (or an old one is removed),
update the matching `keyword`-list in:

- `editors/vim/syntax/harc.vim` — relevant `syn keyword harc<group>` line
- `editors/vscode/syntaxes/harc.tmLanguage.json` — relevant `\\b(...)\\b` pattern

Both files group keywords the same way (constructs, control flow,
block keywords, mode, probe, types, etc.) so the diff is symmetric.
