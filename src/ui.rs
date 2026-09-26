use crate::{
    dialog::Dialog,
    doctor,
    ledger::{Disposition, Launch, Ledger, expand_path},
    nix_discovery,
    nix_migrate,
};
use anyhow::{Context, Result, ensure};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    collections::HashSet,
    io::{self, IsTerminal, Write},
    process::{Command, Stdio},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn categories(ledger: &Ledger) -> Vec<String> {
    let mut seen = HashSet::new();
    ledger
        .apps
        .iter()
        .filter_map(|a| {
            seen.insert(a.category.clone())
                .then_some(a.category.clone())
        })
        .collect()
}

pub fn counts(ledger: &Ledger, category: &str) -> [usize; 3] {
    let mut counts = [0; 3];
    for app in ledger.apps.iter().filter(|a| a.category == category) {
        counts[match app.disposition {
            Disposition::Using => 0,
            Disposition::Considering => 1,
            Disposition::Archived => 2,
        }] += 1;
    }
    counts
}

#[derive(Clone, Debug, PartialEq)]
enum Row {
    Category(String),
    Group(String, Disposition),
    App(usize),
}

enum Mode {
    Browse,
    Help,
    Search,
    Detail {
        app: usize,
        scroll: u16,
        source_open: bool,
        record_open: bool,
    },
    Add {
        fields: [String; 5],
        field: usize,
        picker: Option<CategoryPicker>,
        editing: Option<usize>,
    },
    Task(Box<crate::ui_task::Task>),
    Archive {
        app: usize,
        reason: String,
    },
}

enum CategoryPicker {
    List(usize),
    Custom(String),
}

fn include_considering(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('G')
        || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::SHIFT))
}

fn category_key(
    key: KeyCode,
    fields: &mut [String; 5],
    picker: &mut Option<CategoryPicker>,
    categories: &[String],
) {
    match picker {
        Some(CategoryPicker::List(selected)) => match key {
            KeyCode::Down => *selected = (*selected + 1).min(categories.len()),
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Esc | KeyCode::Left => *picker = None,
            KeyCode::Enter | KeyCode::Right => {
                if *selected == categories.len() {
                    *picker = Some(CategoryPicker::Custom(String::new()));
                } else {
                    fields[1] = categories[*selected].clone();
                    *picker = None;
                }
            }
            _ => {}
        },
        Some(CategoryPicker::Custom(text)) => match key {
            KeyCode::Esc => *picker = Some(CategoryPicker::List(categories.len())),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            KeyCode::Enter if !text.trim().is_empty() => {
                fields[1] = text.trim().into();
                *picker = None;
            }
            _ => {}
        },
        None => {
            if key == KeyCode::Right {
                *picker = Some(CategoryPicker::List(
                    categories
                        .iter()
                        .position(|c| c == &fields[1])
                        .unwrap_or(categories.len()),
                ));
            }
        }
    }
}

struct State {
    expanded: HashSet<String>,
    archives: HashSet<String>,
    selection: ListState,
    query: String,
    mode: Mode,
    message: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            expanded: HashSet::new(),
            archives: HashSet::new(),
            selection: ListState::default().with_selected(Some(0)),
            query: String::new(),
            mode: Mode::Browse,
            message: String::new(),
        }
    }
}

// Case-insensitive subsequence match, with contiguous hits ranked first.
fn fuzzy_score(query: &str, text: &str) -> Option<usize> {
    let query = query.to_lowercase();
    let text = text.to_lowercase();
    if let Some(pos) = text.find(&query) {
        return Some(pos);
    }
    let mut remaining = text.char_indices();
    let mut score = 1000;
    for wanted in query.chars() {
        score += remaining.find(|(_, c)| *c == wanted)?.0;
    }
    Some(score)
}

impl State {
    fn rows(&self, ledger: &Ledger) -> Vec<Row> {
        if !self.query.is_empty() {
            let mut matches: Vec<_> = ledger
                .apps
                .iter()
                .enumerate()
                .filter_map(|(i, a)| {
                    fuzzy_score(
                        &self.query,
                        &format!(
                            "{} {} {} {}",
                            a.name,
                            a.description,
                            a.category,
                            a.tags.join(" ")
                        ),
                    )
                    .map(|score| (score, i))
                })
                .collect();
            matches.sort_by_key(|(score, _)| *score);
            return matches.into_iter().map(|(_, i)| Row::App(i)).collect();
        }
        let mut rows = Vec::new();
        for category in categories(ledger) {
            rows.push(Row::Category(category.clone()));
            if !self.expanded.contains(&category) {
                continue;
            }
            for disposition in [
                Disposition::Using,
                Disposition::Considering,
                Disposition::Archived,
            ] {
                rows.push(Row::Group(category.clone(), disposition));
                if disposition == Disposition::Archived && !self.archives.contains(&category) {
                    continue;
                }
                for (index, _) in ledger
                    .apps
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| a.category == category && a.disposition == disposition)
                {
                    rows.push(Row::App(index));
                }
            }
        }
        rows
    }

    fn selected_app(&self, rows: &[Row]) -> Option<usize> {
        if let Mode::Detail { app, .. } = self.mode {
            return Some(app);
        }
        match rows.get(self.selection.selected().unwrap_or(0)) {
            Some(Row::App(i)) => Some(*i),
            _ => None,
        }
    }
}

fn color(disposition: Disposition) -> Color {
    match disposition {
        Disposition::Using => Color::Green,
        Disposition::Considering => Color::Yellow,
        Disposition::Archived => Color::Red,
    }
}

const MIN_CATEGORY_WIDTH: usize = 13; // Files & Disks

/// Five label/value pairs, a blank, and two control rows, inside a border.
const ADD_POPUP_HEIGHT: u16 = 5 * 2 + 1 + 2 + 2;

fn totals(ledger: &Ledger) -> [usize; 3] {
    let mut result = [0; 3];
    for category in categories(ledger) {
        for (total, count) in result.iter_mut().zip(counts(ledger, &category)) {
            *total += count;
        }
    }
    result
}

fn count_digits(ledger: &Ledger) -> usize {
    totals(ledger)
        .into_iter()
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(2)
}

fn minimum_size(ledger: &Ledger, state: &State) -> (u16, u16) {
    let height = match state.mode {
        Mode::Add { .. } => ADD_POPUP_HEIGHT,
        Mode::Archive { .. } => 9,
        Mode::Help => 19,
        // One body row, a choice prompt, status, footer, border, and a margin row.
        Mode::Task(_) => 12,
        _ => 8,
    };
    (
        (4 + MIN_CATEGORY_WIDTH + 3 * (count_digits(ledger) + 4)) as u16,
        height,
    )
}

fn fit(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    let truncated = text.width() > width;
    let available = width.saturating_sub(usize::from(truncated));
    for ch in text.chars() {
        let size = ch.width().unwrap_or(0);
        if used + size > available {
            break;
        }
        result.push(ch);
        used += size;
    }
    if truncated && width > 0 {
        result.push('…');
        used += 1;
    }
    result.push_str(&" ".repeat(width.saturating_sub(used)));
    result
}

fn name_cell(name: &str, width: usize, marker: &str) -> String {
    let name_width = width.saturating_sub(3);
    let name = fit(name, name_width);
    let padding = name_width.saturating_sub(name.width());
    format!("{name}{} {marker}", " ".repeat(padding))
}

fn count_cells(values: [usize; 3], digits: usize, labels: bool) -> Vec<Span<'static>> {
    let mut cells = Vec::new();
    for ((count, label), disposition) in values.into_iter().zip(['U', 'C', 'A']).zip([
        Disposition::Using,
        Disposition::Considering,
        Disposition::Archived,
    ]) {
        let text = if labels {
            format!("{count:>digits$} {label}")
        } else {
            format!("{count:>width$}", width = digits + 2)
        };
        cells.push(Span::styled(text, Style::default().fg(color(disposition))));
        cells.push(Span::raw(" |"));
    }
    cells
}

fn draw(frame: &mut Frame, ledger: &Ledger, state: &mut State, rows: &[Row]) {
    let (min_width, min_height) = minimum_size(ledger, state);
    if frame.area().width < min_width || frame.area().height < min_height {
        frame.render_widget(Paragraph::new(format!("Track\nNeed {min_width} columns × {min_height} rows\nResize to continue\nq quit · Esc back")).wrap(Wrap { trim: false }), frame.area());
        return;
    }
    let digits = count_digits(ledger);
    let category_width = categories(ledger)
        .iter()
        .map(|c| c.width())
        .max()
        .unwrap_or(MIN_CATEGORY_WIDTH)
        .max(MIN_CATEGORY_WIDTH)
        .min(frame.area().width as usize - 4 - 3 * (digits + 4));
    let areas = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(
            if state.message.is_empty() && !matches!(state.mode, Mode::Search) {
                0
            } else {
                2
            },
        ),
        Constraint::Length(2),
    ])
    .split(frame.area());
    let mut heading = vec![Span::styled(
        fit(" Track", category_width + 4),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    heading.extend(count_cells(totals(ledger), digits, true));
    frame.render_widget(
        Paragraph::new(Line::from(heading)).block(Block::default().borders(Borders::BOTTOM)),
        areas[0],
    );
    let items: Vec<_> = rows
        .iter()
        .map(|row| {
            let line = match row {
                Row::Category(category) => {
                    let c = counts(ledger, category);
                    let mut cells = vec![Span::styled(
                        format!(
                            "{} {}",
                            if state.expanded.contains(category) {
                                "▾"
                            } else {
                                "▸"
                            },
                            fit(category, category_width)
                        ),
                        Style::default().add_modifier(Modifier::BOLD),
                    )];
                    cells.extend(count_cells(c, digits, false));
                    Line::from(cells)
                }
                Row::Group(category, disposition) => {
                    let label = match disposition {
                        Disposition::Using => "Using",
                        Disposition::Considering => "Considering",
                        Disposition::Archived => "Archived",
                    };
                    let suffix = if *disposition == Disposition::Archived {
                        format!(
                            " {} {}",
                            if state.archives.contains(category) {
                                "▾"
                            } else {
                                "▸"
                            },
                            counts(ledger, category)[2]
                        )
                    } else {
                        String::new()
                    };
                    Line::styled(
                        format!("  {label}{suffix}"),
                        Style::default()
                            .fg(color(*disposition))
                            .add_modifier(Modifier::BOLD),
                    )
                }
                Row::App(index) => {
                    let app = &ledger.apps[*index];
                    let status = doctor::health(app);
                    let health = if status.starts_with('!') {
                        format!("  {status}")
                    } else {
                        String::new()
                    };
                    let description = if app.description.is_empty() {
                        "Description not yet recorded"
                    } else {
                        &app.description
                    };
                    Line::from(vec![
                        Span::styled(
                            format!(
                                "  {}",
                                name_cell(
                                    &app.name,
                                    category_width,
                                    doctor::presence_marker(app),
                                )
                            ),
                            Style::default().fg(color(app.disposition)),
                        ),
                        Span::styled(health, Style::default().fg(Color::Red)),
                        Span::raw(format!("  {description}")),
                    ])
                }
            };
            ListItem::new(line)
        })
        .collect();
    let mut list = List::new(items);
    if !state.query.is_empty() {
        list = list.block(Block::default().title(format!(
            "/{} · {} matches",
            state.query,
            rows.len()
        )));
    }
    frame.render_stateful_widget(
        list.highlight_symbol("› ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        areas[1],
        &mut state.selection,
    );
    frame.render_widget(
        Paragraph::new(state.message.as_str()).wrap(Wrap { trim: false }),
        areas[2],
    );
    frame.render_widget(
        Paragraph::new("↑↓ move  →← fold  Enter inspect\ne edit  i install  n Nix  / search  ? help  q quit")
            .style(Style::default().add_modifier(Modifier::DIM)),
        areas[3],
    );

    match &mut state.mode {
        Mode::Help => popup(
            frame,
            " Track — keys ",
            vec![
                Line::raw("↑↓ / jk    Move"),
                Line::raw("→ / ←      Expand / parent"),
                Line::raw("Enter      Inspect"),
                Line::raw("/          Search"),
                Line::raw("a / A      Add / complete recipe"),
                Line::raw("e          Edit / review source"),
                Line::raw("U / C      Using / Considering"),
                Line::raw("x          Archive with reason"),
                Line::raw("o          Launch"),
                Line::raw("u / i      Update / install selected"),
                Line::raw("n          Check Nixpkgs"),
                Line::raw("m          Nix migrate (confirmed)"),
                Line::raw("g          Update Using"),
                Line::raw("G          + Considering"),
                Line::raw("r          Reload ledger"),
                Line::raw("q          Back / quit"),
                Line::raw(""),
                Line::raw("Direct Go: out of scope · Nix: landed"),
                Line::raw("Esc / ?    Close help"),
            ],
            20,
        ),
        Mode::Detail { app, scroll, source_open, record_open } => {
            let assessment = doctor::install_assessment(&ledger.apps[*app]);
            let text = doctor::report_with_sections(
                &ledger.apps[*app], &ledger.record(*app), *source_open, *record_open,
            );
            let verdict_color = match assessment.verdict {
                doctor::InstallVerdict::Ready => Color::Green,
                doctor::InstallVerdict::Review => Color::Yellow,
                doctor::InstallVerdict::Unavailable => Color::Red,
            };
            let lines: Vec<Line<'_>> = text.lines().map(|line| {
                if line.starts_with("Install: ") {
                    Line::from(vec![
                        Span::raw("Install: "),
                        Span::styled(assessment.verdict.label(), Style::default().fg(verdict_color).add_modifier(Modifier::BOLD)),
                    ])
                } else {
                    Line::raw(line.to_owned())
                }
            }).collect();
            frame.render_widget(Clear, areas[1]);
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title(format!(
                        " {} — ↑↓ scroll · s source · l record · Esc back ",
                        ledger.apps[*app].name
                    )))
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0)),
                areas[1],
            );
        }
        Mode::Search => {
            frame.render_widget(Clear, areas[2]);
            frame.render_widget(
                Paragraph::new(format!(
                    "/{}▏  (Enter browse results · Esc clear)",
                    state.query
                )),
                areas[2],
            );
        }
        Mode::Task(task) => task.draw(frame),
        Mode::Add {
            fields,
            field,
            picker,
            editing,
        } => {
            let labels = [
                "Name",
                "Category (→ change)",
                "Upstream / identity (optional)",
                "Description",
                "Tags (comma separated)",
            ];
            let mut lines = vec![];
            for (i, (label, text)) in labels.iter().zip(fields).enumerate() {
                lines.push(Line::styled(
                    *label,
                    Style::default().add_modifier(Modifier::DIM),
                ));
                lines.push(Line::styled(
                    fit(
                        &format!(
                            "{}{}{}",
                            if i == *field { "› " } else { "  " },
                            text,
                            if i == *field && i != 1 { "▏" } else { "" }
                        ),
                        frame.area().width.saturating_sub(6) as usize,
                    ),
                    if i == *field {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    },
                ));
            }
            lines.push(Line::raw(""));
            lines.push(Line::raw("Tab / Shift-Tab fields"));
            lines.push(Line::raw("Enter setup · ^S record · Esc"));
            popup(frame, if editing.is_some() { " Edit record " } else { " Add to Considering " }, lines, ADD_POPUP_HEIGHT);
            if let Some(picker) = picker {
                match picker {
                    CategoryPicker::List(selected) => {
                        let mut names = categories(ledger);
                        names.push("<custom>".into());
                        let area = frame.area();
                        let rect = Rect::new(
                            2,
                            2,
                            area.width.saturating_sub(4),
                            area.height.saturating_sub(4),
                        );
                        frame.render_widget(Clear, rect);
                        let mut list = ListState::default().with_selected(Some(*selected));
                        frame.render_stateful_widget(
                            List::new(names)
                                .block(Block::bordered().title(" Category · Enter select "))
                                .highlight_symbol("› ")
                                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
                            rect,
                            &mut list,
                        );
                    }
                    CategoryPicker::Custom(text) => popup(
                        frame,
                        " <custom> category ",
                        vec![
                            Line::raw(format!("{text}▏")),
                            Line::raw("Enter save · Esc back"),
                        ],
                        5,
                    ),
                }
            }
        }
        Mode::Archive { app, reason } => {
            popup(
                frame,
                &format!(" Archive {} ", ledger.apps[*app].name),
                vec![
                    Line::raw("Review / reason:"),
                    Line::raw(format!("{reason}▏")),
                    Line::raw(""),
                    Line::raw("Managed installations offer removal next."),
                    Line::raw("Enter continue · Esc cancel"),
                ],
                9,
            );
        }
        Mode::Browse => {}
    }
}

fn popup(frame: &mut Frame, title: &str, lines: Vec<Line<'_>>, height: u16) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(90);
    let height = height.min(area.height);
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn launch(terminal: &mut DefaultTerminal, recipe: &Launch) -> Result<String> {
    ensure!(!recipe.program.trim().is_empty(), "Launch program is empty");
    let program = expand_path(&recipe.program);
    let args: Vec<_> = recipe
        .args
        .iter()
        .map(|arg| expand_path(arg).into_os_string())
        .collect();
    if recipe.gui {
        let child = Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("Cannot launch {}", program.display()))?;
        let pid = child.id();
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        return Ok(format!(
            "Started {} (pid {pid}); process startup is not a health check",
            program.display()
        ));
    }
    ratatui::restore();
    let result = Command::new(&program)
        .args(&args)
        .status()
        .with_context(|| format!("Cannot launch {}", program.display()));
    match &result {
        Ok(status) => println!("\n{} exited: {status}", program.display()),
        Err(e) => println!("\n{e:#}"),
    }
    print!("Press Enter to return to Track.");
    let _ = io::stdout().flush();
    let _ = io::stdin().read_line(&mut String::new());
    *terminal = ratatui::init();
    terminal.clear()?;
    Ok(format!("{} exited: {}", program.display(), result?))
}

fn handle(
    key: KeyEvent,
    ledger: &mut Ledger,
    state: &mut State,
    rows: &[Row],
    terminal: &mut DefaultTerminal,
) -> Result<bool> {
    if let Mode::Task(task) = &mut state.mode {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if task.done {
                return Ok(true);
            }
            task.cancel();
            return Ok(false);
        }
        if task.done && key.code == KeyCode::Enter {
            state.mode = Mode::Browse;
            return Ok(false);
        }
        if task.done && key.code == KeyCode::Esc {
            return Ok(true);
        }
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }
    let size = terminal.size()?;
    let (min_width, min_height) = minimum_size(ledger, state);
    if size.width < min_width || size.height < min_height {
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Esc => state.mode = Mode::Browse,
            _ => {}
        }
        return Ok(false);
    }
    match &mut state.mode {
        Mode::Task(task) => {
            task.key(key);
            return Ok(false);
        }
        Mode::Help => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?' | 'q')) {
                state.mode = Mode::Browse;
            }
            return Ok(false);
        }
        Mode::Search => {
            match key.code {
                KeyCode::Esc => {
                    state.query.clear();
                    state.mode = Mode::Browse;
                }
                KeyCode::Enter => state.mode = Mode::Browse,
                KeyCode::Backspace => {
                    state.query.pop();
                }
                KeyCode::Char(c) => state.query.push(c),
                _ => {}
            }
            state.selection.select(Some(0));
            return Ok(false);
        }
        Mode::Add {
            fields,
            field,
            picker,
            editing,
        } => {
            if picker.is_some() || (*field == 1 && key.code == KeyCode::Right) {
                category_key(key.code, fields, picker, &categories(ledger));
                return Ok(false);
            }
            let record_only =
                key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s');
            if key.code == KeyCode::Enter || record_only {
                let saved = fields.clone();
                if let Some(index) = editing {
                    let index = *index;
                    ledger.edit_record(index, &saved[0], &saved[1], &saved[2], &saved[3], &saved[4])?;
                    let source = ledger.apps[index].identity.clone();
                    if !record_only && source.starts_with("https://github.com/") && ledger.apps[index].recipe.is_none() {
                        state.mode = Mode::Task(Box::new(crate::ui_task::Task::start(
                            ledger.clone(),
                            move |ledger, dialog| crate::intake::configure_source(ledger, index, dialog),
                        )));
                    } else {
                        state.mode = Mode::Detail { app: index, scroll: 0, source_open: false, record_open: false };
                        state.message = "Record saved. Installation provenance is unchanged.".into();
                    }
                    return Ok(false);
                }
                if !record_only && !saved[2].trim().is_empty() {
                    state.mode = Mode::Task(Box::new(crate::ui_task::Task::start(
                        ledger.clone(),
                        move |ledger, dialog| {
                            crate::intake::interactive_with(
                                ledger,
                                Some(saved[2].trim()),
                                &saved[1],
                                Some(&saved),
                                dialog,
                            )
                        },
                    )));
                } else {
                    let identity = if saved[2].trim().is_empty() {
                        saved[0].trim()
                    } else {
                        saved[2].trim()
                    };
                    if let Ok(index) = ledger.find(identity) {
                        state.mode = Mode::Detail {
                            app: index,
                            scroll: 0,
                            source_open: false,
                            record_open: false,
                        };
                        state.message = "Already tracked; opened existing record. Use a to complete its recipe.".into();
                    } else {
                        ledger.add(&saved[0], &saved[1], &saved[2], &saved[3], &saved[4])?;
                        state.query = saved[0].trim().into();
                        state.selection.select(Some(0));
                        state.message =
                            "Added to Considering; installation remains unknown.".into();
                        state.mode = Mode::Browse;
                    }
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc => state.mode = Mode::Browse,
                KeyCode::Tab => *field = (*field + 1) % fields.len(),
                KeyCode::BackTab => *field = (*field + fields.len() - 1) % fields.len(),
                KeyCode::Backspace if *field != 1 => {
                    fields[*field].pop();
                }
                KeyCode::Char(c) if *field != 1 => fields[*field].push(c),
                _ => {}
            }
            return Ok(false);
        }
        Mode::Archive { app, reason } => {
            match key.code {
                KeyCode::Esc => state.mode = Mode::Browse,
                KeyCode::Backspace => {
                    reason.pop();
                }
                KeyCode::Char(c) => reason.push(c),
                KeyCode::Enter => {
                    let app = *app;
                    let reason = reason.clone();
                    state.mode = Mode::Task(Box::new(crate::ui_task::Task::start(
                        ledger.clone(),
                        move |ledger, dialog| {
                            crate::removal::archive(ledger, app, &reason, dialog)
                        },
                    )));
                }
                _ => {}
            }
            return Ok(false);
        }
        Mode::Detail { scroll, source_open, record_open, .. } => match key.code {
            KeyCode::Char('s') => {
                *source_open = !*source_open;
                *scroll = 0;
                return Ok(false);
            }
            KeyCode::Char('l') => {
                *record_open = !*record_open;
                *scroll = 0;
                return Ok(false);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *scroll = scroll.saturating_add(1);
                return Ok(false);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                *scroll = scroll.saturating_sub(1);
                return Ok(false);
            }
            KeyCode::PageDown => {
                *scroll = scroll.saturating_add(10);
                return Ok(false);
            }
            KeyCode::PageUp => {
                *scroll = scroll.saturating_sub(10);
                return Ok(false);
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => {
                state.mode = Mode::Browse;
                return Ok(false);
            }
            _ => {}
        },
        Mode::Browse => {}
    }
    let selected = state.selection.selected().unwrap_or(0);
    let app = state.selected_app(rows);
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Esc => {
            state.query.clear();
            state.selection.select(Some(0));
        }
        KeyCode::Down | KeyCode::Char('j') => state
            .selection
            .select(Some((selected + 1).min(rows.len().saturating_sub(1)))),
        KeyCode::Up | KeyCode::Char('k') => {
            state.selection.select(Some(selected.saturating_sub(1)))
        }
        KeyCode::PageDown => state
            .selection
            .select(Some((selected + 10).min(rows.len().saturating_sub(1)))),
        KeyCode::PageUp => state.selection.select(Some(selected.saturating_sub(10))),
        KeyCode::Right | KeyCode::Enter => match rows.get(selected) {
            Some(Row::Category(category)) => {
                state.expanded.insert(category.clone());
            }
            Some(Row::Group(category, Disposition::Archived)) => {
                if !state.archives.insert(category.clone()) {
                    state.archives.remove(category);
                }
            }
            Some(Row::App(i)) => state.mode = Mode::Detail { app: *i, scroll: 0, source_open: false, record_open: false },
            _ => {}
        },
        KeyCode::Left => {
            if !state.query.is_empty() {
                state.query.clear();
                state.selection.select(Some(0));
            } else if let Some(row) = rows.get(selected) {
                let category = match row {
                    Row::Category(c) | Row::Group(c, _) => c.clone(),
                    Row::App(i) => ledger.apps[*i].category.clone(),
                };
                if matches!(row, Row::Group(_, Disposition::Archived))
                    && state.archives.remove(&category)
                {
                } else {
                    state.expanded.remove(&category);
                    let collapsed = state.rows(ledger);
                    state.selection.select(
                        collapsed
                            .iter()
                            .position(|r| *r == Row::Category(category.clone())),
                    );
                }
            }
        }
        KeyCode::Char('?') => state.mode = Mode::Help,
        KeyCode::Char('/') => {
            state.query.clear();
            state.mode = Mode::Search;
        }
        KeyCode::Char('a' | 'A') => {
            let category = match rows.get(selected) {
                Some(Row::Category(c) | Row::Group(c, _)) => c.clone(),
                Some(Row::App(i)) => ledger.apps[*i].category.clone(),
                None => String::new(),
            };
            state.mode = Mode::Add {
                fields: [
                    String::new(),
                    category,
                    String::new(),
                    String::new(),
                    String::new(),
                ],
                field: 0,
                picker: None,
                editing: None,
            };
        }
        KeyCode::Char('e') if app.is_some() => {
            let i = app.unwrap();
            let selected = &ledger.apps[i];
            state.mode = Mode::Add {
                fields: [selected.name.clone(), selected.category.clone(),
                    if selected.identity.starts_with("name:") { String::new() } else { selected.identity.clone() },
                    selected.description.clone(), selected.tags.join(", ")],
                field: 0,
                picker: None,
                editing: Some(i),
            };
        }
        KeyCode::Char('U' | 'C') if app.is_some() => {
            let disposition = if key.code == KeyCode::Char('U') {
                Disposition::Using
            } else {
                Disposition::Considering
            };
            let i = app.unwrap();
            ledger.decide(i, disposition, None)?;
            state.message = format!(
                "{} → {} (installation and outcome unchanged)",
                ledger.apps[i].name,
                disposition.label()
            );
            let new_rows = state.rows(ledger);
            if let Some(pos) = new_rows.iter().position(|r| *r == Row::App(i)) {
                state.selection.select(Some(pos));
            }
        }
        KeyCode::Char('x') if app.is_some() => {
            state.mode = Mode::Archive {
                app: app.unwrap(),
                reason: String::new(),
            }
        }
        KeyCode::Char('o') if app.is_some() => {
            let recipe = ledger.apps[app.unwrap()].launch.as_ref().context("No launch recipe recorded. Add [apps.launch] with program, args and gui to the TOML, then r to reload.")?;
            state.message = launch(terminal, recipe)?;
        }
        KeyCode::Char('n') if app.is_some() => {
            let index = app.unwrap();
            state.mode = Mode::Task(Box::new(crate::ui_task::Task::start(
                ledger.clone(),
                move |ledger, dialog| {
                    nix_discovery::discover_tui(ledger, index, dialog.cancel_flag(), |message| {
                        dialog.message(message.into())
                    })
                },
            )));
        }
        KeyCode::Char('m') if app.is_some() => {
            let index = app.unwrap();
            state.mode = Mode::Task(Box::new(crate::ui_task::Task::start(
                ledger.clone(),
                move |ledger, dialog| nix_migrate::migrate(ledger, index, dialog, false, Some(dialog.cancel_flag())),
            )));
        }
        KeyCode::Char('g' | 'G') | KeyCode::Char('u' | 'i')
            if app.is_some() || !matches!(key.code, KeyCode::Char('u' | 'i')) =>
        {
            let single = if matches!(key.code, KeyCode::Char('u' | 'i')) {
                app
            } else {
                None
            };
            let considering = include_considering(key);
            state.mode = Mode::Task(Box::new(crate::ui_task::Task::start(
                ledger.clone(),
                move |ledger, dialog| {
                    if key.code == KeyCode::Char('i') {
                        crate::dialog::install_one(ledger, single.unwrap(), dialog)
                    } else {
                        crate::dialog::updates(ledger, single, considering, dialog)
                    }
                },
            )));
        }
        KeyCode::Char('r') => {
            *ledger = Ledger::open(&ledger.path)?;
            state.mode = Mode::Browse;
            state.selection.select(Some(0));
            state.message = "Reloaded ledger and filesystem observations.".into();
        }
        _ => {}
    }
    Ok(false)
}

pub fn run(ledger: &mut Ledger) -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "The TUI needs a terminal. Use apptrack list or apptrack <app> doctor in scripts."
    );
    crossterm::execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, ledger);
    ratatui::restore();
    crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste)?;
    result
}

fn event_loop(terminal: &mut DefaultTerminal, ledger: &mut Ledger) -> Result<()> {
    let mut state = State::default();
    let mut needs_draw = true;
    loop {
        if let Mode::Task(task) = &mut state.mode {
            needs_draw |= task.poll(ledger);
        }
        let rows = state.rows(ledger);
        state.selection.select(if rows.is_empty() {
            None
        } else {
            Some(state.selection.selected().unwrap_or(0).min(rows.len() - 1))
        });
        if needs_draw {
            terminal.draw(|frame| draw(frame, ledger, &mut state, &rows))?;
            needs_draw = false;
        }
        if !event::poll(std::time::Duration::from_millis(50))? {
            continue;
        }
        let input = event::read()?;
        needs_draw = true;
        if let Event::Paste(text) = &input {
            let size = terminal.size()?;
            let minimum = minimum_size(ledger, &state);
            if size.width < minimum.0 || size.height < minimum.1 {
                continue;
            }
            let text: String = text.chars().filter(|c| !c.is_control()).collect();
            match &mut state.mode {
                Mode::Task(task) => task.paste(&text),
                Mode::Add {
                    fields,
                    field,
                    picker,
                    ..
                } => match picker {
                    Some(CategoryPicker::Custom(value)) => value.push_str(&text),
                    None if *field != 1 => fields[*field].push_str(&text),
                    _ => {}
                },
                Mode::Search => state.query.push_str(&text),
                _ => {}
            }
        }
        if let Event::Key(key) = input {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match handle(key, ledger, &mut state, &rows, terminal) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => state.message = format!("{error:#}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_markers_are_two_cells_at_a_fixed_name_edge() {
        let present = name_cell("short", 13, "\\/");
        let absent = name_cell("a much longer application", 13, "[]");
        assert_eq!(present.width(), 13);
        assert_eq!(absent.width(), 13);
        assert!(present.ends_with("\\/"));
        assert!(absent.ends_with("[]"));
    }

    #[test]
    fn global_scope_accepts_both_shifted_g_event_encodings() {
        assert!(include_considering(KeyEvent::new(
            KeyCode::Char('G'),
            KeyModifiers::SHIFT
        )));
        assert!(include_considering(KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::SHIFT
        )));
        assert!(!include_considering(KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn category_is_locked_until_right_and_custom_is_last() {
        let mut fields = [
            String::new(),
            "Editing".into(),
            String::new(),
            String::new(),
            String::new(),
        ];
        let categories = vec!["Editing".into(), "Tools".into()];
        let mut picker = None;
        category_key(KeyCode::Backspace, &mut fields, &mut picker, &categories);
        category_key(KeyCode::Char('x'), &mut fields, &mut picker, &categories);
        assert_eq!(fields[1], "Editing");
        assert!(picker.is_none());
        category_key(KeyCode::Right, &mut fields, &mut picker, &categories);
        category_key(KeyCode::Down, &mut fields, &mut picker, &categories);
        category_key(KeyCode::Enter, &mut fields, &mut picker, &categories);
        assert_eq!(fields[1], "Tools");
        category_key(KeyCode::Right, &mut fields, &mut picker, &categories);
        category_key(KeyCode::Down, &mut fields, &mut picker, &categories);
        category_key(KeyCode::Enter, &mut fields, &mut picker, &categories);
        assert!(matches!(picker, Some(CategoryPicker::Custom(_))));
        category_key(KeyCode::Enter, &mut fields, &mut picker, &categories);
        assert_eq!(fields[1], "Tools");
        for c in "Research".chars() {
            category_key(KeyCode::Char(c), &mut fields, &mut picker, &categories);
        }
        category_key(KeyCode::Enter, &mut fields, &mut picker, &categories);
        assert_eq!(fields[1], "Research");
        assert!(picker.is_none());
    }
    fn ledger_fixture() -> Result<(tempfile::TempDir, Ledger)> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        std::fs::write(
            &path,
            "schema_version = 1\n[[apps]]\nidentity = 'name:toast'\nname = 'toast'\ncategory = 'Development Workspaces'\ndisposition = 'using'\n",
        )?;
        Ok((dir, Ledger::open(&path)?))
    }

    /// A ledger shaped like the real bootstrap: 19 categories, the widest being
    /// "Development Workspaces" at 22 cells, counts reaching two digits.
    fn wide_ledger_fixture() -> Result<(tempfile::TempDir, Ledger)> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        let categories = [
            "Development Workspaces",
            "System Monitoring",
            "Graphics & Design",
            "Data & Documents",
            "AI Workspaces",
            "Files & Disks",
            "System Tools",
            "Productivity",
            "Networking",
            "AI Agents",
            "Terminals",
            "AI Tools",
            "Editing",
            "Writing",
            "Desktop",
            "Media",
            "Games",
            "PKM",
            "Git",
        ];
        let mut text = "schema_version = 1\n".to_string();
        for (i, category) in categories.iter().enumerate() {
            for n in 0..12 {
                text.push_str(&format!(
                    "\n[[apps]]\nidentity = 'name:a{i}-{n}'\nname = 'vibecodingtracker'\ncategory = '{category}'\ndisposition = 'using'\n"
                ));
            }
        }
        std::fs::write(&path, text)?;
        Ok((dir, Ledger::open(&path)?))
    }

    /// Renders a frame at an exact size and returns the visible rows, trimmed.
    fn render(ledger: &Ledger, state: &mut State, width: u16, height: u16) -> Result<Vec<String>> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
        let rows = state.rows(ledger);
        terminal.draw(|frame| draw(frame, ledger, state, &rows))?;
        let buffer = terminal.backend().buffer().clone();
        Ok((0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect())
    }

    #[test]
    fn add_dialog_puts_values_under_labels_and_fits_its_minimum_height() -> Result<()> {
        let (_dir, ledger) = ledger_fixture()?;
        let mut state = State::default();
        state.mode = Mode::Add {
            fields: [
                "Toast".into(),
                "Development Workspaces".into(),
                "https://github.com/paradise-runner/toast".into(),
                String::new(),
                String::new(),
            ],
            field: 0,
            picker: None,
            editing: None,
        };
        let (min_width, min_height) = minimum_size(&ledger, &state);
        let rows = render(&ledger, &mut state, min_width.max(60), min_height)?;
        let text = rows.join("\n");
        // Nothing clipped at exactly the minimum height.
        assert!(text.contains("Tab / Shift-Tab fields"), "{text}");
        assert!(text.contains("Enter setup"), "{text}");
        // Each value sits on the row directly under its label, no blank between.
        for (label, value) in [
            ("Name", "Toast"),
            ("Category", "Development Workspaces"),
            ("Upstream", "github.com/paradise-runner/toast"),
        ] {
            let at = rows
                .iter()
                .position(|r| r.contains(label))
                .unwrap_or_else(|| panic!("no {label} row in:\n{text}"));
            assert!(
                rows[at + 1].contains(value),
                "{label} value not on the next row; got {:?}\n{text}",
                rows[at + 1]
            );
        }
        Ok(())
    }

    #[test]
    fn help_names_source_scope_without_exceeding_the_quarter_tile() -> Result<()> {
        let (_dir, ledger) = ledger_fixture()?;
        let mut state = State::default();
        state.mode = Mode::Help;
        let text = render(&ledger, &mut state, 87, 22)?.join("\n");
        assert!(text.contains("Direct Go: out of scope · Nix: landed"), "{text}");
        assert!(text.contains("m          Nix migrate (confirmed)"), "{text}");
        assert!(!text.contains("Resize to continue"), "{text}");
        Ok(())
    }

    #[test]
    fn add_dialog_warns_instead_of_clipping_one_row_below_its_minimum() -> Result<()> {
        let (_dir, ledger) = ledger_fixture()?;
        let mut state = State::default();
        state.mode = Mode::Add {
            fields: [
                "Toast".into(),
                "Development Workspaces".into(),
                String::new(),
                String::new(),
                String::new(),
            ],
            field: 0,
            picker: None,
            editing: None,
        };
        let (min_width, min_height) = minimum_size(&ledger, &state);
        let rows = render(&ledger, &mut state, min_width.max(60), min_height - 1)?;
        let text = rows.join("\n");
        assert!(text.contains("Resize to continue"), "{text}");
        Ok(())
    }

    /// A silent worker must still show time passing, or it reads as hung.
    #[test]
    fn a_running_task_shows_elapsed_seconds_without_worker_output() -> Result<()> {
        let (_dir, mut ledger) = ledger_fixture()?;
        let hold = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let worker_hold = hold.clone();
        let mut task = crate::ui_task::Task::start(ledger.clone(), move |_, dialog| {
            use crate::dialog::Dialog;
            dialog.message("Checking releases".into());
            while worker_hold.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok("Done".into())
        });
        // Poll past a one-second boundary while the worker stays silent.
        let mut ticked = false;
        for _ in 0..300 {
            ticked |= task.poll(&mut ledger);
            if ticked && !task.done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut state = State::default();
        state.mode = Mode::Task(Box::new(task));
        let text = render(&ledger, &mut state, 87, 22)?.join("\n");
        hold.store(false, std::sync::atomic::Ordering::Release);
        assert!(
            text.contains("Checking releases") && text.contains("s)"),
            "no elapsed figure while the worker is silent:\n{text}"
        );
        Ok(())
    }

    #[test]
    fn task_popup_is_sized_to_its_content_and_centred_not_full_screen() -> Result<()> {
        let (_dir, mut ledger) = ledger_fixture()?;
        let mut task = crate::ui_task::Task::start(ledger.clone(), |_, dialog| {
            use crate::dialog::Dialog;
            dialog.message("Plan\nshort body\nthree rows".into());
            Ok("Done".into())
        });
        for _ in 0..200 {
            task.poll(&mut ledger);
            if task.done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(task.done, "worker never finished");
        let mut state = State::default();
        state.mode = Mode::Task(Box::new(task));
        let rows = render(&ledger, &mut state, 80, 40)?;
        assert_eq!(
            rows.iter().map(|row| row.matches("Done").count()).sum::<usize>(),
            1,
            "the final result should appear in the body once, not repeat as status"
        );
        let framed: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.contains('│') || r.contains('┌') || r.contains('└'))
            .map(|(i, _)| i)
            .collect();
        let (top, bottom) = (framed[0], framed[framed.len() - 1]);
        let height = bottom - top + 1;
        assert!(
            height < 30,
            "short plan should not fill a 40-row terminal; used {height} rows\n{}",
            rows.join("\n")
        );
        let above = top;
        let below = rows.len() - 1 - bottom;
        assert!(
            above.abs_diff(below) <= 1,
            "popup not vertically centred: {above} above, {below} below"
        );
        Ok(())
    }

    /// AppTrack lives in a quarter tile. IBM Plex Mono at 16px is a 10x21 cell
    /// (0.6em advance, 1.3em line), so a 1920x1200 quarter is 96x28 — one row
    /// held back here for window decoration. Height is the scarce axis; the map
    /// needs only 44 of the 96 columns.
    #[test]
    fn every_mode_fits_a_quarter_tile() -> Result<()> {
        let (_dir, mut ledger) = ledger_fixture()?;
        const QUARTER: (u16, u16) = (87, 22);
        let mut task = crate::ui_task::Task::start(ledger.clone(), |_, dialog| {
            use crate::dialog::Dialog;
            dialog.message("Plan\nrow two\nrow three".into());
            Ok("Done".into())
        });
        for _ in 0..200 {
            task.poll(&mut ledger);
            if task.done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let modes = [
            ("browse", Mode::Browse),
            ("help", Mode::Help),
            (
                "add",
                Mode::Add {
                    fields: [
                        "Toast".into(),
                        "Development Workspaces".into(),
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                    field: 0,
                    picker: None,
                    editing: None,
                },
            ),
            ("task", Mode::Task(Box::new(task))),
        ];
        // The real ledger's 19 categories must still browse in a quarter.
        let (_wide_dir, wide) = wide_ledger_fixture()?;
        let mut wide_state = State::default();
        let (wide_min_width, _) = minimum_size(&wide, &wide_state);
        assert!(
            wide_min_width <= QUARTER.0,
            "map needs {wide_min_width} cols, wider than a quarter"
        );
        let map = render(&wide, &mut wide_state, QUARTER.0, QUARTER.1)?;
        let text = map.join("\n");
        assert!(!text.contains("Resize to continue"), "{text}");
        let visible = map.iter().filter(|r| r.contains('|')).count();
        assert!(
            visible >= 15,
            "only {visible} of 19 categories visible in a quarter:\n{text}"
        );

        for (name, mode) in modes {
            let mut state = State::default();
            state.mode = mode;
            let (min_width, min_height) = minimum_size(&ledger, &state);
            assert!(
                min_width <= QUARTER.0 && min_height <= QUARTER.1,
                "{name} needs {min_width}x{min_height}, larger than a {}x{} quarter",
                QUARTER.0,
                QUARTER.1
            );
            let text = render(&ledger, &mut state, QUARTER.0, QUARTER.1)?.join("\n");
            assert!(
                !text.contains("Resize to continue"),
                "{name} does not fit a quarter tile:\n{text}"
            );
        }
        Ok(())
    }

    #[test]
    fn map_starts_collapsed_and_archives_are_separate() -> Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        write!(
            file,
            "schema_version = 1\n[[apps]]\nidentity = 'name:a'\nname = 'a'\ncategory = 'Editing'\ndisposition = 'using'\n[[apps]]\nidentity = 'name:b'\nname = 'b'\ncategory = 'Editing'\ndisposition = 'archived'\n"
        )?;
        let ledger = Ledger::open(file.path())?;
        let mut state = State::default();
        assert_eq!(state.rows(&ledger), vec![Row::Category("Editing".into())]);
        state.expanded.insert("Editing".into());
        assert!(state.rows(&ledger).contains(&Row::App(0)));
        assert!(!state.rows(&ledger).contains(&Row::App(1)));
        state.archives.insert("Editing".into());
        assert!(state.rows(&ledger).contains(&Row::App(1)));
        state.query = "b".into();
        assert_eq!(state.rows(&ledger), vec![Row::App(1)]);
        Ok(())
    }
}
