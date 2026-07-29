# sey

_`sey` what was once `sed`._

Ripgrep's regex and file walking. IntelliJ's find/replace ergonomics.

```
sey 'foo(\w+)' 'bar$1' src/
```

Fits on screen → diff preview, one prompt.

![diff preview, then applying it](demo/basic.gif)

Bigger results (or `-p`) → two-pane TUI.

![browsing and replacing matches in the TUI](demo/tui.gif)

Piped → plain `file:line:col:content`, like `rg --vimgrep`. No writes.

![plain vimgrep-style output once piped](demo/pipe.gif)

## Install

```
curl -fsSL https://raw.githubusercontent.com/iantabolt/sey/master/install.sh | sh
```

Builds from source via `cargo`. Needs Rust (https://rustup.rs).

## Usage

```
sey [OPTIONS] [PATTERN] [REPLACEMENT] [PATHS]...
```

Omit `PATTERN`/`REPLACEMENT` (or pass `-e`) to launch the live editor.

| Flag | Meaning |
|---|---|
| `-f`, `--files <FILE>` | extra files/dirs to search (repeatable) |
| `-e`, `--edit` | launch straight into the live editor |
| `-i`, `--ignore-case` | case-insensitive match |
| `-w`, `--word` | match whole words only |
| `-F`, `--fixed-strings` | literal text, no regex |
| `-y`, `--yes` | apply immediately, no UI |
| `-p`, `--pager` | always open the TUI |
| `-P`, `--no-pager` | never open the TUI |
| `-g`, `--glob <GLOB>` | only matching files (repeatable) |
| `-t`, `--type <TYPE>` | only this file type (repeatable) |
| `-T`, `--type-not <TYPE>` | skip this file type (repeatable) |
| `--type-list` | list file types and exit |
| `-C`, `--context <N>` | context lines (default 2) |
| `-c`, `--compact` | one line per match |
| `--no-ignore` | include hidden/gitignored files |
| `--preview <old\|new\|diff>` | preview style (default `diff`) |
| `--vimgrep` | `file:line:col:content`, no replacement |

`REPLACEMENT` supports `$1` / `${name}`, unless `-F`.

### In the TUI

| Key | Action |
|---|---|
| `↑` / `↓` | move between matches |
| `Enter` | replace current match, advance |
| `Shift+Enter` | replace all (terminal-dependent) |
| `e` | edit pattern/replacement live |
| `Tab` | switch field, while editing |
| `q` | write accepted matches, quit |
| `Ctrl+C` | abort, write nothing |

## Status

Early. Flags and defaults may still change.

## Acknowledgements

- [`ignore`](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore) — file walking, `.gitignore`
- [`regex`](https://github.com/rust-lang/regex) — the regex engine
- both via [ripgrep](https://github.com/BurntSushi/ripgrep) / [Andrew Gallant](https://github.com/BurntSushi)

## License

MIT — see [LICENSE](LICENSE).
