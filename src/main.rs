use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{poll, read as read_event, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ignore::overrides::OverrideBuilder;
use ignore::types::{Types, TypesBuilder};
use ignore::WalkBuilder;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use regex::{Regex, RegexBuilder};
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

#[derive(clap::ValueEnum, Debug, Clone, PartialEq)]
enum Preview {
    /// Show original line with matches highlighted (like IntelliJ's list view)
    Old,
    /// Show replacement line with new text highlighted
    New,
    /// Show both old and new lines (diff style)
    Diff,
}

/// Find/replace across files, ripgrep-powered.
#[derive(Parser, Debug)]
#[command(name = "sey", version, about)]
struct Cli {
    /// Search pattern (regex by default; use -F for a literal string)
    pattern: Option<String>,
    /// Replacement (supports $1 / ${name} capture references)
    replacement: Option<String>,
    /// Files or directories to search (default: current directory)
    paths: Vec<PathBuf>,
    /// Extra files or directories to search (useful with shell substitution: -f (fd …))
    #[arg(short = 'f', long = "files", value_name = "FILE")]
    files: Vec<PathBuf>,
    /// Open live pattern/replacement editor (launches TUI directly)
    #[arg(short = 'e', long)]
    edit: bool,

    /// Case-insensitive match
    #[arg(short = 'i', long)]
    ignore_case: bool,
    /// Match whole words only
    #[arg(short = 'w', long)]
    word: bool,
    /// Treat pattern and replacement as literal text (no regex, no capture refs)
    #[arg(short = 'F', long)]
    fixed_strings: bool,
    /// Apply all changes immediately without confirmation or UI
    #[arg(short = 'y', long)]
    yes: bool,
    /// Always open the interactive TUI pager
    #[arg(short = 'p', long)]
    pager: bool,
    /// Never open the TUI; print results and prompt inline
    #[arg(short = 'P', long)]
    no_pager: bool,
    /// Only touch files matching this glob (repeatable), e.g. -g '*.kt'
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    globs: Vec<String>,
    /// Only touch files of this type (repeatable), e.g. -t rust. See --type-list.
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    type_matches: Vec<String>,
    /// Skip files of this type (repeatable), e.g. -T markdown
    #[arg(short = 'T', long = "type-not", value_name = "TYPE")]
    type_not: Vec<String>,
    /// List all supported file types and their glob patterns, then exit
    #[arg(long)]
    type_list: bool,
    /// Lines of context to show around each match in the preview
    #[arg(short = 'C', long, default_value_t = 2)]
    context: usize,
    /// Include hidden files and files excluded by .gitignore
    #[arg(long)]
    no_ignore: bool,
    /// Preview style: old (highlight matches), new (highlight replacements), diff (show both)
    #[arg(long, value_enum, default_value = "diff")]
    preview: Preview,
    /// One line per match instead of the full diff+context view
    #[arg(short = 'c', long)]
    compact: bool,
    /// Output matches as file:line:col:content (one line per match, no replacement applied).
    /// Automatically enabled when stdout is piped.
    #[arg(long)]
    vimgrep: bool,
}

#[derive(Clone)]
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
    noop_count: usize,
}

impl FileEdit {
    fn total_matches(&self) -> usize {
        self.changes.iter().map(|c| c.match_count).sum()
    }
}

struct MatchEntry {
    edit_idx: usize,
    change_idx: usize,
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();

    if cli.type_list {
        print_type_list();
        return Ok(());
    }

    // Merge -f/--files into paths so the rest of the code sees one unified list.
    cli.paths.extend(std::mem::take(&mut cli.files));

    // Validate -t/-T up front so a typo'd type name fails fast with a clear
    // message, rather than silently matching zero files deep in a TUI.
    build_types(&cli)?;

    if cli.pattern.is_none() || cli.edit {
        // Edit mode: any positionals clap assigned to pattern/replacement slots are paths.
        let mut extra: Vec<PathBuf> = Vec::new();
        if let Some(p) = cli.pattern.take() { extra.push(PathBuf::from(p)); }
        if let Some(r) = cli.replacement.take() { extra.push(PathBuf::from(r)); }
        for p in extra { cli.paths.insert(0, p); }
        if cli.paths.is_empty() { cli.paths.push(PathBuf::from(".")); }
        return run_tui(&cli, "", "", true, None, 0);
    }

    if cli.replacement.is_none() {
        eprintln!("error: REPLACEMENT is required when PATTERN is given");
        std::process::exit(1);
    }

    if cli.paths.is_empty() {
        cli.paths.push(PathBuf::from("."));
    }

    let pattern = cli.pattern.as_deref().unwrap();
    let replacement = cli.replacement.as_deref().unwrap_or("");
    let re = build_regex(&cli, pattern)?;
    let rx = spawn_search(&cli, re, replacement.to_string());

    run_batch(rx, &cli)
}

fn build_regex(cli: &Cli, pattern: &str) -> Result<Regex> {
    let mut pat = if cli.fixed_strings {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    if cli.word {
        pat = format!(r"\b(?:{})\b", pat);
    }
    RegexBuilder::new(&pat)
        .case_insensitive(cli.ignore_case)
        .build()
        .with_context(|| format!("invalid pattern: {}", pattern))
}

fn build_types(cli: &Cli) -> Result<Option<Types>> {
    if cli.type_matches.is_empty() && cli.type_not.is_empty() {
        return Ok(None);
    }
    let mut tb = TypesBuilder::new();
    tb.add_defaults();
    for t in &cli.type_matches {
        tb.select(t);
    }
    for t in &cli.type_not {
        tb.negate(t);
    }
    let types = tb.build().context("invalid file type (see --type-list for available types)")?;
    Ok(Some(types))
}

fn print_type_list() {
    let mut tb = TypesBuilder::new();
    tb.add_defaults();
    for def in tb.definitions() {
        println!("{}: {}", def.name(), def.globs().join(", "));
    }
}

fn spawn_search(cli: &Cli, re: Regex, replacement: String) -> mpsc::Receiver<FileEdit> {
    let (tx, rx) = mpsc::channel();

    let paths = cli.paths.clone();
    let globs = cli.globs.clone();
    let type_matches = cli.type_matches.clone();
    let type_not = cli.type_not.clone();
    let no_ignore = cli.no_ignore;
    let fixed = cli.fixed_strings;

    thread::spawn(move || {
        let mut wb = WalkBuilder::new(&paths[0]);
        for p in &paths[1..] {
            wb.add(p);
        }
        if no_ignore {
            wb.git_ignore(false)
                .git_global(false)
                .git_exclude(false)
                .ignore(false)
                .hidden(false);
        }
        if !globs.is_empty() {
            let mut ob = OverrideBuilder::new(".");
            for g in &globs {
                ob.add(g).ok();
            }
            if let Ok(overrides) = ob.build() {
                wb.overrides(overrides);
            }
        }
        if !type_matches.is_empty() || !type_not.is_empty() {
            let mut tb = TypesBuilder::new();
            tb.add_defaults();
            for t in &type_matches {
                tb.select(t);
            }
            for t in &type_not {
                tb.negate(t);
            }
            // Already validated in main() before this thread was spawned.
            if let Ok(types) = tb.build() {
                wb.types(types);
            }
        }

        for dent in wb.build() {
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue,
            };
            if !dent.file_type().map_or(false, |t| t.is_file()) {
                continue;
            }
            if let Some(edit) = process_file(dent.into_path(), &re, &replacement, fixed) {
                if tx.send(edit).is_err() {
                    break;
                }
            }
        }
    });

    rx
}

// ── Batch mode ───────────────────────────────────────────────────────────────

fn run_batch(rx: mpsc::Receiver<FileEdit>, cli: &Cli) -> Result<()> {
    // Piped / vimgrep: structured output, no UI, no changes.
    if cli.vimgrep || !io::stdout().is_terminal() {
        let mut out = StandardStream::stdout(ColorChoice::Never);
        for edit in &rx { print_file_vimgrep(&edit, &mut out)?; }
        return Ok(());
    }

    let pat = cli.pattern.as_deref().unwrap_or("");
    let rep = cli.replacement.as_deref().unwrap_or("");

    // -y: silent apply, no UI.
    if cli.yes {
        let edits: Vec<FileEdit> = rx.into_iter().collect();
        let noop_total: usize = edits.iter().map(|e| e.noop_count).sum();
        let edits: Vec<FileEdit> = edits.into_iter().filter(|e| !e.changes.is_empty()).collect();
        if edits.is_empty() {
            eprintln!("{}", no_change_message(noop_total));
            return Ok(());
        }
        let total: usize = edits.iter().map(|e| e.total_matches()).sum();
        for edit in &edits {
            fs::write(&edit.path, &edit.new_text)
                .with_context(|| format!("writing {}", edit.path.display()))?;
        }
        eprintln!("Replaced {total} matches across {} files.{}", edits.len(), noop_suffix(noop_total));
        return Ok(());
    }

    // -p/--pager: force TUI (start search fresh inside TUI).
    if cli.pager {
        return run_tui(cli, pat, rep, false, None, 0);
    }

    // Collect all results first so we can decide plain vs TUI.
    let edits: Vec<FileEdit> = rx.into_iter().collect();
    let noop_total: usize = edits.iter().map(|e| e.noop_count).sum();
    let edits: Vec<FileEdit> = edits.into_iter().filter(|e| !e.changes.is_empty()).collect();
    if edits.is_empty() {
        eprintln!("{}", no_change_message(noop_total));
        return Ok(());
    }
    let total: usize = edits.iter().map(|e| e.total_matches()).sum();

    // -P/--no-pager: force plain output.
    // Default: use plain if output fits on screen, otherwise open TUI.
    let term_h = crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24);
    let estimated_lines = estimate_output_lines(&edits, cli);
    let use_plain = cli.no_pager || estimated_lines <= term_h;

    if use_plain {
        let mut out = StandardStream::stdout(ColorChoice::Auto);
        for edit in &edits {
            if cli.compact {
                print_file_compact(edit, &cli.preview, &mut out)?;
            } else {
                print_file_preview(edit, cli.context, &cli.preview, &mut out)?;
            }
        }
        match prompt_no_pager(total, edits.len(), noop_total)? {
            PromptChoice::ReplaceAll => {
                for edit in &edits {
                    fs::write(&edit.path, &edit.new_text)
                        .with_context(|| format!("writing {}", edit.path.display()))?;
                }
                eprintln!("Replaced {total} matches across {} files.{}", edits.len(), noop_suffix(noop_total));
            }
            PromptChoice::Quit => eprintln!("Aborted."),
            PromptChoice::Edit => run_tui(cli, pat, rep, true, Some(edits), noop_total)?,
        }
    } else {
        run_tui(cli, pat, rep, false, Some(edits), noop_total)?;
    }
    Ok(())
}

// Wording for when a search finds real regex matches but every computed
// replacement is byte-identical to the original text (e.g. `sey fn fn`, or
// an ambiguous unbraced `$1_$1` capture ref) — distinct from finding
// nothing at all, so it doesn't look like the pattern silently failed.
fn no_change_message(noop_total: usize) -> String {
    if noop_total == 0 {
        "No matches.".to_string()
    } else {
        let word = if noop_total == 1 { "match" } else { "matches" };
        format!("{noop_total} {word} not replaced.")
    }
}

fn noop_suffix(noop_total: usize) -> String {
    if noop_total == 0 {
        String::new()
    } else {
        let word = if noop_total == 1 { "match" } else { "matches" };
        format!(" ({noop_total} {word} not replaced)")
    }
}

fn estimate_output_lines(edits: &[FileEdit], cli: &Cli) -> usize {
    edits.iter().map(|edit| {
        let hunks = build_hunks(&edit.changes, cli.context, edit.lines.len());
        let context_lines: usize = hunks.iter().map(|h| h.end - h.start + 1).sum();
        let extra_per_change = if cli.preview == Preview::Diff { edit.changes.len() } else { 0 };
        1 + context_lines + extra_per_change + hunks.len().saturating_sub(1)
    }).sum()
}

// ── Unified TUI (view mode + edit mode) ──────────────────────────────────────
//
// Two-pane split: top pane = compact match list (↑/↓ to navigate),
// bottom pane = full context for the selected match (j/k/ctrl+n/p to scroll).
// Edit mode adds two input lines at top; view mode hides them.

fn run_tui(
    cli: &Cli,
    init_pat: &str,
    init_rep: &str,
    start_in_edit: bool,
    preloaded: Option<Vec<FileEdit>>,
    initial_noop: usize,
) -> Result<()> {
    let mut pat = InputField::new(init_pat);
    let mut rep = InputField::new(init_rep);
    let mut active_field: usize = 0;
    let mut in_edit = start_in_edit;
    let mut snap_pat = pat.text.clone();
    let mut snap_rep = rep.text.clone();
    let mut noop_total: usize = initial_noop;

    let has_preloaded = preloaded.is_some();
    let (mut all_edits, mut match_list, mut total_matches, mut search_done) =
        match preloaded {
            Some(edits) => {
                let mut ml = Vec::new();
                let mut tm = 0usize;
                for (ei, edit) in edits.iter().enumerate() {
                    tm += edit.total_matches();
                    for ci in 0..edit.changes.len() {
                        ml.push(MatchEntry { edit_idx: ei, change_idx: ci });
                    }
                }
                (edits, ml, tm, true)
            }
            None => (Vec::new(), Vec::new(), 0, true),
        };
    let mut search_rx: Option<mpsc::Receiver<FileEdit>> = None;
    let mut last_change: Option<std::time::Instant> = if init_pat.is_empty() || has_preloaded {
        None
    } else {
        Some(std::time::Instant::now() - Duration::from_millis(300))
    };
    let mut regex_error: Option<String> = None;

    // Split-pane state
    let mut selected: usize = 0;
    let mut top_scroll: usize = 0;
    let mut bottom_lines: Vec<Line<'static>> = Vec::new();
    let mut last_selected: Option<usize> = None;
    let mut bottom_scroll: usize = 0;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut applied: HashSet<usize> = HashSet::new(); // indices into match_list
    let mut abort = false;  // ctrl+c: write nothing
    let mut write_all = false; // shift+enter: apply everything + write

    'tui: loop {
        // ── drain search results ─────────────────────────────────────────
        if let Some(ref mut r) = search_rx {
            loop {
                match r.try_recv() {
                    Ok(edit) => {
                        total_matches += edit.total_matches();
                        noop_total += edit.noop_count;
                        if !edit.changes.is_empty() {
                            let edit_idx = all_edits.len();
                            for change_idx in 0..edit.changes.len() {
                                match_list.push(MatchEntry { edit_idx, change_idx });
                            }
                            all_edits.push(edit);
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        search_done = true;
                        search_rx = None;
                        break;
                    }
                }
            }
        }

        // ── debounced search trigger ─────────────────────────────────────
        if let Some(t) = last_change {
            if t.elapsed() >= Duration::from_millis(200) {
                last_change = None;
                search_rx = None;
                match_list.clear();
                all_edits.clear();
                applied.clear();
                total_matches = 0;
                noop_total = 0;
                selected = 0;
                top_scroll = 0;
                bottom_scroll = 0;
                bottom_lines.clear();
                last_selected = None;
                regex_error = None;
                if !pat.text.is_empty() {
                    match build_regex(cli, &pat.text) {
                        Ok(re) => {
                            search_rx = Some(spawn_search(cli, re, rep.text.clone()));
                            search_done = false;
                        }
                        Err(e) => {
                            regex_error = Some(e.to_string());
                            search_done = true;
                        }
                    }
                } else {
                    search_done = true;
                }
            }
        }

        // ── viewport sizes ───────────────────────────────────────────────
        let total_h = terminal.size()?.height as usize;
        let input_rows: usize = if in_edit { 2 } else { 0 };
        let overhead = input_rows + 2; // status bar (1 border line + 1 text line)
        let available = total_h.saturating_sub(overhead);
        let top_h = (available / 3).max(3).min(available.saturating_sub(3).max(3));
        let bottom_h = available.saturating_sub(top_h).saturating_sub(1); // -1 for border line

        // ── clamp/auto-scroll top pane ───────────────────────────────────
        if !match_list.is_empty() && selected >= match_list.len() {
            selected = match_list.len() - 1;
        }
        if selected < top_scroll {
            top_scroll = selected;
        }
        if top_h > 0 && !match_list.is_empty() && selected >= top_scroll + top_h {
            top_scroll = selected + 1 - top_h;
        }

        // ── clamp bottom scroll ──────────────────────────────────────────
        bottom_scroll = bottom_scroll.min(bottom_lines.len().saturating_sub(bottom_h));

        // ── recompute bottom pane when selection changes or data arrives ──
        let sel_changed = last_selected != Some(selected);
        let was_empty = bottom_lines.is_empty();
        let needs_recompute = sel_changed
            || (was_empty
                && match_list
                    .get(selected)
                    .and_then(|e| all_edits.get(e.edit_idx))
                    .is_some());
        let eff_preview = if rep.text.is_empty() { Preview::Old } else { cli.preview.clone() };
        if needs_recompute {
            last_selected = Some(selected);
            if let Some(entry) = match_list.get(selected) {
                if let Some(edit) = all_edits.get(entry.edit_idx) {
                    bottom_lines = render_match_context(edit, entry.change_idx, &eff_preview);
                    if sel_changed || was_empty {
                        // Scroll to put the match line near the top (index 0 = path header)
                        let match_row = edit.changes[entry.change_idx].line_idx + 1;
                        bottom_scroll = match_row.saturating_sub(2);
                    }
                } else {
                    bottom_lines.clear();
                    bottom_scroll = 0;
                }
            } else {
                bottom_lines.clear();
                bottom_scroll = 0;
            }
        }

        // ── build top pane lines ─────────────────────────────────────────
        let top_lines: Vec<Line<'static>> = if match_list.is_empty() {
            let msg = if !search_done {
                " Searching\u{2026}".to_string()
            } else if pat.text.is_empty() {
                " Type a pattern to search".to_string()
            } else {
                format!(" {}", no_change_message(noop_total))
            };
            vec![Line::from(Span::styled(
                msg,
                Style::default().add_modifier(Modifier::DIM),
            ))]
        } else {
            let end = match_list.len().min(top_scroll + top_h);
            match_list[top_scroll..end]
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    let mi = top_scroll + i;
                    let is_sel = mi == selected;
                    let is_applied = applied.contains(&mi);
                    let edit = &all_edits[entry.edit_idx];
                    render_compact_entry(
                        &edit.path,
                        &edit.changes[entry.change_idx],
                        &eff_preview,
                        is_sel,
                        is_applied,
                    )
                })
                .collect()
        };

        // ── bottom visible slice ─────────────────────────────────────────
        let b_end = bottom_lines.len().min(bottom_scroll + bottom_h);
        let bottom_visible: Vec<Line<'static>> = bottom_lines[bottom_scroll..b_end].to_vec();

        // ── status bar ───────────────────────────────────────────────────
        let status_text = if let Some(ref e) = regex_error {
            format!(" Error: {e}")
        } else if in_edit {
            " \u{21b5} replace mode  \u{00b7}  esc revert  \u{00b7}  tab/shift+tab switch field  \u{00b7}  q quit"
                .to_string()
        } else if !search_done {
            format!(" Searching\u{2026} {total_matches} matches{}  \u{00b7}  \u{21b5} replace  \u{00b7}  shift+\u{21b5} replace all  \u{00b7}  e edit  \u{00b7}  q quit", noop_suffix(noop_total))
        } else if total_matches > 0 {
            let applied_info = if !applied.is_empty() {
                format!("  \u{00b7}  {}/{} replaced", applied.len(), match_list.len())
            } else {
                String::new()
            };
            let pos = format!(" [{}/{}]", selected + 1, match_list.len());
            format!(
                " {total_matches} matches in {} files{}{pos}{applied_info}  \u{00b7}  \u{21b5} replace  \u{00b7}  shift+\u{21b5} replace all  \u{00b7}  e edit  \u{00b7}  q quit",
                all_edits.len(), noop_suffix(noop_total)
            )
        } else if pat.text.is_empty() {
            " Type a pattern  \u{00b7}  esc quit".to_string()
        } else {
            format!(" {}  \u{00b7}  e edit  \u{00b7}  q quit", no_change_message(noop_total))
        };
        let status_style = if regex_error.is_some() {
            Style::default().fg(RColor::Red)
        } else {
            Style::default()
        };

        let pat_line = render_input_line("Pattern:     ", &pat, active_field == 0 && in_edit);
        let rep_line = render_input_line("Replacement: ", &rep, active_field == 1 && in_edit);
        let sep_style = Style::default().fg(RColor::DarkGray);

        terminal.draw(|f| {
            let area = f.area();
            let bottom_widget = Paragraph::new(bottom_visible).block(
                ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::TOP)
                    .border_style(sep_style),
            );
            // Status bar: thin separator line + text row (Length(2) total)
            let status_widget = Paragraph::new(status_text).style(status_style).block(
                ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::TOP)
                    .border_style(sep_style),
            );
            if in_edit {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(2),
                        Constraint::Length(top_h as u16),
                        Constraint::Min(0),
                        Constraint::Length(2),
                    ])
                    .split(area);
                f.render_widget(Paragraph::new(vec![pat_line, rep_line]), chunks[0]);
                f.render_widget(Paragraph::new(top_lines), chunks[1]);
                f.render_widget(bottom_widget, chunks[2]);
                f.render_widget(status_widget, chunks[3]);
            } else {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(top_h as u16),
                        Constraint::Min(0),
                        Constraint::Length(2),
                    ])
                    .split(area);
                f.render_widget(Paragraph::new(top_lines), chunks[0]);
                f.render_widget(bottom_widget, chunks[1]);
                f.render_widget(status_widget, chunks[2]);
            }
        })?;

        if !poll(Duration::from_millis(50))? {
            continue;
        }

        let ev = read_event()?;

        if in_edit {
            // ── edit mode keys ───────────────────────────────────────────
            match ev {
                Event::Key(KeyEvent { code: KeyCode::Char('q'), modifiers, .. })
                    if !modifiers.contains(KeyModifiers::CONTROL) => break 'tui,
                Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    abort = true;
                    break 'tui;
                }
                Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                    in_edit = false;
                }
                Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                    let changed = pat.text != snap_pat || rep.text != snap_rep;
                    pat = InputField::new(&snap_pat);
                    rep = InputField::new(&snap_rep);
                    if changed {
                        last_change =
                            Some(std::time::Instant::now() - Duration::from_millis(300));
                    }
                    in_edit = false;
                }
                Event::Key(KeyEvent { code: KeyCode::Tab, .. })
                | Event::Key(KeyEvent { code: KeyCode::BackTab, .. }) => {
                    active_field = 1 - active_field;
                }
                // Navigate top pane
                Event::Key(KeyEvent { code: KeyCode::Up, .. }) => {
                    if selected > 0 {
                        selected -= 1;
                        last_selected = None;
                    }
                }
                Event::Key(KeyEvent { code: KeyCode::Down, .. }) => {
                    if selected + 1 < match_list.len() {
                        selected += 1;
                        last_selected = None;
                    }
                }
                // Scroll bottom pane
                Event::Key(KeyEvent { code: KeyCode::Char('n'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    bottom_scroll = bottom_scroll.saturating_add(1);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('p'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    bottom_scroll = bottom_scroll.saturating_sub(1);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('v'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    bottom_scroll = bottom_scroll.saturating_add(bottom_h);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('v'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    bottom_scroll = bottom_scroll.saturating_sub(bottom_h);
                }
                other => {
                    let field = if active_field == 0 { &mut pat } else { &mut rep };
                    if handle_input_key(field, other) {
                        last_change = Some(std::time::Instant::now());
                    }
                }
            }
        } else {
            // ── replace mode keys ────────────────────────────────────────
            match ev {
                // Quit: write applied changes
                Event::Key(KeyEvent { code: KeyCode::Char('q'), .. })
                | Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => break 'tui,
                // Abort: write nothing
                Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    abort = true;
                    break 'tui;
                }
                // Replace current match and advance
                Event::Key(KeyEvent { code: KeyCode::Enter, modifiers, .. })
                    if modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    write_all = true;
                    break 'tui;
                }
                Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                    if !match_list.is_empty() {
                        applied.insert(selected);
                        if selected + 1 < match_list.len() {
                            selected += 1;
                            last_selected = None;
                        }
                    }
                }
                // Edit pattern/replacement
                Event::Key(KeyEvent { code: KeyCode::Char('e'), .. }) => {
                    snap_pat = pat.text.clone();
                    snap_rep = rep.text.clone();
                    in_edit = true;
                }
                // Navigate top pane
                Event::Key(KeyEvent { code: KeyCode::Up, .. }) => {
                    if selected > 0 {
                        selected -= 1;
                        last_selected = None;
                    }
                }
                Event::Key(KeyEvent { code: KeyCode::Down, .. }) => {
                    if selected + 1 < match_list.len() {
                        selected += 1;
                        last_selected = None;
                    }
                }
                // Jump to next/prev file
                Event::Key(KeyEvent { code: KeyCode::Char('}'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    if let Some(cur) = match_list.get(selected) {
                        let cur_edit = cur.edit_idx;
                        if let Some(offset) =
                            match_list[selected..].iter().position(|e| e.edit_idx > cur_edit)
                        {
                            selected = selected + offset;
                            last_selected = None;
                        }
                    }
                }
                Event::Key(KeyEvent { code: KeyCode::Char('{'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    if let Some(cur) = match_list.get(selected) {
                        let cur_edit = cur.edit_idx;
                        let prev_edit = match_list[..selected]
                            .iter()
                            .rev()
                            .find(|e| e.edit_idx < cur_edit)
                            .map(|e| e.edit_idx);
                        if let Some(prev) = prev_edit {
                            if let Some(pos) =
                                match_list.iter().position(|e| e.edit_idx == prev)
                            {
                                selected = pos;
                                last_selected = None;
                            }
                        }
                    }
                }
                // Scroll bottom pane
                Event::Key(KeyEvent { code: KeyCode::Char('j'), .. }) => {
                    bottom_scroll = bottom_scroll.saturating_add(1);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('k'), .. }) => {
                    bottom_scroll = bottom_scroll.saturating_sub(1);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('n'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    bottom_scroll = bottom_scroll.saturating_add(1);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('p'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    bottom_scroll = bottom_scroll.saturating_sub(1);
                }
                Event::Key(KeyEvent { code: KeyCode::Char(' '), .. })
                | Event::Key(KeyEvent { code: KeyCode::PageDown, .. })
                | Event::Key(KeyEvent { code: KeyCode::Char('f'), .. }) => {
                    bottom_scroll = bottom_scroll.saturating_add(bottom_h);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('v'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    bottom_scroll = bottom_scroll.saturating_add(bottom_h);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('b'), .. })
                | Event::Key(KeyEvent { code: KeyCode::PageUp, .. }) => {
                    bottom_scroll = bottom_scroll.saturating_sub(bottom_h);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('v'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    bottom_scroll = bottom_scroll.saturating_sub(bottom_h);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('g'), .. }) => {
                    bottom_scroll = 0;
                }
                Event::Key(KeyEvent { code: KeyCode::Char('G'), .. }) => {
                    bottom_scroll = bottom_lines.len().saturating_sub(bottom_h);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('<'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    selected = 0;
                    last_selected = None;
                }
                Event::Key(KeyEvent { code: KeyCode::Char('>'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    if !match_list.is_empty() {
                        selected = match_list.len() - 1;
                        last_selected = None;
                    }
                }
                _ => {}
            }
        }
    }

    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.backend_mut().execute(Show)?;
    disable_raw_mode()?;
    drop(terminal);

    if abort {
        eprintln!("Aborted.");
        return Ok(());
    }

    if write_all {
        // Drain any remaining search results
        if let Some(r) = search_rx {
            for edit in r {
                let edit_idx = all_edits.len();
                for change_idx in 0..edit.changes.len() {
                    let mi = match_list.len();
                    match_list.push(MatchEntry { edit_idx, change_idx });
                    applied.insert(mi);
                }
                all_edits.push(edit);
            }
        }
        // Mark everything already received as applied
        for i in 0..match_list.len() {
            applied.insert(i);
        }
    }

    if applied.is_empty() {
        eprintln!("No replacements made.");
        return Ok(());
    }

    // Group applied entries by file
    let mut per_file: HashMap<usize, HashSet<usize>> = HashMap::new();
    for &mi in &applied {
        let entry = &match_list[mi];
        per_file.entry(entry.edit_idx).or_default().insert(entry.change_idx);
    }
    let file_count = per_file.len();
    let replace_count = applied.len();
    for (ei, change_indices) in &per_file {
        let edit = &all_edits[*ei];
        let new_text = compute_partial_replacement(edit, change_indices);
        fs::write(&edit.path, new_text)
            .with_context(|| format!("writing {}", edit.path.display()))?;
    }
    eprintln!("Replaced {replace_count} matches across {file_count} files.");

    Ok(())
}

fn render_compact_entry(
    path: &std::path::Path,
    change: &Change,
    preview: &Preview,
    selected: bool,
    applied: bool,
) -> Line<'static> {
    let hl_style = if *preview == Preview::New {
        Style::default().fg(RColor::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(RColor::Yellow).add_modifier(Modifier::BOLD)
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let segs = if *preview == Preview::New { &change.new_segments } else { &change.orig_segments };
    let prefix = if applied {
        Span::styled("\u{2713} ", Style::default().fg(RColor::Green))
    } else {
        Span::raw("  ")
    };
    let mut spans = vec![
        prefix,
        Span::styled(name, Style::default().fg(RColor::Magenta).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(":{}", change.line_idx + 1),
            Style::default().fg(RColor::Cyan),
        ),
        Span::raw("  "),
    ];
    for (text, is_hl) in segs {
        spans.push(if *is_hl {
            Span::styled(text.clone(), hl_style)
        } else {
            Span::raw(text.clone())
        });
    }
    let line = Line::from(spans);
    if selected {
        line.style(Style::default().bg(RColor::Blue))
    } else {
        line
    }
}

fn render_match_context(
    edit: &FileEdit,
    change_idx: usize,
    preview: &Preview,
) -> Vec<Line<'static>> {
    let path_str = edit.path.display().to_string();
    let mut lines = vec![Line::from(Span::styled(
        path_str,
        Style::default().fg(RColor::Magenta).add_modifier(Modifier::BOLD),
    ))];

    let change = &edit.changes[change_idx];
    let start = 0;
    let end = edit.lines.len().saturating_sub(1);
    let change_map: HashMap<usize, &Change> =
        std::iter::once((change.line_idx, change)).collect();

    for line_idx in start..=end {
        let (body, _) = split_nl(&edit.lines[line_idx]);
        if let Some(c) = change_map.get(&line_idx) {
            match preview {
                Preview::Old => {
                    let mut spans = vec![Span::styled(
                        format!("{:>6}  ", line_idx + 1),
                        Style::default().fg(RColor::Cyan),
                    )];
                    for (text, is_m) in &c.orig_segments {
                        spans.push(if *is_m {
                            Span::styled(
                                text.clone(),
                                Style::default()
                                    .fg(RColor::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            )
                        } else {
                            Span::raw(text.clone())
                        });
                    }
                    lines.push(Line::from(spans));
                }
                Preview::New => {
                    let mut spans = vec![Span::styled(
                        format!("{:>6}  ", line_idx + 1),
                        Style::default().fg(RColor::Cyan),
                    )];
                    for (text, is_hl) in &c.new_segments {
                        spans.push(if *is_hl {
                            Span::styled(
                                text.clone(),
                                Style::default()
                                    .fg(RColor::Green)
                                    .add_modifier(Modifier::BOLD),
                            )
                        } else {
                            Span::raw(text.clone())
                        });
                    }
                    lines.push(Line::from(spans));
                }
                Preview::Diff => {
                    let mut old_spans = vec![
                        Span::styled(
                            "-",
                            Style::default().fg(RColor::Red).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{:>5}  ", line_idx + 1),
                            Style::default().fg(RColor::Cyan),
                        ),
                    ];
                    for (text, is_m) in &c.orig_segments {
                        old_spans.push(if *is_m {
                            Span::styled(
                                text.clone(),
                                Style::default()
                                    .fg(RColor::Red)
                                    .add_modifier(Modifier::BOLD),
                            )
                        } else {
                            Span::raw(text.clone())
                        });
                    }
                    lines.push(Line::from(old_spans));

                    let mut new_spans = vec![
                        Span::styled(
                            "+",
                            Style::default().fg(RColor::Green).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{:>5}  ", line_idx + 1),
                            Style::default().fg(RColor::Cyan),
                        ),
                    ];
                    for (text, is_hl) in &c.new_segments {
                        new_spans.push(if *is_hl {
                            Span::styled(
                                text.clone(),
                                Style::default()
                                    .fg(RColor::Green)
                                    .add_modifier(Modifier::BOLD),
                            )
                        } else {
                            Span::raw(text.clone())
                        });
                    }
                    lines.push(Line::from(new_spans));
                }
            }
        } else if *preview == Preview::Diff {
            lines.push(Line::from(Span::styled(
                format!(" {:>5}  {}", line_idx + 1, body),
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:>6}  ", line_idx + 1),
                    Style::default().fg(RColor::Cyan),
                ),
                Span::styled(body.to_string(), Style::default().add_modifier(Modifier::DIM)),
            ]));
        }
    }

    lines
}

fn compute_partial_replacement(edit: &FileEdit, applied_changes: &HashSet<usize>) -> String {
    let applied_map: HashMap<usize, &Change> = edit
        .changes
        .iter()
        .enumerate()
        .filter(|(ci, _)| applied_changes.contains(ci))
        .map(|(_, c)| (c.line_idx, c))
        .collect();
    let mut text = String::with_capacity(edit.new_text.len());
    for (idx, raw) in edit.lines.iter().enumerate() {
        if let Some(change) = applied_map.get(&idx) {
            let (_, nl) = split_nl(raw);
            let new_body: String = change.new_segments.iter().map(|(t, _)| t.as_str()).collect();
            text.push_str(&new_body);
            text.push_str(nl);
        } else {
            text.push_str(raw);
        }
    }
    text
}

// ── Display ───────────────────────────────────────────────────────────────────

fn print_file_vimgrep<W: WriteColor>(edit: &FileEdit, out: &mut W) -> io::Result<()> {
    for change in &edit.changes {
        let (body, _) = split_nl(&edit.lines[change.line_idx]);
        let line = change.line_idx + 1;
        let mut col = 1usize;
        for (text, is_match) in &change.orig_segments {
            if *is_match {
                writeln!(out, "{}:{}:{}:{}", edit.path.display(), line, col, body)?;
            }
            col += text.len();
        }
    }
    Ok(())
}

fn print_file_compact<W: WriteColor>(edit: &FileEdit, preview: &Preview, out: &mut W) -> io::Result<()> {
    let path_spec = spec(Color::Magenta, true);
    let num = spec(Color::Cyan, false);
    let yellow_bold = spec(Color::Yellow, true);
    let green_bold = spec(Color::Green, true);

    out.set_color(&path_spec)?;
    writeln!(out, "{}", edit.path.display())?;
    out.reset()?;

    for change in &edit.changes {
        out.set_color(&num)?;
        write!(out, "{:>6}  ", change.line_idx + 1)?;
        out.reset()?;
        let (segs, highlight) = if *preview == Preview::New {
            (&change.new_segments, &green_bold)
        } else {
            (&change.orig_segments, &yellow_bold)
        };
        write_segments(out, segs, highlight)?;
        writeln!(out)?;
    }
    writeln!(out)?;
    Ok(())
}

fn print_file_preview<W: WriteColor>(
    edit: &FileEdit,
    context: usize,
    preview: &Preview,
    out: &mut W,
) -> io::Result<()> {
    let path_spec = spec(Color::Magenta, true);
    let dim = dimmed();

    out.set_color(&path_spec)?;
    writeln!(out, "{}", edit.path.display())?;
    out.reset()?;

    let change_map: HashMap<usize, &Change> =
        edit.changes.iter().map(|c| (c.line_idx, c)).collect();
    let hunks = build_hunks(&edit.changes, context, edit.lines.len());

    for (hi, hunk) in hunks.iter().enumerate() {
        if hi > 0 {
            out.set_color(&dim)?;
            writeln!(out, "        ⋮")?;
            out.reset()?;
        }
        print_hunk(out, &edit.lines, &change_map, hunk.start, hunk.end, preview)?;
    }
    writeln!(out)?;
    Ok(())
}

fn print_hunk<W: WriteColor>(
    out: &mut W,
    lines: &[String],
    change_map: &HashMap<usize, &Change>,
    start: usize,
    end: usize,
    preview: &Preview,
) -> io::Result<()> {
    let num = spec(Color::Cyan, false);
    let dim = dimmed();
    let yellow_bold = spec(Color::Yellow, true);
    let green_bold = spec(Color::Green, true);
    let red_bold = spec(Color::Red, true);

    for line_idx in start..=end {
        let (body, _) = split_nl(&lines[line_idx]);

        if let Some(change) = change_map.get(&line_idx) {
            match preview {
                Preview::Old => {
                    out.set_color(&num)?;
                    write!(out, "{:>6}  ", line_idx + 1)?;
                    out.reset()?;
                    write_segments(out, &change.orig_segments, &yellow_bold)?;
                    writeln!(out)?;
                }
                Preview::New => {
                    out.set_color(&num)?;
                    write!(out, "{:>6}  ", line_idx + 1)?;
                    out.reset()?;
                    write_segments(out, &change.new_segments, &green_bold)?;
                    writeln!(out)?;
                }
                Preview::Diff => {
                    out.set_color(&red_bold)?;
                    write!(out, "-")?;
                    out.set_color(&num)?;
                    write!(out, "{:>5}  ", line_idx + 1)?;
                    out.reset()?;
                    write_segments(out, &change.orig_segments, &red_bold)?;
                    writeln!(out)?;

                    out.set_color(&green_bold)?;
                    write!(out, "+")?;
                    out.set_color(&num)?;
                    write!(out, "{:>5}  ", line_idx + 1)?;
                    out.reset()?;
                    write_segments(out, &change.new_segments, &green_bold)?;
                    writeln!(out)?;
                }
            }
        } else if *preview == Preview::Diff {
            out.set_color(&dim)?;
            write!(out, " {:>5}  {}", line_idx + 1, body)?;
            out.reset()?;
            writeln!(out)?;
        } else {
            out.set_color(&num)?;
            write!(out, "{:>6}  ", line_idx + 1)?;
            out.reset()?;
            out.set_color(&dim)?;
            write!(out, "{}", body)?;
            out.reset()?;
            writeln!(out)?;
        }
    }
    Ok(())
}

fn write_segments<W: WriteColor>(
    out: &mut W,
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

// ── File processing ───────────────────────────────────────────────────────────

fn process_file(path: PathBuf, re: &Regex, repl: &str, fixed: bool) -> Option<FileEdit> {
    let text = fs::read_to_string(&path).ok()?;
    let lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();

    let mut changes = Vec::new();
    let mut noop_count = 0usize;
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
            noop_count += match_count;
        }
        new_text.push_str(raw);
    }

    if changes.is_empty() && noop_count == 0 {
        None
    } else {
        Some(FileEdit { path, lines, changes, new_text, noop_count })
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

// ── Hunk grouping ─────────────────────────────────────────────────────────────

struct Hunk {
    start: usize,
    end: usize,
}

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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn spec(fg: Color, bold: bool) -> ColorSpec {
    let mut s = ColorSpec::new();
    s.set_fg(Some(fg)).set_bold(bold);
    s
}

fn dimmed() -> ColorSpec {
    let mut s = ColorSpec::new();
    s.set_dimmed(true);
    s
}


enum PromptChoice { ReplaceAll, Edit, Quit }

fn prompt_no_pager(total: usize, file_count: usize, noop_total: usize) -> io::Result<PromptChoice> {
    eprint!("\n{total} matches in {file_count} files{}", noop_suffix(noop_total));
    eprint!("\nshift+\u{21b5} replace all  \u{00b7}  e edit  \u{00b7}  q quit  ");
    io::stderr().flush()?;
    enable_raw_mode()?;
    let choice = loop {
        match read_event()? {
            Event::Key(KeyEvent { code: KeyCode::Enter, modifiers, .. })
                if modifiers.contains(KeyModifiers::SHIFT) => break PromptChoice::ReplaceAll,
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                eprint!("\x07"); // bell — plain Enter not accepted here
                io::stderr().flush()?;
            }
            Event::Key(KeyEvent { code: KeyCode::Char('e'), .. }) => break PromptChoice::Edit,
            Event::Key(KeyEvent { code: KeyCode::Char('q'), .. })
            | Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => break PromptChoice::Quit,
            Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers, .. })
                if modifiers.contains(KeyModifiers::CONTROL) => break PromptChoice::Quit,
            _ => continue,
        }
    };
    disable_raw_mode()?;
    eprintln!();
    Ok(choice)
}

// ── Live edit TUI ─────────────────────────────────────────────────────────────

struct InputField {
    text: String,
    cursor: usize, // byte offset
}

impl InputField {
    fn new(s: &str) -> Self {
        Self { text: s.to_owned(), cursor: s.len() }
    }

    fn insert(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.prev_boundary();
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    fn delete_forward(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.next_boundary();
            self.text.drain(self.cursor..next);
        }
    }

    fn move_left(&mut self) { if self.cursor > 0 { self.cursor = self.prev_boundary(); } }
    fn move_right(&mut self) { if self.cursor < self.text.len() { self.cursor = self.next_boundary(); } }
    fn move_to_start(&mut self) { self.cursor = 0; }
    fn move_to_end(&mut self) { self.cursor = self.text.len(); }
    fn kill_to_end(&mut self) { self.text.truncate(self.cursor); }

    fn kill_to_start(&mut self) {
        self.text.drain(..self.cursor);
        self.cursor = 0;
    }

    fn kill_word_backward(&mut self) {
        let before = &self.text[..self.cursor];
        let end = before.trim_end_matches(|c: char| !c.is_whitespace()).len();
        let start = before[..end].trim_end_matches(|c: char| c.is_whitespace()).len();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.cursor + self.text[self.cursor..].chars().next().map_or(0, |c| c.len_utf8())
    }
}

fn handle_input_key(field: &mut InputField, ev: Event) -> bool {
    match ev {
        Event::Key(KeyEvent { code: KeyCode::Char(ch), modifiers, .. }) => {
            if modifiers.contains(KeyModifiers::CONTROL) {
                match ch {
                    'a' => { field.move_to_start(); false }
                    'e' => { field.move_to_end(); false }
                    'f' => { field.move_right(); false }
                    'b' => { field.move_left(); false }
                    'h' => { field.backspace(); true }
                    'd' => { field.delete_forward(); true }
                    'k' => { field.kill_to_end(); true }
                    'u' => { field.kill_to_start(); true }
                    'w' => { field.kill_word_backward(); true }
                    _ => false,
                }
            } else if modifiers.contains(KeyModifiers::ALT) {
                match ch {
                    'd' => { field.kill_word_backward(); true } // alt+d = kill word forward (approx)
                    _ => false,
                }
            } else {
                field.insert(ch);
                true
            }
        }
        Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => { field.backspace(); true }
        Event::Key(KeyEvent { code: KeyCode::Delete, .. }) => { field.delete_forward(); true }
        Event::Key(KeyEvent { code: KeyCode::Left, .. }) => { field.move_left(); false }
        Event::Key(KeyEvent { code: KeyCode::Right, .. }) => { field.move_right(); false }
        Event::Key(KeyEvent { code: KeyCode::Home, .. }) => { field.move_to_start(); false }
        Event::Key(KeyEvent { code: KeyCode::End, .. }) => { field.move_to_end(); false }
        _ => false,
    }
}

fn render_input_line(label: &str, field: &InputField, active: bool) -> Line<'static> {
    let label_style = if active {
        Style::default().fg(RColor::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(RColor::DarkGray)
    };
    let mut spans = vec![Span::styled(label.to_owned(), label_style)];
    if active {
        let before = field.text[..field.cursor].to_owned();
        let (cursor_ch, after) = if field.cursor < field.text.len() {
            let end = field.cursor
                + field.text[field.cursor..].chars().next().map_or(0, |c| c.len_utf8());
            (field.text[field.cursor..end].to_owned(), field.text[end..].to_owned())
        } else {
            (" ".to_owned(), String::new())
        };
        spans.push(Span::raw(before));
        spans.push(Span::styled(
            cursor_ch,
            Style::default().bg(RColor::White).fg(RColor::Black),
        ));
        if !after.is_empty() {
            spans.push(Span::raw(after));
        }
    } else {
        spans.push(Span::styled(field.text.clone(), Style::default().add_modifier(Modifier::DIM)));
    }
    Line::from(spans)
}
