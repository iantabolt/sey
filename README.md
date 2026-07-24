# sey

_`sey` what was once `sed`._

Interactive find/replace across a directory tree, using ripgrep's regex
engine and file-walking conventions (`.gitignore` aware, glob filters, etc.)
with a preview-before-you-commit workflow modeled on IntelliJ's
find/replace-in-path dialog.

```
sey 'foo(\w+)' 'bar$1' src/
```

![sey renaming a function across a few files, then confirming the change](demo/basic.gif)

Results stream in as files are found. By default `sey` shows a diff-style
preview of every change and asks for confirmation once before writing anything.

Pass `-I` to step through each match one at a time (`y`/`n`/`a`/`q`):

![sey stepping through matches one at a time in interactive mode](demo/interactive.gif)

Pipe the output to another command and `sey` switches to a plain
`file:line:col:content` listing instead — like `rg --vimgrep` — and makes no
changes, so it composes cleanly with the rest of your shell:

![sey printing a rich preview normally, then plain vimgrep-style output once piped](demo/pipe.gif)

## Install

```
curl -fsSL https://raw.githubusercontent.com/iantabolt/sey/master/install.sh | sh
```

This builds `sey` from source with `cargo`, so you'll need Rust installed
(https://rustup.rs). Prebuilt binaries / Homebrew are on the roadmap.

## Usage

```
sey [OPTIONS] <PATTERN> <REPLACEMENT> [PATHS]...
```

| Flag | Meaning |
|---|---|
| `-i`, `--ignore-case` | case-insensitive match |
| `-w`, `--word` | match whole words only |
| `-F`, `--fixed-strings` | treat pattern/replacement as literal text |
| `-g`, `--glob <GLOB>` | only touch matching files (repeatable) |
| `-t`, `--type <TYPE>` | only touch files of this type (repeatable), e.g. `-t rust` |
| `-T`, `--type-not <TYPE>` | skip files of this type (repeatable) |
| `--type-list` | list all supported file types and their globs |
| `-C`, `--context <N>` | lines of context around each match (default 2) |
| `-I`, `--interactive` | review and apply changes one at a time |
| `-y`, `--yes` | apply without confirmation |
| `-c`, `--compact` | one line per match instead of full diff+context |
| `--no-ignore` | include hidden/gitignored files |
| `--preview <old\|new\|diff>` | preview style (default `diff`) |
| `--no-pager` | don't pipe output through a pager |
| `--vimgrep` | print `file:line:col:content`, one per match, no replacement |

`REPLACEMENT` supports capture references (`$1`, `${name}`) unless `-F` is
given.

## Status

Early and under active development. Flags and defaults may still change.

## Acknowledgements

File walking and `.gitignore` handling via the [`ignore`](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore) crate, and regex matching via the [`regex`](https://github.com/rust-lang/regex) crate — both foundational to [ripgrep](https://github.com/BurntSushi/ripgrep) and maintained largely by [Andrew Gallant](https://github.com/BurntSushi).

## License

MIT — see [LICENSE](LICENSE).
