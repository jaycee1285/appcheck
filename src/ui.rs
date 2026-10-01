use crate::{
    android::{Catalog as AndroidCatalog, Listing as AndroidListing},
    dialog::Dialog,
    doctor,
    ledger::{AndroidRecord, Disposition, Launch, Ledger, expand_path},
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

#[derive(Clone, Debug, PartialEq)]
enum AndroidRow {
    Category(String),
    Group(String, Option<Disposition>),
    App(usize),
}

#[derive(Clone)]
struct AndroidView {
    catalog: AndroidCatalog,
    selection: ListState,
    expanded: HashSet<String>,
    archives: HashSet<String>,
    detail: Option<usize>,
    query: String,
    searching: bool,
}

impl AndroidView {
    fn new(catalog: AndroidCatalog) -> Self {
        Self { catalog, selection: ListState::default().with_selected(Some(0)),
            expanded: HashSet::new(), archives: HashSet::new(), detail: None,
            query: String::new(), searching: false }
    }

    fn categories(&self) -> Vec<String> {
        let mut names: Vec<_> = self.catalog.apps.iter().filter(|app| !app.unlisted)
            .flat_map(|app| if app.categories.is_empty() { vec!["Uncategorized".to_string()] } else { app.categories.clone() })
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        names.dedup();
        if let Some(index) = names.iter().position(|name| name == "Uncategorized") {
            names.remove(index);
            names.push("Uncategorized".into());
        }
        if self.catalog.apps.iter().any(|app| app.unlisted) { names.push("Unlisted".into()); }
        names
    }

    fn in_category(app: &AndroidListing, category: &str) -> bool {
        if category == "Unlisted" { return app.unlisted; }
        if app.unlisted { return false; }
        if category == "Uncategorized" { app.categories.is_empty() }
        else { app.categories.iter().any(|name| name == category) }
    }

    fn count(&self, category: &str, disposition: Option<Disposition>) -> usize {
        self.catalog.apps.iter().filter(|app| Self::in_category(app, category) && app.disposition == disposition).count()
    }

    fn rows(&self) -> Vec<AndroidRow> {
        if !self.query.is_empty() {
            let mut matches: Vec<_> = self.catalog.apps.iter().enumerate()
                .filter_map(|(i, app)| fuzzy_score(&self.query, &format!("{} {} {} {}", app.name, app.identity, app.categories.join(" "), app.description))
                    .map(|score| (score, i))).collect();
            matches.sort_by_key(|(score, _)| *score);
            return matches.into_iter().map(|(_, i)| AndroidRow::App(i)).collect();
        }
        let mut rows = Vec::new();
        for category in self.categories() {
            rows.push(AndroidRow::Category(category.clone()));
            if !self.expanded.contains(&category) { continue; }
            for disposition in [Some(Disposition::Using), Some(Disposition::Considering), Some(Disposition::Archived), None] {
                let count = self.count(&category, disposition);
                if count == 0 { continue; }
                rows.push(AndroidRow::Group(category.clone(), disposition));
                if disposition == Some(Disposition::Archived) && !self.archives.contains(&category) { continue; }
                let mut indices: Vec<_> = self.catalog.apps.iter().enumerate()
                    .filter(|(_, app)| Self::in_category(app, &category) && app.disposition == disposition)
                    .map(|(i, _)| i).collect();
                indices.sort_by_key(|i| self.catalog.apps[*i].name.to_lowercase());
                rows.extend(indices.into_iter().map(AndroidRow::App));
            }
        }
        rows
    }

    fn focus(&mut self, app: usize) {
        let listing = &self.catalog.apps[app];
        let category = if listing.unlisted { "Unlisted".to_string() }
            else { listing.categories.first().cloned().unwrap_or_else(|| "Uncategorized".into()) };
        self.expanded.insert(category.clone());
        if listing.disposition == Some(Disposition::Archived) { self.archives.insert(category); }
        if let Some(position) = self.rows().iter().position(|row| *row == AndroidRow::App(app)) {
            self.selection.select(Some(position));
        }
    }
}

fn save_android_listing(ledger: &mut Ledger, catalog: &AndroidCatalog, app: usize,
    disposition: Option<Disposition>, description: &str, reason: &str) -> Result<()> {
    let listing = &catalog.apps[app];
    if let Some(index) = listing.note_index {
        return ledger.edit_android(index, disposition, description, reason);
    }
    let file = catalog.file.file_name().and_then(|name| name.to_str()).unwrap_or("Android export");
    let record = AndroidRecord {
        identity: listing.identity.clone(), name: listing.name.clone(),
        categories: listing.categories.clone(), has_category_snapshot: true,
        disposition, description: description.trim().into(), archived_because: reason.trim().into(),
        author: listing.author.clone(), installed_version: listing.installed_version.clone(),
        latest_version: listing.latest_version.clone(), imported_from: file.into(),
        observed_on: file.split("-export-").nth(1).unwrap_or("").chars().take(10).collect(),
    };
    ledger.add_android(&record)
}

enum Mode {
    Browse,
    Help,
    Search,
    Android(AndroidView),
    AndroidEdit {
        view: Box<AndroidView>,
        app: usize,
        field: usize,
        disposition: Option<Disposition>,
        description: String,
        reason: String,
    },
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

fn android_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('A')
        || (key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::SHIFT))
}

fn cycle_disposition(current: Option<Disposition>, forward: bool) -> Option<Disposition> {
    let states = [None, Some(Disposition::Using), Some(Disposition::Considering), Some(Disposition::Archived)];
    let position = states.iter().position(|state| *state == current).unwrap_or(0);
    states[(position + if forward { 1 } else { states.len() - 1 }) % states.len()]
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
        Mode::AndroidEdit { .. } => 11,
        Mode::Archive { .. } => 9,
        Mode::Help => 21,
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

fn draw_android(frame: &mut Frame, view: &mut AndroidView, message: &str) {
    let apps = &view.catalog.apps;
    let total = apps.iter().filter(|app| !app.unlisted).count();
    let unlisted = apps.len() - total;
    let counts = [Some(Disposition::Using), Some(Disposition::Considering), Some(Disposition::Archived), None]
        .map(|status| apps.iter().filter(|app| !app.unlisted && app.disposition == status).count());
    let rows = view.rows();
    view.selection.select(if rows.is_empty() { None } else { Some(view.selection.selected().unwrap_or(0).min(rows.len() - 1)) });
    let areas = Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(2)]).split(frame.area());
    let heading = Line::from(vec![
        Span::styled(" Android Track", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(" · {total} apps  U {} | C {} | A {} | Untagged {} | Unlisted {unlisted}", counts[0], counts[1], counts[2], counts[3])),
    ]);
    frame.render_widget(Paragraph::new(heading).block(Block::default().borders(Borders::BOTTOM)), areas[0]);
    if let Some(index) = view.detail {
        let app = &apps[index];
        let categories = if app.categories.is_empty() { "Uncategorized".into() } else { app.categories.join(", ") };
        let status = app.disposition.map_or("Untagged", Disposition::label);
        let text = format!(
            "Status: {status}{}\nCategory: {categories}\nDescription: {}\nArchive reason: {}\n\nPackage: {}\nAuthor: {}\nInstalled: {}\nLatest: {}\nSource: {}\nExport: {}",
            if app.unlisted { " · Unlisted" } else { "" }, app.description, app.archived_because,
            app.identity.trim_start_matches("android:"), app.author, app.installed_version,
            app.latest_version, if app.url.is_empty() { "saved snapshot" } else { &app.url },
            view.catalog.file.file_name().and_then(|name| name.to_str()).unwrap_or("unknown"),
        );
        frame.render_widget(Paragraph::new(text).block(Block::bordered().title(format!(" {} ", app.name)))
            .wrap(Wrap { trim: false }), areas[1]);
    } else {
        let items: Vec<_> = rows.iter().map(|row| {
            let line = match row {
                AndroidRow::Category(category) => {
                    let totals = [
                        view.count(category, Some(Disposition::Using)),
                        view.count(category, Some(Disposition::Considering)),
                        view.count(category, Some(Disposition::Archived)),
                        view.count(category, None),
                    ];
                    let mut cells = vec![Span::styled(
                        format!("{} {}", if view.expanded.contains(category) { "▾" } else { "▸" }, fit(category, 24)),
                        Style::default().add_modifier(Modifier::BOLD),
                    )];
                    cells.extend(count_cells([totals[0], totals[1], totals[2]], 2, false));
                    cells.push(Span::styled(format!("{:>3} ○", totals[3]), Style::default()));
                    Line::from(cells)
                }
                AndroidRow::Group(category, status) => {
                    let (label, tint) = match status {
                        Some(Disposition::Using) => ("Using", Color::Green),
                        Some(Disposition::Considering) => ("Considering", Color::Yellow),
                        Some(Disposition::Archived) => ("Archived", Color::Red),
                        None => ("Untagged", Color::Reset),
                    };
                    let count = view.count(category, *status);
                    let fold = if *status == Some(Disposition::Archived) {
                        if view.archives.contains(category) { "▾ " } else { "▸ " }
                    } else { "" };
                    Line::styled(format!("  {fold}{label} {count}"), Style::default().fg(tint).add_modifier(Modifier::BOLD))
                }
                AndroidRow::App(index) => {
                    let app = &apps[*index];
                    let (marker, tint) = match app.disposition {
                        Some(status) => ("●", color(status)), None => ("○", Color::Reset),
                    };
                    Line::from(vec![
                        Span::styled(format!("  {marker} "), Style::default().fg(tint)),
                        Span::raw(&app.name),
                        Span::raw(if view.query.is_empty() && !app.unlisted { String::new() }
                            else { format!("  · {}", if app.categories.is_empty() { "Uncategorized".into() } else { app.categories.join(", ") }) }),
                        Span::styled(if app.description.is_empty() { String::new() } else { format!("  — {}", app.description) },
                            Style::default().add_modifier(Modifier::DIM)),
                    ])
                }
            };
            ListItem::new(line)
        }).collect();
        frame.render_stateful_widget(List::new(items)
            .block(Block::default().title(if view.query.is_empty() { "Categories · → expand · Enter inspect".into() }
                else { format!("/{} · {} matches", view.query, rows.len()) }))
            .highlight_symbol("› ").highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            areas[1], &mut view.selection);
    }
    let footer = if view.searching { format!("/{}▏ · Enter results · Esc clear", view.query) }
        else if !message.is_empty() { message.into() }
        else if view.detail.is_some() { "e edit · Esc categories · r reload".into() }
        else { "↑↓ move · →← fold · Enter inspect · U/C/0 · x archive\ne edit · / search · r reload · Esc desktop".into() };
    frame.render_widget(Paragraph::new(footer), areas[2]);
}

fn android_editor(frame: &mut Frame, record: &AndroidListing, field: usize, disposition: Option<Disposition>, description: &str, reason: &str) {
    let label = disposition.map_or("Untagged", Disposition::label);
    let width = frame.area().width.saturating_sub(6) as usize;
    popup(frame, &format!(" Android · {} ", record.name), vec![
        Line::raw("Status (←/→ or U/C/A/0)"),
        Line::raw(fit(&format!("{} {label}", if field == 0 { "›" } else { " " }), width)),
        Line::raw("Description"),
        Line::raw(fit(&format!("{} {description}{}", if field == 1 { "›" } else { " " }, if field == 1 { "▏" } else { "" }), width)),
        Line::raw("Archive reason"),
        Line::raw(fit(&format!("{} {reason}{}", if field == 2 { "›" } else { " " }, if field == 2 { "▏" } else { "" }), width)),
        Line::raw(""),
        Line::raw("Tab fields · Enter save · Esc cancel"),
    ], 11);
}

fn draw(frame: &mut Frame, ledger: &Ledger, state: &mut State, rows: &[Row]) {
    let (min_width, min_height) = minimum_size(ledger, state);
    if frame.area().width < min_width || frame.area().height < min_height {
        frame.render_widget(Paragraph::new(format!("Track\nNeed {min_width} columns × {min_height} rows\nResize to continue\nq quit · Esc back")).wrap(Wrap { trim: false }), frame.area());
        return;
    }
    match &mut state.mode {
        Mode::Android(view) => {
            draw_android(frame, view, &state.message);
            return;
        }
        Mode::AndroidEdit { view, app, field, disposition, description, reason } => {
            draw_android(frame, view, "");
            android_editor(frame, &view.catalog.apps[*app], *field, *disposition, description, reason);
            return;
        }
        _ => {}
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
        Paragraph::new("↑↓ move  →← fold  Enter inspect\na add  Shift-A Android  e edit  c check  r release  i install  / search  ? help")
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
                Line::raw("a          Add / complete recipe"),
                Line::raw("Shift-A    Android notes"),
                Line::raw("e          Edit / review source"),
                Line::raw("c          Check all source routes"),
                Line::raw("r          Check release artifacts"),
                Line::raw("U / C      Using / Considering"),
                Line::raw("x          Archive with reason"),
                Line::raw("o          Launch"),
                Line::raw("u / i      Update / install selected"),
                Line::raw("n / m      Nix check / migrate"),
                Line::raw("g / G      Update Using / + Considering"),
                Line::raw("R          Reload ledger"),
                Line::raw("q          Back / quit"),
                Line::raw("Esc / ?    Close help"),
            ],
            21,
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
        Mode::Android(_) | Mode::AndroidEdit { .. } | Mode::Browse => {}
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
        Mode::Android(view) => {
            if view.searching {
                match key.code {
                    KeyCode::Esc => { view.query.clear(); view.searching = false; }
                    KeyCode::Enter => view.searching = false,
                    KeyCode::Backspace => { view.query.pop(); }
                    KeyCode::Char(c) => view.query.push(c),
                    _ => {}
                }
                view.selection.select(Some(0));
                return Ok(false);
            }
            let rows = view.rows();
            let selected = view.selection.selected().unwrap_or(0).min(rows.len().saturating_sub(1));
            let app = view.detail.or_else(|| match rows.get(selected) { Some(AndroidRow::App(i)) => Some(*i), _ => None });
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    if view.detail.take().is_some() {}
                    else if !view.query.is_empty() { view.query.clear(); view.selection.select(Some(0)); }
                    else { state.mode = Mode::Browse; }
                }
                KeyCode::Char('/') if view.detail.is_none() => {
                    view.query.clear(); view.searching = true;
                }
                KeyCode::Char('r') => {
                    *ledger = Ledger::open(&ledger.path)?;
                    view.catalog = AndroidCatalog::load(ledger)?;
                    state.message = "Android export and TOML reloaded.".into();
                }
                KeyCode::Char('e' | 'x') if app.is_some() => {
                    let index = app.unwrap();
                    let listing = &view.catalog.apps[index];
                    let archive = key.code == KeyCode::Char('x');
                    state.mode = Mode::AndroidEdit {
                        view: Box::new(view.clone()), app: index, field: if archive { 2 } else { 1 },
                        disposition: if archive { Some(Disposition::Archived) } else { listing.disposition },
                        description: listing.description.clone(), reason: listing.archived_because.clone(),
                    };
                }
                KeyCode::Char('U' | 'C' | '0') if app.is_some() => {
                    let index = app.unwrap();
                    let listing = &view.catalog.apps[index];
                    let name = listing.name.clone();
                    let status = match key.code {
                        KeyCode::Char('U') => Some(Disposition::Using),
                        KeyCode::Char('C') => Some(Disposition::Considering),
                        _ => None,
                    };
                    save_android_listing(ledger, &view.catalog, index, status, &listing.description, &listing.archived_because)?;
                    view.catalog = AndroidCatalog::from_export(ledger, &view.catalog.file)?;
                    view.focus(index);
                    state.message = format!("{name} status saved.");
                }
                KeyCode::Up | KeyCode::Char('k') if view.detail.is_none() => view.selection.select(Some(selected.saturating_sub(1))),
                KeyCode::Down | KeyCode::Char('j') if view.detail.is_none() => view.selection.select(Some((selected + 1).min(rows.len().saturating_sub(1)))),
                KeyCode::PageUp if view.detail.is_none() => view.selection.select(Some(selected.saturating_sub(10))),
                KeyCode::PageDown if view.detail.is_none() => view.selection.select(Some((selected + 10).min(rows.len().saturating_sub(1)))),
                KeyCode::Right | KeyCode::Enter if view.detail.is_none() => match rows.get(selected) {
                    Some(AndroidRow::Category(category)) => { view.expanded.insert(category.clone()); }
                    Some(AndroidRow::Group(category, Some(Disposition::Archived))) => {
                        if !view.archives.insert(category.clone()) { view.archives.remove(category); }
                    }
                    Some(AndroidRow::App(index)) => view.detail = Some(*index),
                    _ => {}
                },
                KeyCode::Left if view.detail.is_none() => {
                    if !view.query.is_empty() { view.query.clear(); view.selection.select(Some(0)); }
                    else if let Some(row) = rows.get(selected) {
                        let category = match row {
                            AndroidRow::Category(category) | AndroidRow::Group(category, _) => category.clone(),
                            AndroidRow::App(index) => {
                                let listing = &view.catalog.apps[*index];
                                if listing.unlisted { "Unlisted".into() }
                                else { listing.categories.first().cloned().unwrap_or_else(|| "Uncategorized".into()) }
                            }
                        };
                        view.expanded.remove(&category);
                        view.archives.remove(&category);
                        view.selection.select(view.rows().iter().position(|row| *row == AndroidRow::Category(category.clone())));
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        Mode::AndroidEdit { view, app, field, disposition, description, reason } => {
            match key.code {
                KeyCode::Esc => state.mode = Mode::Android((**view).clone()),
                KeyCode::Tab => *field = (*field + 1) % 3,
                KeyCode::BackTab => *field = (*field + 2) % 3,
                KeyCode::Left if *field == 0 => *disposition = cycle_disposition(*disposition, false),
                KeyCode::Right if *field == 0 => *disposition = cycle_disposition(*disposition, true),
                KeyCode::Char('U') if *field == 0 => *disposition = Some(Disposition::Using),
                KeyCode::Char('C') if *field == 0 => *disposition = Some(Disposition::Considering),
                KeyCode::Char('A') if *field == 0 => *disposition = Some(Disposition::Archived),
                KeyCode::Char('0') if *field == 0 => *disposition = None,
                KeyCode::Backspace if *field == 1 => { description.pop(); }
                KeyCode::Backspace if *field == 2 => { reason.pop(); }
                KeyCode::Char(c) if *field == 1 => description.push(c),
                KeyCode::Char(c) if *field == 2 => reason.push(c),
                KeyCode::Enter => {
                    let index = *app;
                    save_android_listing(ledger, &view.catalog, index, *disposition, description, reason)?;
                    let mut returned = (**view).clone();
                    returned.catalog = AndroidCatalog::from_export(ledger, &returned.catalog.file)?;
                    returned.detail = None;
                    returned.focus(index);
                    state.mode = Mode::Android(returned);
                    state.message = "Android note saved to TOML.".into();
                }
                _ => {}
            }
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
                    state.message = "Already tracked; opened existing record. Use e to review its source.".into();
                } else {
                    ledger.add(&saved[0], &saved[1], &saved[2], &saved[3], &saved[4])?;
                    let index = ledger.find(identity)?;
                    state.query = saved[0].trim().into();
                    state.selection.select(Some(0));
                    if !record_only && ledger.apps[index].identity.starts_with("https://github.com/") {
                        state.mode = Mode::Task(Box::new(crate::ui_task::Task::start(
                            ledger.clone(),
                            move |ledger, dialog| crate::intake::configure_source(ledger, index, dialog),
                        )));
                    } else {
                        state.message = "Added to Considering; installation remains unknown. Use e to review its source.".into();
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
        _ if android_key(key) => {
            state.mode = Mode::Android(AndroidView::new(AndroidCatalog::load(ledger)?));
            state.message.clear();
        }
        KeyCode::Char('a') => {
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
        KeyCode::Char('c' | 'r') if app.is_some() => {
            let index = app.unwrap();
            let release_only = key.code == KeyCode::Char('r');
            state.mode = Mode::Task(Box::new(crate::ui_task::Task::start(
                ledger.clone(),
                move |ledger, dialog| if release_only {
                    crate::intake::release_check(ledger, index, dialog)
                } else {
                    crate::intake::configure_source(ledger, index, dialog)
                },
            )));
        }
        KeyCode::Char('R') => {
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
                Mode::AndroidEdit { field: 1, description, .. } => description.push_str(&text),
                Mode::AndroidEdit { field: 2, reason, .. } => reason.push_str(&text),
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
    fn android_view_uses_saved_categories_and_fits_the_quarter_tile() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        std::fs::write(&path, "schema_version = 1\n[[inbox.android]]\nidentity = 'android:one.app'\nname = 'Old name'\ndisplay_name = 'New name'\ndisplay_categories = '[\"Tools\",\"Daily\"]'\ndisposition = 'using'\ndescription = 'My note'\n")?;
        let export = dir.path().join("obtainx-export-test.json");
        std::fs::write(&export, r#"{"apps":[{"id":"one.app","name":"New name","categories":["Tools","Daily"]},{"id":"two.app","name":"Second","categories":["Tools"]}]}"#)?;
        let ledger = Ledger::open(&path)?;
        let mut state = State::default();
        let mut view = AndroidView::new(AndroidCatalog::from_export(&ledger, &export)?);
        view.expanded.insert("Tools".into());
        state.mode = Mode::Android(view);
        let list = render(&ledger, &mut state, 87, 22)?.join("\n");
        assert!(list.contains("Android Track · 2 apps"), "{list}");
        assert!(list.contains("New name"), "{list}");
        assert!(list.contains("Second"), "{list}");
        assert!(!list.contains("Resize to continue"), "{list}");
        assert!(android_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)));
        assert!(!android_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
        Ok(())
    }

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
        assert!(text.contains("c          Check all source routes"), "{text}");
        assert!(text.contains("r          Check release artifacts"), "{text}");
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
