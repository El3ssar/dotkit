//! Shared event loop and terminal restoration for the file and package managers.
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Stylize,
    text::Line,
    widgets::{Block, Paragraph},
    Frame,
};

/// Keep browseable rows and apply/cancel affordances visible in small SSH panes.
pub fn panels(area: Rect, package: bool) -> [Rect; 5] {
    let compact = area.width < 90 || area.height < 23;
    Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(if compact {
            3
        } else if package {
            6
        } else {
            4
        }),
        Constraint::Length(if compact { 3 } else { 5 }),
    ])
    .areas(area)
}

/// Wrap help/review text by display width so scrolling has a precise, stable end.
/// This avoids Ratatui's optional unstable rendered-line-count feature.
fn wrapped_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut occupied = 0;
        for word in paragraph.split_inclusive(char::is_whitespace) {
            let word_width = Line::from(word).width();
            if word_width <= width && occupied > 0 && occupied + word_width > width {
                lines.push(Line::from(std::mem::take(&mut line)));
                occupied = 0;
            }
            for ch in word.chars() {
                let cell_width = Line::from(ch.to_string()).width();
                if occupied > 0 && occupied + cell_width > width {
                    lines.push(Line::from(std::mem::take(&mut line)));
                    occupied = 0;
                }
                line.push(ch);
                occupied += cell_width;
            }
        }
        lines.push(Line::from(line));
    }
    lines
}

pub fn overlay(frame: &mut Frame, title: &str, text: &str, scroll: &mut u16) {
    let area = frame.area();
    let lines = wrapped_lines(text, area.width.saturating_sub(2) as usize);
    let max_scroll = lines
        .len()
        .saturating_sub(area.height.saturating_sub(2) as usize)
        .min(u16::MAX as usize) as u16;
    *scroll = (*scroll).min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((*scroll, 0))
            .block(Block::bordered().title(title).cyan()),
        area,
    );
}

use std::io;

pub enum Outcome {
    Apply,
    Cancel,
}

pub trait Editor {
    fn render(&mut self, frame: &mut Frame);
    fn key(&mut self, key: KeyEvent) -> Option<Outcome>;
}

struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

pub fn run(app: &mut impl Editor) -> io::Result<bool> {
    let _restore = RestoreTerminal;
    let mut terminal = ratatui::try_init()?;
    loop {
        terminal.draw(|frame| app.render(frame))?;
        loop {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    match app.key(key) {
                        Some(Outcome::Apply) => return Ok(true),
                        Some(Outcome::Cancel) => return Ok(false),
                        None => break,
                    }
                }
                Event::Resize(_, _) => break,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    #[test]
    fn overlay_end_stops_at_content_and_resizing_keeps_text_visible() {
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        let text = format!(
            "{}\nTAIL",
            "many words wrapping on a narrow terminal\n".repeat(30)
        );
        let mut scroll = u16::MAX;
        terminal
            .draw(|frame| overlay(frame, "Review", &text, &mut scroll))
            .unwrap();
        assert!(scroll < u16::MAX);
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("TAIL"));
        terminal.backend_mut().resize(80, 60);
        terminal.resize(Rect::new(0, 0, 80, 60)).unwrap();
        terminal
            .draw(|frame| overlay(frame, "Review", &text, &mut scroll))
            .unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("TAIL"));
    }
    #[test]
    fn wrapping_preserves_unicode_and_long_paths() {
        let input = "説明 foo/bar/baz/some-long-file-name\n\nlast";
        let lines = wrapped_lines(input, 8);
        assert!(lines.iter().all(|line| line.width() <= 8));
        assert_eq!(
            lines
                .iter()
                .map(|line| line.to_string())
                .collect::<String>(),
            input.replace('\n', "")
        );
    }
}
