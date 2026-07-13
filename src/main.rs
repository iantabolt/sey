use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

/// Find/replace across files, ripgrep-powered.
#[derive(Parser, Debug)]
#[command(name = "sey", version, about)]
struct Cli {
    /// Search pattern (regex by default; use -F for a literal string)
    pattern: String,
    /// Replacement (supports $1 / ${name} capture references)
    replacement: String,
    /// Files or directories to search (default: current directory)
    paths: Vec<PathBuf>,

    /// Case-insensitive match
    #[arg(short = 'i', long)]
    ignore_case: bool,
    /// Match whole words only
    #[arg(short = 'w', long)]
    word: bool,
    /// Treat pattern and replacement as literal text (no regex, no capture refs)
    #[arg(short = 'F', long)]
    fixed_strings: bool,
    /// Apply without asking for confirmation
    #[arg(short = 'y', long)]
    yes: bool,
    /// Only touch files matching this glob (repeatable), e.g. -g '*.kt'
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    globs: Vec<String>,
    /// Lines of context to show around each match in the preview
    #[arg(short = 'C', long, default_value_t = 2)]
    context: usize,
    /// Include hidden files and files excluded by .gitignore
    #[arg(long)]
    no_ignore: bool,
}

/// A replaced line, split into (text, is_replacement) segments for highlighting.
struct Change {
    line_idx: usize,
    segments: Vec<(String, bool)>,
}

struct FileEdit {
    path: PathBuf,
    lines: Vec<String>, // raw lines, each keeping its trailing '\n' if present
    changes: Vec<Change>,
    new_text: String,
    matches: usize,
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    if cli.paths.is_empty() {
        cli.paths.push(PathBuf::from("."));
    }

    let re = build_regex(&cli)?;

    let edits = collect_edits(&cli, &re)?;
    if edits.is_empty() {
        eprintln!("No matches.");
        return Ok(());
    }

    print_preview(&edits, cli.context)?;

    let total: usize = edits.iter().map(|e| e.matches).sum();
    eprintln!("\n{} matches in {} files", total, edits.len());

    if !cli.yes && !confirm()? {
        eprintln!("Aborted.");
        return Ok(());
    }

    for edit in &edits {
        fs::write(&edit.path, &edit.new_text).with_context(|| format!("writing {}", edit.path.display()))?;
    }
    eprintln!("Replaced {} matches across {} files.", total, edits.len());
    Ok(())
}

fn build_regex(cli: &Cli) -> Result<Regex> {
    let mut pat = if cli.fixed_strings {
        regex::escape(&cli.pattern)
    } else {
        cli.pattern.clone()
    };
    if cli.word {
        pat = format!(r"\b(?:{})\b", pat);
    }
    RegexBuilder::new(&pat)
        .case_insensitive(cli.ignore_case)
        .build()
        .with_context(|| format!("invalid pattern: {}", cli.pattern))
}

fn collect_edits(cli: &Cli, re: &Regex) -> Result<Vec<FileEdit>> {
    let mut wb = WalkBuilder::new(&cli.paths[0]);
    for p in &cli.paths[1..] {
        wb.add(p);
    }
    if cli.no_ignore {
        wb.git_ignore(false).git_global(false).git_exclude(false).ignore(false).hidden(false);
    }
    if !cli.globs.is_empty() {
        let mut ob = OverrideBuilder::new(".");
        for g in &cli.globs {
            ob.add(g)?;
        }
        wb.overrides(ob.build()?);
    }

    let mut edits = Vec::new();
    for dent in wb.build() {
        let dent = match dent {
            Ok(d) => d,
            Err(_) => continue,
        };
        if !dent.file_type().map_or(false, |t| t.is_file()) {
            continue;
        }
        if let Some(edit) = process_file(dent.into_path(), re, &cli.replacement, cli.fixed_strings) {
            edits.push(edit);
        }
    }
    Ok(edits)
}

fn process_file(path: PathBuf, re: &Regex, repl: &str, fixed: bool) -> Option<FileEdit> {
    let text = fs::read_to_string(&path).ok()?; // skips binary / non-UTF-8 files
    let lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();

    let mut changes = Vec::new();
    let mut new_text = String::with_capacity(text.len());
    let mut matches = 0;

    for (idx, raw) in lines.iter().enumerate() {
        let (body, nl) = split_nl(raw);
        if re.is_match(body) {
            let (segments, count) = segment_line(body, re, repl, fixed);
            let new_body: String = segments.iter().map(|(t, _)| t.as_str()).collect();
            if new_body != body {
                matches += count;
                changes.push(Change { line_idx: idx, segments });
                new_text.push_str(&new_body);
                new_text.push_str(nl);
                continue;
            }
        }
        new_text.push_str(raw);
    }

    if changes.is_empty() {
        None
    } else {
        Some(FileEdit { path, lines, changes, new_text, matches })
    }
}

/// Split each match out of `body`, expanding the replacement so we can highlight it.
fn segment_line(body: &str, re: &Regex, repl: &str, fixed: bool) -> (Vec<(String, bool)>, usize) {
    let mut segments = Vec::new();
    let mut last = 0;
    let mut count = 0;
    for caps in re.captures_iter(body) {
        let m = caps.get(0).unwrap();
        if m.start() > last {
            segments.push((body[last..m.start()].to_string(), false));
        }
        let mut rep = String::new();
        if fixed {
            rep.push_str(repl);
        } else {
            caps.expand(repl, &mut rep);
        }
        segments.push((rep, true));
        last = m.end();
        count += 1;
    }
    if last < body.len() {
        segments.push((body[last..].to_string(), false));
    }
    (segments, count)
}

fn split_nl(s: &str) -> (&str, &str) {
    match s.strip_suffix('\n') {
        Some(stripped) => (stripped, "\n"),
        None => (s, ""),
    }
}

fn print_preview(edits: &[FileEdit], context: usize) -> io::Result<()> {
    let mut out = StandardStream::stdout(ColorChoice::Auto);

    let path_spec = spec(Color::Magenta, true, false);
    let num_spec = spec(Color::Green, false, false);
    let dim_spec = {
        let mut s = ColorSpec::new();
        s.set_dimmed(true);
        s
    };
    let rep_spec = spec(Color::Green, true, false);

    for edit in edits {
        out.set_color(&path_spec)?;
        writeln!(out, "{}", edit.path.display())?;
        out.reset()?;

        for (ci, change) in edit.changes.iter().enumerate() {
            let i = change.line_idx;
            let start = i.saturating_sub(context);
            let end = (i + context).min(edit.lines.len().saturating_sub(1));
            for j in start..=end {
                let (body, _) = split_nl(&edit.lines[j]);
                out.set_color(&num_spec)?;
                write!(out, "{:>6}  ", j + 1)?;
                out.reset()?;
                if j == i {
                    for (text, is_rep) in &change.segments {
                        if *is_rep {
                            out.set_color(&rep_spec)?;
                            write!(out, "{}", text)?;
                            out.reset()?;
                        } else {
                            write!(out, "{}", text)?;
                        }
                    }
                    writeln!(out)?;
                } else {
                    out.set_color(&dim_spec)?;
                    writeln!(out, "{}", body)?;
                    out.reset()?;
                }
            }
            if ci + 1 < edit.changes.len() {
                out.set_color(&dim_spec)?;
                writeln!(out, "        ⋮")?;
                out.reset()?;
            }
        }
        writeln!(out)?;
    }
    Ok(())
}

fn spec(fg: Color, bold: bool, dimmed: bool) -> ColorSpec {
    let mut s = ColorSpec::new();
    s.set_fg(Some(fg)).set_bold(bold).set_dimmed(dimmed);
    s
}

fn confirm() -> io::Result<bool> {
    eprint!("Apply these changes? [y/N] ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let ans = input.trim().to_lowercase();
    Ok(ans == "y" || ans == "yes")
}
