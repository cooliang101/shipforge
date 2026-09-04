//! Bounded, local-only candidate search. Choosing a row never activates a command.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use super::presentation::safe_text;

const MAX_CHOICES: usize = 4096;
const MAX_QUERY_CHARS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ChoiceSet {
    pub title: &'static str,
    pub items: Vec<(usize, String)>,
    pub selected: usize,
    pub empty_hint: &'static str,
}

#[derive(Debug)]
pub(super) struct Picker {
    source: ChoiceSet,
    query: String,
    matching: Vec<usize>,
    cursor: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PickerAction {
    Continue,
    Cancel,
    Focus(usize),
}

impl Picker {
    pub fn new(source: ChoiceSet) -> Result<Self, &'static str> {
        if source.items.len() > MAX_CHOICES {
            return Err("Too many choices to search safely. Choose a narrower directory or page.");
        }
        let cursor = source
            .items
            .iter()
            .position(|(index, _)| *index == source.selected)
            .unwrap_or(0);
        let matching = (0..source.items.len()).collect();
        Ok(Self {
            source,
            query: String::new(),
            matching,
            cursor,
        })
    }

    pub fn source_matches(&self, current: &ChoiceSet) -> bool {
        self.source.title == current.title && self.source.items == current.items
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerAction {
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            return PickerAction::Cancel;
        }
        if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            return PickerAction::Continue;
        }
        match key.code {
            KeyCode::Esc => return PickerAction::Cancel,
            KeyCode::Enter if key.modifiers.is_empty() => {
                if let Some(row) = self.matching.get(self.cursor) {
                    return PickerAction::Focus(self.source.items[*row].0);
                }
            }
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => {
                self.cursor = self
                    .cursor
                    .saturating_add(1)
                    .min(self.matching.len().saturating_sub(1));
            }
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(10),
            KeyCode::PageDown => {
                self.cursor = self
                    .cursor
                    .saturating_add(10)
                    .min(self.matching.len().saturating_sub(1));
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.matching.len().saturating_sub(1),
            KeyCode::Backspace => {
                self.query.pop();
                self.filter();
            }
            KeyCode::Delete => {
                self.query.clear();
                self.filter();
            }
            KeyCode::Char(character) if self.query.chars().count() < MAX_QUERY_CHARS => {
                let text = safe_text(&character.to_string());
                if !text.is_empty() {
                    self.query.push_str(&text);
                    self.filter();
                }
            }
            _ => {}
        }
        PickerAction::Continue
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        let words: Vec<_> = query.split_whitespace().collect();
        self.matching = self
            .source
            .items
            .iter()
            .enumerate()
            .filter_map(|(row, (_, label))| {
                let label = safe_text(label).to_lowercase();
                words.iter().all(|word| label.contains(word)).then_some(row)
            })
            .collect();
        self.cursor = 0;
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Find {} (local choices) ", self.source.title));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let parts = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("Query: {}", self.query)),
                Line::from(format!(
                    "{} / {} matching; selection only, no operation is run",
                    self.matching.len(),
                    self.source.items.len()
                )),
            ]),
            parts[0],
        );
        if self.source.items.is_empty() || self.matching.is_empty() {
            let message = if self.source.items.is_empty() {
                self.source.empty_hint
            } else {
                "No matches. Backspace edits; Delete clears; Esc keeps the original selection."
            };
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), parts[1]);
        } else {
            let height = usize::from(parts[1].height).max(1);
            let start = self.cursor.saturating_sub(height.saturating_sub(1));
            let lines: Vec<_> = self
                .matching
                .iter()
                .enumerate()
                .skip(start)
                .take(height)
                .map(|(row, index)| {
                    let selected = row == self.cursor;
                    let prefix = if selected { "> " } else { "  " };
                    let line = Line::from(format!(
                        "{prefix}{}",
                        safe_text(&self.source.items[*index].1)
                    ));
                    if selected {
                        line.style(
                            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
                        )
                    } else {
                        line
                    }
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), parts[1]);
        }
        frame.render_widget(Paragraph::new("Type to filter · ↑/↓ PgUp/PgDn Home/End move\nEnter focus choice · Delete clear · Esc cancel"), parts[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn choices() -> ChoiceSet {
        ChoiceSet {
            title: "Components",
            items: vec![
                (7, "Backend API".into()),
                (42, "生产 Worker".into()),
                (99, "Backend Worker".into()),
            ],
            selected: 42,
            empty_hint: "No candidates. Esc returns to manual setup.",
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn filtered_choices_preserve_original_indexes_and_support_unicode_words() {
        let mut picker = Picker::new(choices()).unwrap();
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            PickerAction::Focus(42)
        );
        for c in "WORKer backend".chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            PickerAction::Focus(99)
        );
        picker.handle_key(key(KeyCode::Delete));
        for c in "生产".chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            PickerAction::Focus(42)
        );
    }

    #[test]
    fn empty_no_matches_cancel_and_modified_confirmation_do_not_select() {
        let mut picker = Picker::new(choices()).unwrap();
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            PickerAction::Continue
        );
        picker.handle_key(key(KeyCode::Char('z')));
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            PickerAction::Continue
        );
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), PickerAction::Cancel);
        let mut source = choices();
        source.items.clear();
        assert_eq!(
            Picker::new(source).unwrap().handle_key(key(KeyCode::Enter)),
            PickerAction::Continue
        );
    }

    #[test]
    fn query_and_choice_count_are_bounded_and_changed_sources_are_rejected() {
        let mut picker = Picker::new(choices()).unwrap();
        for _ in 0..500 {
            picker.handle_key(key(KeyCode::Char('中')));
        }
        assert_eq!(picker.query.chars().count(), MAX_QUERY_CHARS);
        let mut changed = choices();
        changed.items.swap(0, 1);
        assert!(!picker.source_matches(&changed));
        changed.items = (0..=MAX_CHOICES).map(|i| (i, format!("{i}"))).collect();
        assert!(Picker::new(changed).is_err());
    }

    #[test]
    fn narrow_viewport_keeps_last_choice_visible_without_relying_on_color() {
        let mut source = choices();
        source.items = (0..150).map(|i| (i, format!("Component {i}"))).collect();
        let mut picker = Picker::new(source).unwrap();
        picker.handle_key(key(KeyCode::End));
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, frame.area()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("> Component 149"));
        assert!(text.contains("Enter focus choice"));
    }
}
