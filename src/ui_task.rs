use crate::{dialog, ledger::Ledger};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

struct Prompt {
    label: String,
    input: String,
    choices: Vec<String>,
    selected: Option<usize>,
    reply: mpsc::Sender<Option<String>>,
}

/// Rows a wrapped paragraph occupies, counted the way `Wrap { trim: false }`
/// breaks: greedily on whitespace, splitting only words wider than the area.
fn wrapped_lines(text: &str, width: usize) -> u16 {
    let width = width.max(1);
    let mut rows: usize = 0;
    for line in text.split('\n') {
        let mut column = 0;
        let mut wrote = false;
        for word in line.split_inclusive(' ') {
            let cells = unicode_width::UnicodeWidthStr::width(word);
            if wrote && column + cells > width {
                rows += 1;
                column = 0;
            }
            wrote = true;
            column += cells;
            while column > width {
                rows += 1;
                column -= width;
            }
        }
        rows += 1;
    }
    rows.min(u16::MAX as usize) as u16
}

fn wrapped(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut column = 0;
    for c in text.chars() {
        let cells = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if column + cells > width.max(1) {
            out.push('\n');
            column = 0;
        }
        out.push(c);
        column += cells;
    }
    out
}

pub struct Task {
    rx: mpsc::Receiver<dialog::Event>,
    cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    prompt: Option<Prompt>,
    body: String,
    failures: Vec<String>,
    status: String,
    scroll: u16,
    /// Body rows and visible body height from the last draw, so PgUp/PgDn can
    /// clamp against what is actually on screen after a resize.
    body_rows: u16,
    body_view: u16,
    /// Wall clock since the task started, and the whole second last drawn.
    /// A network call can sit silent for seconds; without a ticking figure a
    /// working task is indistinguishable from a hung one.
    started: std::time::Instant,
    shown_secs: u64,
    pub done: bool,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Task {
    pub fn start(
        mut ledger: Ledger,
        run: impl FnOnce(&mut Ledger, &dialog::Bridge) -> anyhow::Result<String> + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let bridge = dialog::Bridge {
            tx: tx.clone(),
            cancel: cancel.clone(),
        };
        let worker = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(&mut ledger, &bridge)
            }))
            .map_err(|_| {
                "Worker stopped unexpectedly; reload the ledger before continuing.".to_string()
            })
            .and_then(|r| r.map_err(|e| format!("{e:#}")));
            let _ = tx.send(dialog::Event::Done { ledger, result });
        });
        Self {
            rx,
            cancel,
            worker: Some(worker),
            prompt: None,
            body: String::new(),
            failures: vec![],
            status: "Starting…".into(),
            scroll: 0,
            body_rows: 0,
            body_view: 0,
            started: std::time::Instant::now(),
            shown_secs: 0,
            done: false,
        }
    }

    pub fn poll(&mut self, ledger: &mut Ledger) -> bool {
        let mut changed = false;
        // Redraw once a second while work is in flight, so the elapsed figure
        // advances even when the worker sends nothing.
        let secs = self.started.elapsed().as_secs();
        if !self.done && secs != self.shown_secs {
            self.shown_secs = secs;
            changed = true;
        }
        while let Ok(event) = self.rx.try_recv() {
            changed = true;
            match event {
                dialog::Event::Message(text) => {
                    if text.contains(" failed:") {
                        self.failures.push(text.clone());
                    }
                    if text.trim().contains('\n') {
                        self.body = text.trim().into();
                        self.scroll = 0;
                    } else {
                        self.status = text.trim().into();
                    }
                }
                dialog::Event::Prompt {
                    label,
                    default,
                    choices,
                    reply,
                } => {
                    if self.cancel.load(Ordering::Acquire) {
                        let _ = reply.send(None);
                        continue;
                    }
                    let selected = if choices.is_empty() {
                        None
                    } else {
                        default.parse().ok()
                    };
                    self.prompt = Some(Prompt {
                        label,
                        input: if choices.is_empty() {
                            default
                        } else {
                            String::new()
                        },
                        choices,
                        selected,
                        reply,
                    });
                    self.status.clear();
                }
                dialog::Event::Done {
                    ledger: updated,
                    result,
                } => {
                    *ledger = updated;
                    self.prompt = None;
                    self.done = true;
                    let (text, status) = match result {
                        Ok(s) => (s, "Complete."),
                        Err(e) => (format!("Failed\n{e}"), "Failed."),
                    };
                    if !self.failures.is_empty() {
                        self.body
                            .push_str(&format!("\n\nFailures\n{}", self.failures.join("\n\n")));
                    }
                    self.body.push_str(&format!("\n\n{text}"));
                    self.status = status.into();
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                }
            }
        }
        changed
    }

    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(prompt) = self.prompt.take() {
            let _ = prompt.reply.send(None);
        }
        self.status = "Cancelling; waiting for in-flight work…".into();
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(prompt) = &mut self.prompt {
            if prompt.choices.is_empty() {
                prompt
                    .input
                    .extend(text.chars().filter(|c| !c.is_control()));
            }
        }
    }

    pub fn key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::PageUp {
            self.scroll = self.scroll.saturating_sub(8);
            return;
        }
        if key.code == KeyCode::PageDown {
            self.scroll = self.scroll.saturating_add(8).min(self.max_scroll());
            return;
        }
        if key.code == KeyCode::Esc && !self.done {
            self.cancel();
            return;
        }
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        match key.code {
            KeyCode::Down if !prompt.choices.is_empty() => {
                prompt.selected = Some(
                    prompt
                        .selected
                        .map_or(0, |i| (i + 1).min(prompt.choices.len() - 1)),
                )
            }
            KeyCode::Up if !prompt.choices.is_empty() => {
                prompt.selected = Some(prompt.selected.map_or(0, |i| i.saturating_sub(1)))
            }
            KeyCode::Char(c) if prompt.choices.is_empty() => prompt.input.push(c),
            KeyCode::Backspace if prompt.choices.is_empty() => {
                prompt.input.pop();
            }
            KeyCode::Enter => {
                let answer = if prompt.choices.is_empty() {
                    Some(prompt.input.clone())
                } else {
                    prompt.selected.map(|i| i.to_string())
                };
                if let Some(answer) = answer {
                    let prompt = self.prompt.take().unwrap();
                    let _ = prompt.reply.send(Some(answer));
                    self.status = "Working…".into();
                }
            }
            _ => {}
        }
    }

    /// Last row the body can scroll to, so the end of a plan is reachable and
    /// scrolling cannot run past it into blank space.
    fn max_scroll(&self) -> u16 {
        self.body_rows.saturating_sub(self.body_view)
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let width = area.width.saturating_sub(2).min(92);
        // One row of margin above and below, and never taller than the terminal.
        let room = area.height.saturating_sub(2).max(1);
        let control_height = self.prompt.as_ref().map_or(0, |p| {
            if p.choices.is_empty() {
                4
            } else {
                (p.choices.len() as u16 + 2).min((room.saturating_sub(2) / 2).max(4))
            }
        });
        // status + footer are two rows each; the border takes the other two.
        let furniture = control_height + 4;
        self.body_rows = wrapped_lines(&self.body, width.saturating_sub(2) as usize);
        let height = (self.body_rows.saturating_add(furniture).saturating_add(2))
            .min(room)
            .max(furniture.saturating_add(3).min(room));
        let rect = Rect::new(
            (area.width - width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, rect);
        let title = if self.done {
            " Track · result "
        } else {
            " Track · action "
        };
        let block = Block::bordered().title(title);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let areas = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(control_height),
            Constraint::Length(2),
            Constraint::Length(2),
        ])
        .split(inner);
        // A resize can shrink the viewport under a scrolled body; re-clamp
        // before rendering so the plan cannot scroll off into blank space.
        self.body_view = areas[0].height;
        self.scroll = self.scroll.min(self.max_scroll());
        frame.render_widget(
            Paragraph::new(self.body.as_str())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            areas[0],
        );
        if let Some(prompt) = &self.prompt {
            let block = Block::bordered().title(prompt.label.as_str());
            if prompt.choices.is_empty() {
                let room = areas[1].width.saturating_sub(3) as usize;
                let mut tail = String::new();
                let mut width = 0;
                for c in prompt.input.chars().rev() {
                    width += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if width > room {
                        break;
                    }
                    tail.insert(0, c);
                }
                frame.render_widget(Paragraph::new(format!("{tail}▏")).block(block), areas[1]);
            } else {
                let items: Vec<_> = prompt
                    .choices
                    .iter()
                    .map(|s| ListItem::new(wrapped(s, areas[1].width.saturating_sub(4) as usize)))
                    .collect();
                let mut state = ListState::default().with_selected(prompt.selected);
                frame.render_stateful_widget(
                    List::new(items)
                        .block(block)
                        .highlight_symbol("› ")
                        .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
                    areas[1],
                    &mut state,
                );
            }
        }
        let status = if self.done || self.status.is_empty() {
            self.status.clone()
        } else {
            format!("{}  ({}s)", self.status, self.shown_secs)
        };
        frame.render_widget(
            Paragraph::new(status).wrap(Wrap { trim: false }),
            areas[2],
        );
        frame.render_widget(
            Paragraph::new(if self.done {
                "Enter → Track · Esc → Bash\nPgUp/PgDn review"
            } else {
                "↑↓ select · Enter confirm\nPgUp/PgDn review · Esc cancel"
            }),
            areas[3],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_rows_count_word_breaks_and_oversized_words() {
        assert_eq!(wrapped_lines("", 20), 1);
        assert_eq!(wrapped_lines("short", 20), 1);
        assert_eq!(wrapped_lines("one\ntwo\nthree", 20), 3);
        // Wraps at the space rather than mid-word.
        assert_eq!(wrapped_lines("aaaa bbbb", 6), 2);
        // A word wider than the area is split across rows.
        assert_eq!(wrapped_lines("aaaaaaaaaaaa", 4), 3);
        // A blank trailing line is still a row.
        assert_eq!(wrapped_lines("a\n", 4), 2);
        assert!(wrapped_lines("wide  ", 1) >= 6);
    }

    #[test]
    fn scroll_stops_at_the_end_of_the_body_and_follows_a_shrinking_viewport() {
        let mut rows = 40;
        let mut view = 10;
        let max = |rows: u16, view: u16| rows.saturating_sub(view);
        assert_eq!(max(rows, view), 30);
        // A body shorter than the viewport cannot scroll at all.
        rows = 4;
        assert_eq!(max(rows, view), 0);
        // Shrinking the viewport exposes more scroll, never negative.
        rows = 40;
        view = 3;
        assert_eq!(max(rows, view), 37);
        view = 100;
        assert_eq!(max(rows, view), 0);
    }
}
