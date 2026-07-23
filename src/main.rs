use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

#[derive(clap::ValueEnum, Debug, Clone, PartialEq)]
enum Preview {
    /// Show original line with matches highlighted (like IntelliJ's list view)
    Old,
    /// Show replacement line with new text highlighted
    New,
    /// Show old and new lines side-by-side (diff style)
    Diff,
}

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
    /// Preview style: old (highlight matches), new (highlight replacements), diff (show both)
    #[arg(long, value_enum, default_value = "diff")]
    preview: Preview,
}

struct Change {
    line_idx: usize,
    orig_segments: Vec<(String, bool)>,
    new_segments: Vec<(String, bool)>,
    match_count: usize,
}

struct FileEdit {
    path: PathBuf,
    lines: Vec<String>,
    changes: Vec<Change>,
    new_text: String,
}

impl FileEdit {
    fn total_matches(&self) -> usize {
        self.changes.iter().map(|c| c.match_count).sum()
    }
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

    print_preview(&edits, cli.context, &cli.preview)?;

    let total: usize = edits.iter().map(|e| e.total_matches()).sum();
    eprintln!("\n{} matches in {} files", total, edits.len());

    if !cli.yes && !confirm()? {
        eprintln!("Aborted.");
        return Ok(());
    }

    for edit in &edits {
        fs::write(&edit.path, &edit.new_text)
            .with_context(|| format!("writing {}", edit.path.display()))?;
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
        wb.git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .hidden(false);
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
    let text = fs::read_to_string(&path).ok()?;
    let lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();

    let mut changes = Vec::new();
    let mut new_text = String::with_capacity(text.len());

    for (idx, raw) in lines.iter().enumerate() {
        let (body, nl) = split_nl(raw);
        if re.is_match(body) {
            let orig_segments = segment_original(body, re);
            let (new_segments, match_count) = segment_replacement(body, re, repl, fixed);
            let new_body: String = new_segments.iter().map(|(t, _)| t.as_str()).collect();
            if new_body != body {
                changes.push(Change { line_idx: idx, orig_segments, new_segments, match_count });
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
        Some(FileEdit { path, lines, changes, new_text })
    }
}

fn segment_original(body: &str, re: &Regex) -> Vec<(String, bool)> {
    let mut segments = Vec::new();
    let mut last = 0;
    for m in re.find_iter(body) {
        if m.start() > last {
            segments.push((body[last..m.start()].to_string(), false));
        }
        segments.push((body[m.start()..m.end()].to_string(), true));
        last = m.end();
    }
    if last < body.len() {
        segments.push((body[last..].to_string(), false));
    }
    segments
}

fn segment_replacement(body: &str, re: &Regex, repl: &str, fixed: bool) -> (Vec<(String, bool)>, usize) {
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

struct Hunk {
    start: usize,
    end: usize,
}

/// Merge overlapping or adjacent context windows into contiguous hunks.
fn build_hunks(changes: &[Change], context: usize, total_lines: usize) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for change in changes {
        let start = change.line_idx.saturating_sub(context);
        let end = (change.line_idx + context).min(total_lines.saturating_sub(1));
        if let Some(last) = hunks.last_mut() {
            if start <= last.end + 1 {
                last.end = last.end.max(end);
                continue;
            }
        }
        hunks.push(Hunk { start, end });
    }
    hunks
}

fn print_preview(edits: &[FileEdit], context: usize, preview: &Preview) -> io::Result<()> {
    let mut out = StandardStream::stdout(ColorChoice::Auto);

    let path_spec = spec(Color::Magenta, true);
    let num_spec = spec(Color::Cyan, false);
    let yellow_bold = spec(Color::Yellow, true);
    let green_bold = spec(Color::Green, true);
    let red_bold = spec(Color::Red, true);
    let dim_spec = {
        let mut s = ColorSpec::new();
        s.set_dimmed(true);
        s
    };

    for edit in edits {
        out.set_color(&path_spec)?;
        writeln!(out, "{}", edit.path.display())?;
        out.reset()?;

        let change_map: HashMap<usize, &Change> =
            edit.changes.iter().map(|c| (c.line_idx, c)).collect();
        let hunks = build_hunks(&edit.changes, context, edit.lines.len());

        for (hi, hunk) in hunks.iter().enumerate() {
            if hi > 0 {
                out.set_color(&dim_spec)?;
                writeln!(out, "        ⋮")?;
                out.reset()?;
            }

            for line_idx in hunk.start..=hunk.end {
                let (body, _) = split_nl(&edit.lines[line_idx]);

                if let Some(change) = change_map.get(&line_idx) {
                    match preview {
                        Preview::Old => {
                            out.set_color(&num_spec)?;
                            write!(out, "{:>6}  ", line_idx + 1)?;
                            out.reset()?;
                            write_segments(&mut out, &change.orig_segments, &yellow_bold)?;
                            writeln!(out)?;
                        }
                        Preview::New => {
                            out.set_color(&num_spec)?;
                            write!(out, "{:>6}  ", line_idx + 1)?;
                            out.reset()?;
                            write_segments(&mut out, &change.new_segments, &green_bold)?;
                            writeln!(out)?;
                        }
                        Preview::Diff => {
                            out.set_color(&red_bold)?;
                            write!(out, "-")?;
                            out.set_color(&num_spec)?;
                            write!(out, "{:>5}  ", line_idx + 1)?;
                            out.reset()?;
                            write_segments(&mut out, &change.orig_segments, &red_bold)?;
                            writeln!(out)?;

                            out.set_color(&green_bold)?;
                            write!(out, "+")?;
                            out.set_color(&num_spec)?;
                            write!(out, "{:>5}  ", line_idx + 1)?;
                            out.reset()?;
                            write_segments(&mut out, &change.new_segments, &green_bold)?;
                            writeln!(out)?;
                        }
                    }
                } else {
                    if *preview == Preview::Diff {
                        out.set_color(&dim_spec)?;
                        write!(out, " {:>5}  {}", line_idx + 1, body)?;
                        out.reset()?;
                        writeln!(out)?;
                    } else {
                        out.set_color(&num_spec)?;
                        write!(out, "{:>6}  ", line_idx + 1)?;
                        out.reset()?;
                        out.set_color(&dim_spec)?;
                        write!(out, "{}", body)?;
                        out.reset()?;
                        writeln!(out)?;
                    }
                }
            }
        }
        writeln!(out)?;
    }
    Ok(())
}

fn write_segments(
    out: &mut StandardStream,
    segments: &[(String, bool)],
    highlight: &ColorSpec,
) -> io::Result<()> {
    for (text, is_highlighted) in segments {
        if *is_highlighted {
            out.set_color(highlight)?;
        } else {
            out.reset()?;
        }
        write!(out, "{}", text)?;
    }
    out.reset()
}

fn spec(fg: Color, bold: bool) -> ColorSpec {
    let mut s = ColorSpec::new();
    s.set_fg(Some(fg)).set_bold(bold);
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
