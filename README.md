# sey

Interactive find/replace across a directory tree, using ripgrep's regex
engine and file-walking conventions (`.gitignore` aware, glob filters, etc.)
with a preview-before-you-commit workflow modeled on IntelliJ's
find/replace-in-path dialog.

```
sey 'foo(\w+)' 'bar$1' src/
```

Results stream in as files are found. By default `sey` shows a diff-style
preview of every change and asks for confirmation once before writing anything.
Pass `-I` to step through each match one at a time (`y`/`n`/`a`/`q`), or `-y`
to skip confirmation entirely.

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
| `-C`, `--context <N>` | lines of context around each match (default 2) |
| `-I`, `--interactive` | review and apply changes one at a time |
| `-y`, `--yes` | apply without confirmation |
| `--no-ignore` | include hidden/gitignored files |
| `--preview <old\|new\|diff>` | preview style (default `diff`) |

`REPLACEMENT` supports capture references (`$1`, `${name}`) unless `-F` is
given.

## Status

Early and under active development. Flags and defaults may still change.
