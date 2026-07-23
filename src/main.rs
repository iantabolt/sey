use std::collections::HashMap;
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

#[derive(Debug, Clone, Copy)]
enum Action {
    Yes,
    No,
    All,
    Quit,
}

/// Find/replace across files, ripgrep-powered.
#[derive(Parser, Debug)]
#[command(name = "sey", version, about)]
struct Cli {
    /// Search pattern (regex by default; use -F for a literal string)
    #[arg(required_unless_present = "edit")]
    pattern: Option<String>,
    /// Replacement (supports $1 / ${name} capture references)
    #[arg(required_unless_present = "edit")]
    replacement: Option<String>,
    /// Files or directories to search (default: current directory)
    paths: Vec<PathBuf>,
    /// Open live pattern/replacement editor (pattern and replacement become optional)
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
    /// Apply without asking for confirmation (batch mode only)
    #[arg(short = 'y', long)]
    yes: bool,
    /// Review and apply replacements one at a time
    #[arg(short = 'I', long)]
    interactive: bool,
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
    /// One line per match instead of the full diff+context view
    #[arg(short = 'c', long)]
    compact: bool,
    /// Do not pipe output through a pager
    #[arg(long)]
    no_pager: bool,
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
}

impl FileEdit {
    fn total_matches(&self) -> usize {
        self.changes.iter().map(|c| c.match_count).sum()
    }
}

// Controls what happens after processing a file's changes in interactive mode.
enum FileControl {
    Continue,
    ApplyAll { from_ci: usize },
    Quit,
}

enum TuiAction {
    Apply,
    Interactive,
    Edit,
    Abort,
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();

    if cli.edit {
        // In edit mode, any positionals misparse'd into pattern/replacement are actually paths.
        let mut extra: Vec<PathBuf> = Vec::new();
        if let Some(p) = cli.pattern.take() { extra.push(PathBuf::from(p)); }
        if let Some(r) = cli.replacement.take() { extra.push(PathBuf::from(r)); }
        for p in extra { cli.paths.insert(0, p); }
        if cli.paths.is_empty() { cli.paths.push(PathBuf::from(".")); }
        return run_edit_tui(&cli, "", "");
    }

    if cli.paths.is_empty() {
        cli.paths.push(PathBuf::from("."));
    }

    let pattern = cli.pattern.as_deref().unwrap();
    let replacement = cli.replacement.as_deref().unwrap_or("");
    let re = build_regex(&cli, pattern)?;
    let rx = spawn_search(&cli, re, replacement.to_string());

    if cli.interactive {
        run_interactive(rx, &cli)
    } else {
        run_batch(rx, &cli)
    }
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

fn spawn_search(cli: &Cli, re: Regex, replacement: String) -> mpsc::Receiver<FileEdit> {
    let (tx, rx) = mpsc::channel();

    let paths = cli.paths.clone();
    let globs = cli.globs.clone();
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
    // vimgrep mode: file:line:col:content, one line per match, no replacement applied.
    // Triggered explicitly via --vimgrep or automatically when stdout is piped.
    if cli.vimgrep || !io::stdout().is_terminal() {
        let mut out = StandardStream::stdout(ColorChoice::Never);
        for edit in &rx {
            print_file_vimgrep(&edit, &mut out)?;
        }
        return Ok(());
    }

    // TUI pager unless --no-pager or -y (which implies non-interactive).
    if !cli.no_pager && !cli.yes {
        return run_pager_tui(rx, cli);
    }

    let mut out = StandardStream::stdout(ColorChoice::Auto);
    let edits = collect_and_display(rx, cli, &mut out)?;

    if edits.is_empty() {
        eprintln!("No matches.");
        return Ok(());
    }

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

fn collect_and_display<W: WriteColor>(
    rx: mpsc::Receiver<FileEdit>,
    cli: &Cli,
    out: &mut W,
) -> Result<Vec<FileEdit>> {
    let mut edits = Vec::new();
    for edit in &rx {
        if cli.compact {
            print_file_compact(&edit, &cli.preview, out)?;
        } else {
            print_file_preview(&edit, cli.context, &cli.preview, out)?;
        }
        edits.push(edit);
    }
    Ok(edits)
}

fn run_pager_tui(rx: mpsc::Receiver<FileEdit>, cli: &Cli) -> Result<()> {
    let first = match rx.recv() {
        Ok(e) => e,
        Err(_) => {
            eprintln!("No matches.");
            return Ok(());
        }
    };

    let mut total_matches = first.total_matches();
    let mut all_lines = render_file_to_lines(&first, cli.context, &cli.preview, cli.compact);
    let mut all_edits: Vec<FileEdit> = vec![first];
    let mut file_starts: Vec<usize> = vec![0];
    let mut search_done = false;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut scroll: usize = 0;
    let mut action = TuiAction::Abort;

    'tui: loop {
        // Drain any results that arrived since the last frame.
        loop {
            match rx.try_recv() {
                Ok(edit) => {
                    total_matches += edit.total_matches();
                    let new_lines =
                        render_file_to_lines(&edit, cli.context, &cli.preview, cli.compact);
                    file_starts.push(all_lines.len());
                    all_lines.extend(new_lines);
                    all_edits.push(edit);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    search_done = true;
                    break;
                }
            }
        }

        // Clamp scroll before rendering.
        {
            let h = terminal.size()?.height.saturating_sub(1) as usize;
            scroll = scroll.min(all_lines.len().saturating_sub(h));
        }

        let cur_scroll = scroll;
        let cur_total = total_matches;
        let cur_files = all_edits.len();
        terminal.draw(|f| {
            let area = f.area();
            let vh = area.height.saturating_sub(1) as usize;
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(1)])
                .split(area);

            let end = all_lines.len().min(cur_scroll + vh);
            let visible: Vec<Line<'static>> = all_lines[cur_scroll..end].to_vec();
            f.render_widget(Paragraph::new(visible), chunks[0]);

            let status = if search_done {
                format!(
                    " {} matches in {} files · y apply · e edit · i interactive · q quit",
                    cur_total, cur_files
                )
            } else {
                format!(" Searching… {} matches · y apply · e edit · i interactive · q quit", cur_total)
            };
            f.render_widget(
                Paragraph::new(status).style(Style::default().bg(RColor::DarkGray)),
                chunks[1],
            );
        })?;

        if poll(Duration::from_millis(50))? {
            let vh = terminal.size()?.height.saturating_sub(1) as usize;
            match read_event()? {
                Event::Key(KeyEvent { code: KeyCode::Char('q'), .. })
                | Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => break 'tui,
                // plain 'n' aborts; ctrl+n scrolls down (emacs)
                Event::Key(KeyEvent { code: KeyCode::Char('n'), modifiers, .. })
                    if !modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    break 'tui
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers,
                    ..
                }) if modifiers.contains(KeyModifiers::CONTROL) => break 'tui,
                Event::Key(KeyEvent { code: KeyCode::Char('y'), .. }) => {
                    action = TuiAction::Apply;
                    break 'tui;
                }
                Event::Key(KeyEvent { code: KeyCode::Char('i'), .. }) => {
                    action = TuiAction::Interactive;
                    break 'tui;
                }
                Event::Key(KeyEvent { code: KeyCode::Char('e'), .. }) => {
                    action = TuiAction::Edit;
                    break 'tui;
                }
                // ── scroll by line ────────────────────────────────────────
                Event::Key(KeyEvent { code: KeyCode::Char('j'), .. })
                | Event::Key(KeyEvent { code: KeyCode::Down, .. }) => {
                    scroll = scroll.saturating_add(1);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('n'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    scroll = scroll.saturating_add(1);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('k'), .. })
                | Event::Key(KeyEvent { code: KeyCode::Up, .. }) => {
                    scroll = scroll.saturating_sub(1);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('p'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    scroll = scroll.saturating_sub(1);
                }
                // ── scroll by page ────────────────────────────────────────
                Event::Key(KeyEvent { code: KeyCode::Char(' '), .. })
                | Event::Key(KeyEvent { code: KeyCode::PageDown, .. })
                | Event::Key(KeyEvent { code: KeyCode::Char('f'), .. }) => {
                    scroll = scroll.saturating_add(vh);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('v'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    scroll = scroll.saturating_add(vh);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('b'), .. })
                | Event::Key(KeyEvent { code: KeyCode::PageUp, .. }) => {
                    scroll = scroll.saturating_sub(vh);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('v'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    scroll = scroll.saturating_sub(vh);
                }
                // ── jump to top / bottom ──────────────────────────────────
                Event::Key(KeyEvent { code: KeyCode::Char('g'), .. }) => {
                    scroll = 0;
                }
                Event::Key(KeyEvent { code: KeyCode::Char('<'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    scroll = 0;
                }
                Event::Key(KeyEvent { code: KeyCode::Char('G'), .. }) => {
                    scroll = all_lines.len().saturating_sub(vh);
                }
                Event::Key(KeyEvent { code: KeyCode::Char('>'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    scroll = all_lines.len().saturating_sub(vh);
                }
                // ── next / prev file (meta+} / meta+{) ───────────────────
                Event::Key(KeyEvent { code: KeyCode::Char('}'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    if let Some(&s) = file_starts.iter().find(|&&s| s > scroll) {
                        scroll = s;
                    }
                }
                Event::Key(KeyEvent { code: KeyCode::Char('{'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::ALT) =>
                {
                    if let Some(&s) = file_starts.iter().rev().find(|&&s| s < scroll) {
                        scroll = s;
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

    match action {
        TuiAction::Apply => {
            for edit in rx {
                all_edits.push(edit);
            }
            let total: usize = all_edits.iter().map(|e| e.total_matches()).sum();
            for edit in &all_edits {
                fs::write(&edit.path, &edit.new_text)
                    .with_context(|| format!("writing {}", edit.path.display()))?;
            }
            eprintln!("Replaced {} matches across {} files.", total, all_edits.len());
        }
        TuiAction::Interactive => {
            let (tx2, rx2) = mpsc::channel();
            for edit in all_edits {
                tx2.send(edit).ok();
            }
            thread::spawn(move || {
                for edit in rx {
                    if tx2.send(edit).is_err() {
                        break;
                    }
                }
            });
            run_interactive(rx2, cli)?;
        }
        TuiAction::Edit => {
            drop(rx);
            let pattern = cli.pattern.as_deref().unwrap_or("");
            let replacement = cli.replacement.as_deref().unwrap_or("");
            run_edit_tui(cli, pattern, replacement)?;
        }
        TuiAction::Abort => {
            eprintln!("Aborted.");
        }
    }

    Ok(())
}

fn render_file_to_lines(
    edit: &FileEdit,
    context: usize,
    preview: &Preview,
    compact: bool,
) -> Vec<Line<'static>> {
    let path_str = edit.path.display().to_string();
    let mut lines = vec![Line::from(Span::styled(
        path_str,
        Style::default().fg(RColor::Magenta).add_modifier(Modifier::BOLD),
    ))];

    if compact {
        for change in &edit.changes {
            let num = Span::styled(
                format!("{:>6}  ", change.line_idx + 1),
                Style::default().fg(RColor::Cyan),
            );
            let (segs, hl) = if *preview == Preview::New {
                (&change.new_segments, Style::default().fg(RColor::Green).add_modifier(Modifier::BOLD))
            } else {
                (&change.orig_segments, Style::default().fg(RColor::Yellow).add_modifier(Modifier::BOLD))
            };
            let mut spans = vec![num];
            for (text, is_hl) in segs {
                spans.push(if *is_hl {
                    Span::styled(text.clone(), hl)
                } else {
                    Span::raw(text.clone())
                });
            }
            lines.push(Line::from(spans));
        }
    } else {
        let change_map: HashMap<usize, &Change> =
            edit.changes.iter().map(|c| (c.line_idx, c)).collect();
        let hunks = build_hunks(&edit.changes, context, edit.lines.len());

        for (hi, hunk) in hunks.iter().enumerate() {
            if hi > 0 {
                lines.push(Line::from(Span::styled(
                    "        \u{22ee}",
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            for line_idx in hunk.start..=hunk.end {
                let (body, _) = split_nl(&edit.lines[line_idx]);
                if let Some(change) = change_map.get(&line_idx) {
                    match preview {
                        Preview::Old => {
                            let mut spans = vec![Span::styled(
                                format!("{:>6}  ", line_idx + 1),
                                Style::default().fg(RColor::Cyan),
                            )];
                            for (text, is_m) in &change.orig_segments {
                                spans.push(if *is_m {
                                    Span::styled(text.clone(), Style::default().fg(RColor::Yellow).add_modifier(Modifier::BOLD))
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
                            for (text, is_hl) in &change.new_segments {
                                spans.push(if *is_hl {
                                    Span::styled(text.clone(), Style::default().fg(RColor::Green).add_modifier(Modifier::BOLD))
                                } else {
                                    Span::raw(text.clone())
                                });
                            }
                            lines.push(Line::from(spans));
                        }
                        Preview::Diff => {
                            let mut old_spans = vec![
                                Span::styled("-", Style::default().fg(RColor::Red).add_modifier(Modifier::BOLD)),
                                Span::styled(format!("{:>5}  ", line_idx + 1), Style::default().fg(RColor::Cyan)),
                            ];
                            for (text, is_m) in &change.orig_segments {
                                old_spans.push(if *is_m {
                                    Span::styled(text.clone(), Style::default().fg(RColor::Red).add_modifier(Modifier::BOLD))
                                } else {
                                    Span::raw(text.clone())
                                });
                            }
                            lines.push(Line::from(old_spans));

                            let mut new_spans = vec![
                                Span::styled("+", Style::default().fg(RColor::Green).add_modifier(Modifier::BOLD)),
                                Span::styled(format!("{:>5}  ", line_idx + 1), Style::default().fg(RColor::Cyan)),
                            ];
                            for (text, is_hl) in &change.new_segments {
                                new_spans.push(if *is_hl {
                                    Span::styled(text.clone(), Style::default().fg(RColor::Green).add_modifier(Modifier::BOLD))
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
                        Span::styled(format!("{:>6}  ", line_idx + 1), Style::default().fg(RColor::Cyan)),
                        Span::styled(body.to_string(), Style::default().add_modifier(Modifier::DIM)),
                    ]));
                }
            }
        }
    }

    lines.push(Line::raw(""));
    lines
}

// ── Interactive mode ──────────────────────────────────────────────────────────

fn run_interactive(rx: mpsc::Receiver<FileEdit>, cli: &Cli) -> Result<()> {
    let mut out = StandardStream::stderr(ColorChoice::Auto);
    let mut match_num = 0usize;
    let mut accepted_total = 0usize;

    loop {
        let edit = match rx.recv() {
            Ok(e) => e,
            Err(_) => break,
        };

        let mut current_lines = edit.lines.clone();
        let mut file_modified = false;
        let mut control = FileControl::Continue;

        for (ci, change) in edit.changes.iter().enumerate() {
            match_num += 1;

            print_interactive_match(
                &edit.path,
                change,
                match_num,
                ci,
                edit.changes.len(),
                &edit.lines,
                cli.context,
                &cli.preview,
                &mut out,
            )?;

            eprint!("  y yes   n no   a all   q quit");
            io::stderr().flush()?;

            let action = read_key()?;

            eprint!("\r\x1b[2K"); // clear the prompt line
            match action {
                Action::Yes => {
                    eprintln!("  \x1b[32m✓\x1b[0m Accepted");
                    apply_change(&mut current_lines, change);
                    file_modified = true;
                    accepted_total += change.match_count;
                }
                Action::No => {
                    eprintln!("  \x1b[2m✗ Skipped\x1b[0m");
                }
                Action::All => {
                    eprintln!("  \x1b[32m✓\x1b[0m Apply all");
                    control = FileControl::ApplyAll { from_ci: ci };
                    break;
                }
                Action::Quit => {
                    eprintln!("  \x1b[2mAborted\x1b[0m");
                    control = FileControl::Quit;
                    break;
                }
            }
        }

        match control {
            FileControl::Continue => {
                if file_modified {
                    write_lines(&edit.path, &current_lines)?;
                }
            }
            FileControl::ApplyAll { from_ci } => {
                for remaining in &edit.changes[from_ci..] {
                    apply_change(&mut current_lines, remaining);
                    accepted_total += remaining.match_count;
                }
                write_lines(&edit.path, &current_lines)?;

                let rest: Vec<FileEdit> = rx.into_iter().collect();

                if !rest.is_empty() {
                    let rest_total: usize = rest.iter().map(|e| e.total_matches()).sum();
                    eprintln!();
                    for f in &rest {
                        print_file_preview(f, cli.context, &cli.preview, &mut out)?;
                    }
                    eprintln!("\n{} remaining matches in {} more files", rest_total, rest.len());
                    if confirm()? {
                        for f in &rest {
                            fs::write(&f.path, &f.new_text)
                                .with_context(|| format!("writing {}", f.path.display()))?;
                        }
                        accepted_total += rest_total;
                    }
                }

                eprintln!("\nApplied {} replacements.", accepted_total);
                return Ok(());
            }
            FileControl::Quit => {
                if file_modified {
                    write_lines(&edit.path, &current_lines)?;
                }
                eprintln!("\nApplied {} replacements.", accepted_total);
                return Ok(());
            }
        }
    }

    if match_num == 0 {
        eprintln!("No matches.");
    } else {
        eprintln!("\nApplied {} replacements.", accepted_total);
    }
    Ok(())
}

fn read_key() -> io::Result<Action> {
    enable_raw_mode()?;
    let action = loop {
        match read_event()? {
            Event::Key(KeyEvent { code: KeyCode::Char('y'), .. }) => break Action::Yes,
            Event::Key(KeyEvent { code: KeyCode::Char('n'), .. }) => break Action::No,
            Event::Key(KeyEvent { code: KeyCode::Char('a'), .. }) => break Action::All,
            Event::Key(KeyEvent { code: KeyCode::Char('q'), .. }) => break Action::Quit,
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL) => break Action::Quit,
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => break Action::Quit,
            _ => continue,
        }
    };
    disable_raw_mode()?;
    Ok(action)
}

fn apply_change(lines: &mut Vec<String>, change: &Change) {
    let nl = if lines[change.line_idx].ends_with('\n') { "\n" } else { "" };
    let new_body: String = change.new_segments.iter().map(|(t, _)| t.as_str()).collect();
    lines[change.line_idx] = format!("{}{}", new_body, nl);
}

fn write_lines(path: &PathBuf, lines: &[String]) -> Result<()> {
    let content: String = lines.iter().map(|s| s.as_str()).collect();
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

// ── Display ───────────────────────────────────────────────────────────────────

fn print_interactive_match<W: WriteColor>(
    path: &PathBuf,
    change: &Change,
    match_num: usize,
    change_idx: usize,
    total_in_file: usize,
    lines: &[String],
    context: usize,
    preview: &Preview,
    out: &mut W,
) -> io::Result<()> {
    let dim = dimmed();
    let path_spec = spec(Color::Magenta, true);

    out.set_color(&dim)?;
    writeln!(out, "──────────────────────────────────────────")?;
    write!(out, "[#{match_num}] ")?;
    out.reset()?;
    out.set_color(&path_spec)?;
    write!(out, "{}", path.display())?;
    out.reset()?;
    out.set_color(&dim)?;
    writeln!(out, ":{} ({}/{})", change.line_idx + 1, change_idx + 1, total_in_file)?;
    out.reset()?;

    let start = change.line_idx.saturating_sub(context);
    let end = (change.line_idx + context).min(lines.len().saturating_sub(1));
    let change_map: HashMap<usize, &Change> = [(change.line_idx, change)].into_iter().collect();
    print_hunk(out, lines, &change_map, start, end, preview)?;
    writeln!(out)?;
    Ok(())
}

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

fn confirm() -> io::Result<bool> {
    eprint!("Apply these changes? [y/N] ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let ans = input.trim().to_lowercase();
    Ok(ans == "y" || ans == "yes")
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

fn run_edit_tui(cli: &Cli, initial_pattern: &str, initial_replacement: &str) -> Result<()> {
    let mut pat = InputField::new(initial_pattern);
    let mut rep = InputField::new(initial_replacement);
    let mut active: usize = 0; // 0 = pattern, 1 = replacement

    let mut all_lines: Vec<Line<'static>> = Vec::new();
    let mut all_edits: Vec<FileEdit> = Vec::new();
    let mut file_starts: Vec<usize> = Vec::new();
    let mut total_matches: usize = 0;
    let mut search_done = true;
    let mut scroll: usize = 0;
    let mut search_rx: Option<mpsc::Receiver<FileEdit>> = None;
    // If pre-populated, trigger search immediately (backdate the timer).
    let mut last_change: Option<std::time::Instant> = if initial_pattern.is_empty() {
        None
    } else {
        Some(std::time::Instant::now() - Duration::from_millis(300))
    };
    let mut regex_error: Option<String> = None;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut tui_action = TuiAction::Abort;

    'edit: loop {
        // Drain search results.
        if let Some(ref mut r) = search_rx {
            loop {
                match r.try_recv() {
                    Ok(edit) => {
                        total_matches += edit.total_matches();
                        file_starts.push(all_lines.len());
                        let eff = if rep.text.is_empty() { Preview::Old } else { cli.preview.clone() };
                        let new_lines =
                            render_file_to_lines(&edit, cli.context, &eff, cli.compact);
                        all_lines.extend(new_lines);
                        all_edits.push(edit);
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

        // Debounced search trigger.
        if let Some(t) = last_change {
            if t.elapsed() >= Duration::from_millis(200) {
                last_change = None;
                search_rx = None; // drop old rx → kills old search thread
                all_lines.clear();
                all_edits.clear();
                file_starts.clear();
                total_matches = 0;
                scroll = 0;
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

        // Snapshot values needed inside draw closure.
        let total_h = terminal.size()?.height as usize;
        let vh = total_h.saturating_sub(4); // 2 inputs + 1 border + 1 status
        scroll = scroll.min(all_lines.len().saturating_sub(vh));

        let pat_line = render_input_line("Pattern:     ", &pat, active == 0);
        let rep_line = render_input_line("Replacement: ", &rep, active == 1);
        let visible_end = all_lines.len().min(scroll + vh);
        let visible: Vec<Line<'static>> = all_lines[scroll..visible_end].to_vec();

        let status_text = if let Some(ref e) = regex_error {
            format!(" Error: {e}")
        } else if !search_done {
            format!(" Searching… {} matches", total_matches)
        } else if total_matches > 0 {
            format!(
                " {} matches in {} files · tab switch field · enter apply · esc quit",
                total_matches,
                all_edits.len()
            )
        } else if pat.text.is_empty() {
            " Type a pattern to search".to_string()
        } else {
            " No matches".to_string()
        };
        let status_style = if regex_error.is_some() {
            Style::default().bg(RColor::Red).fg(RColor::White)
        } else {
            Style::default().bg(RColor::DarkGray)
        };

        terminal.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),
                    Constraint::Min(0),
                    Constraint::Length(1),
                ])
                .split(area);

            f.render_widget(Paragraph::new(vec![pat_line, rep_line]), chunks[0]);

            f.render_widget(
                Paragraph::new(visible).block(
                    ratatui::widgets::Block::default()
                        .borders(ratatui::widgets::Borders::TOP)
                        .border_style(Style::default().fg(RColor::DarkGray)),
                ),
                chunks[1],
            );

            f.render_widget(
                Paragraph::new(status_text).style(status_style),
                chunks[2],
            );
        })?;

        if !poll(Duration::from_millis(50))? {
            continue;
        }

        let vh = terminal.size()?.height.saturating_sub(4) as usize;
        match read_event()? {
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => break 'edit,
            Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers, .. })
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                break 'edit;
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                if total_matches > 0 {
                    tui_action = TuiAction::Apply;
                    break 'edit;
                }
            }
            // Switch field
            Event::Key(KeyEvent { code: KeyCode::Tab, .. })
            | Event::Key(KeyEvent { code: KeyCode::BackTab, .. }) => {
                active = 1 - active;
            }
            // Scroll results
            Event::Key(KeyEvent { code: KeyCode::Char('n'), modifiers, .. })
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                scroll = scroll.saturating_add(1);
            }
            Event::Key(KeyEvent { code: KeyCode::Char('p'), modifiers, .. })
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                scroll = scroll.saturating_sub(1);
            }
            Event::Key(KeyEvent { code: KeyCode::Char('v'), modifiers, .. })
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                scroll = scroll.saturating_add(vh);
            }
            Event::Key(KeyEvent { code: KeyCode::Char('v'), modifiers, .. })
                if modifiers.contains(KeyModifiers::ALT) =>
            {
                scroll = scroll.saturating_sub(vh);
            }
            Event::Key(KeyEvent { code: KeyCode::Down, .. }) => {
                scroll = scroll.saturating_add(1);
            }
            Event::Key(KeyEvent { code: KeyCode::Up, .. }) => {
                scroll = scroll.saturating_sub(1);
            }
            Event::Key(KeyEvent { code: KeyCode::Char('>'), modifiers, .. })
                if modifiers.contains(KeyModifiers::ALT) =>
            {
                scroll = all_lines.len().saturating_sub(vh);
            }
            Event::Key(KeyEvent { code: KeyCode::Char('<'), modifiers, .. })
                if modifiers.contains(KeyModifiers::ALT) =>
            {
                scroll = 0;
            }
            Event::Key(KeyEvent { code: KeyCode::Char('}'), modifiers, .. })
                if modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(&s) = file_starts.iter().find(|&&s| s > scroll) {
                    scroll = s;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Char('{'), modifiers, .. })
                if modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(&s) = file_starts.iter().rev().find(|&&s| s < scroll) {
                    scroll = s;
                }
            }
            // Everything else goes to the active input field.
            ev => {
                let field = if active == 0 { &mut pat } else { &mut rep };
                if handle_input_key(field, ev) {
                    last_change = Some(std::time::Instant::now());
                }
            }
        }
    }

    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.backend_mut().execute(Show)?;
    disable_raw_mode()?;
    drop(terminal);

    match tui_action {
        TuiAction::Apply => {
            if let Some(r) = search_rx {
                for edit in r {
                    all_edits.push(edit);
                }
            }
            if all_edits.is_empty() {
                eprintln!("No matches.");
                return Ok(());
            }
            let total: usize = all_edits.iter().map(|e| e.total_matches()).sum();
            for edit in &all_edits {
                fs::write(&edit.path, &edit.new_text)
                    .with_context(|| format!("writing {}", edit.path.display()))?;
            }
            eprintln!("Replaced {} matches across {} files.", total, all_edits.len());
        }
        TuiAction::Interactive | TuiAction::Edit | TuiAction::Abort => {
            eprintln!("Aborted.");
        }
    }

    Ok(())
}
