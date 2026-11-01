use std::io;

use crossterm::{
    cursor::MoveTo,
    event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    style::{Attribute, Print, SetAttribute},
    terminal::{Clear, ClearType},
};

use crate::{threads::renderer::TocEntry, TERMINAL_SIZE};

/* Navigable table of contents overlay */
pub struct Toc {
    pub open: bool,
    entries: Vec<TocEntry>,
    selected: usize,
}

impl Toc {
    pub fn new() -> Self {
        Self {
            open: false,
            entries: Vec::new(),
            selected: 0,
        }
    }

    pub fn set_entries(&mut self, entries: Vec<TocEntry>) {
        self.entries = entries;
        self.selected = usize::min(self.selected, self.entries.len().saturating_sub(1));
    }

    fn visible_rows() -> usize {
        let rows = TERMINAL_SIZE.get().unwrap().read().unwrap().rows as usize;
        usize::max(rows.saturating_sub(2), 1)
    }

    fn move_selection(&mut self, delta: isize) {
        let last = self.entries.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    /* Returns the page to jump to if an entry has been chosen */
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<usize> {
        if key.kind == KeyEventKind::Release {
            return None;
        }

        let page_step = Self::visible_rows() as isize;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'Q' | '/') => self.open = false,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(page_step),
            KeyCode::PageUp => self.move_selection(-page_step),
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(page_step)
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-page_step)
            }
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.entries.len().saturating_sub(1)
            }
            KeyCode::Enter => {
                if let Some(page) = self.entries.get(self.selected).and_then(|e| e.page) {
                    self.open = false;
                    return Some(page);
                }
            }
            _ => (),
        }

        None
    }

    /* Erases the text line by line; clearing the whole screen would make the
     * terminal free the page images that are still needed afterwards */
    pub fn erase() {
        let rows = TERMINAL_SIZE.get().unwrap().read().unwrap().rows;
        let mut out = io::stdout();
        for row in 0..rows {
            execute!(out, MoveTo(0, row), Clear(ClearType::CurrentLine)).unwrap();
        }
    }

    pub fn draw(&self) {
        let (columns, _) = {
            let size = TERMINAL_SIZE.get().unwrap().read().unwrap();
            (size.columns as usize, size.rows as usize)
        };
        let visible = Self::visible_rows();
        let first = if self.selected >= visible {
            self.selected + 1 - visible
        } else {
            0
        };

        let fit = |s: String| -> String { s.chars().take(columns).collect() };
        let mut out = io::stdout();

        Self::erase();
        execute!(
            out,
            MoveTo(0, 0),
            SetAttribute(Attribute::Bold),
            Print(fit(
                "Table of contents  (j/k move, Enter go, Esc close)".to_owned()
            )),
            SetAttribute(Attribute::Reset)
        )
        .unwrap();

        if self.entries.is_empty() {
            execute!(
                out,
                MoveTo(0, 2),
                Print("This document has no table of contents")
            )
            .unwrap();
            return;
        }

        for (row, (i, entry)) in self
            .entries
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .enumerate()
        {
            let page = entry.page.map_or(String::new(), |p| format!("  {}", p + 1));
            let line = fit(format!(
                "{}{}{}",
                "  ".repeat(entry.depth),
                entry.title,
                page
            ));
            let selected = i == self.selected;
            if selected {
                execute!(out, SetAttribute(Attribute::Reverse)).unwrap();
            }
            execute!(
                out,
                MoveTo(0, row as u16 + 1),
                Print(line),
                SetAttribute(Attribute::Reset)
            )
            .unwrap();
        }
    }
}
