# sey

_`sey` what was once `sed`._

Find/replace across a directory tree, using ripgrep's regex engine and
file-walking conventions (`.gitignore` aware, glob/type filters, etc.), with
the browse-and-replace ergonomics of IntelliJ's find/replace-in-path dialog.

```
sey 'foo(\w+)' 'bar$1' src/
```

If the results fit on one screen, `sey` just prints a diff-style preview and
asks once:

![sey printing a diff preview for a small change, then applying it](demo/basic.gif)

Bigger result sets (or `-p`) drop into a two-pane TUI instead: a compact match
list up top, full file context below. Arrow keys move between matches, Enter
replaces the current one and advances, `e` lets you tweak the pattern/replacement
live and re-search, and `q` writes whatever you've accepted so far and quits:

![sey browsing matches in the two-pane TUI, replacing some, skipping one, then quitting with a partial write](demo/tui.gif)

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
sey [OPTIONS] [PATTERN] [REPLACEMENT] [PATHS]...
```

`PATTERN`/`REPLACEMENT` are optional: omit them (or pass `-e`) to launch the
TUI straight into its live pattern editor instead of typing a full command
line up front.

| Flag | Meaning |
|---|---|
| `-f`, `--files <FILE>` | extra files/dirs to search (repeatable), e.g. `-f (fd -e py)` |
| `-e`, `--edit` | launch straight into the live pattern/replacement editor |
| `-i`, `--ignore-case` | case-insensitive match |
| `-w`, `--word` | match whole words only |
| `-F`, `--fixed-strings` | treat pattern/replacement as literal text |
| `-y`, `--yes` | apply every change immediately, no preview or UI |
| `-p`, `--pager` | always open the two-pane TUI |
| `-P`, `--no-pager` | never open the TUI; print results and prompt inline |
| `-g`, `--glob <GLOB>` | only touch matching files (repeatable) |
| `-t`, `--type <TYPE>` | only touch files of this type (repeatable), e.g. `-t rust` |
| `-T`, `--type-not <TYPE>` | skip files of this type (repeatable) |
| `--type-list` | list all supported file types and their globs |
| `-C`, `--context <N>` | lines of context around each match (default 2) |
| `-c`, `--compact` | one line per match instead of full diff+context |
| `--no-ignore` | include hidden/gitignored files |
| `--preview <old\|new\|diff>` | preview style (default `diff`) |
| `--vimgrep` | print `file:line:col:content`, one per match, no replacement |

`REPLACEMENT` supports capture references (`$1`, `${name}`) unless `-F` is
given.

### In the TUI

`↑`/`↓` move between matches, `Enter` replaces the current match and moves to
the next, `e` jumps into the pattern/replacement fields (typing re-searches
live, `Tab` switches fields, `Enter` returns to browsing, `Esc` reverts), and
`q` writes everything you've accepted so far and exits — so quitting early
leaves a partial replacement, not an aborted one. `Ctrl+C` aborts without
writing anything.
`Shift+Enter` replaces every remaining match at once, though whether it's
recognized as distinct from plain `Enter` depends on your terminal's keyboard
protocol support.

## Status

Early and under active development. Flags and defaults may still change.

## Acknowledgements

File walking and `.gitignore` handling via the [`ignore`](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore) crate, and regex matching via the [`regex`](https://github.com/rust-lang/regex) crate — both foundational to [ripgrep](https://github.com/BurntSushi/ripgrep) and maintained largely by [Andrew Gallant](https://github.com/BurntSushi).

## License

MIT — see [LICENSE](LICENSE).
