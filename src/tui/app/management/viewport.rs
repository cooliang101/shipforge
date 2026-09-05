//! Logical-line viewport for complete management evidence, independent of u16 offsets.

use std::sync::{Arc, OnceLock};

use crossterm::event::KeyCode;
use ratatui::{style::Style, text::Span};

#[derive(Clone, Debug, Default)]
pub(super) struct Viewport {
    top: usize,
    from_end: bool,
    horizontal: usize,
    document: Arc<OnceLock<Document>>,
}

pub(super) struct Document {
    pub(super) title: &'static str,
    text: String,
    starts: Vec<usize>,
    columns: usize,
}

impl std::fmt::Debug for Document {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Document")
            .field("title", &self.title)
            .field("bytes", &self.text.len())
            .field("lines", &self.starts.len())
            .finish_non_exhaustive()
    }
}

impl Document {
    pub(super) fn new(title: &'static str, text: String) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.match_indices('\n')
                .map(|(offset, _)| offset + 1)
                .filter(|offset| *offset < text.len()),
        );
        let columns = text
            .lines()
            .map(|line| Span::raw(line).styled_graphemes(Style::default()).count())
            .max()
            .unwrap_or(0);
        Self {
            title,
            text,
            starts,
            columns,
        }
    }

    pub(super) fn window(&self, top: usize, horizontal: usize, height: u16, width: u16) -> String {
        let offset = self.starts[top.min(self.starts.len().saturating_sub(1))];
        self.text[offset..]
            .lines()
            .take(usize::from(height))
            .map(|line| {
                let mut remaining = usize::from(width);
                Span::raw(line)
                    .styled_graphemes(Style::default())
                    .skip(horizontal)
                    .take_while(|grapheme| {
                        let cells = Span::raw(grapheme.symbol).width();
                        if cells > remaining {
                            return false;
                        }
                        remaining -= cells;
                        true
                    })
                    .map(|grapheme| grapheme.symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn lines(&self) -> usize {
        self.starts.len()
    }
}

impl Viewport {
    pub(super) fn document(
        &self,
        build: impl FnOnce() -> (&'static str, String, Option<usize>),
    ) -> &Document {
        self.document.get_or_init(|| {
            let (title, text, _) = build();
            Document::new(title, text)
        })
    }

    pub(super) fn top(&self, document: &Document) -> usize {
        let last = document.lines().saturating_sub(1);
        if self.from_end {
            last.saturating_sub(self.top)
        } else {
            self.top.min(last)
        }
    }

    pub(super) fn horizontal(&self) -> usize {
        self.horizontal
    }

    pub(super) fn handle_key(&mut self, key: KeyCode) {
        let last = self
            .document
            .get()
            .map_or(usize::MAX, |document| document.lines().saturating_sub(1));
        self.top = self.top.min(last);
        // Dynamic lists also support panning. Their bounded rows are not cached;
        // cap accidental repeated key presses without imposing a u16 limit.
        let columns = self
            .document
            .get()
            .map_or(1_048_576, |document| document.columns.saturating_sub(1));
        match key {
            KeyCode::Up if self.from_end => self.top = self.top.saturating_add(1).min(last),
            KeyCode::Down if self.from_end => self.top = self.top.saturating_sub(1),
            KeyCode::PageUp if self.from_end => self.top = self.top.saturating_add(10).min(last),
            KeyCode::PageDown if self.from_end => self.top = self.top.saturating_sub(10),
            KeyCode::Up => self.top = self.top.saturating_sub(1),
            KeyCode::Down => self.top = self.top.saturating_add(1).min(last),
            KeyCode::PageUp => self.top = self.top.saturating_sub(10),
            KeyCode::PageDown => self.top = self.top.saturating_add(10).min(last),
            KeyCode::Home => {
                self.top = 0;
                self.from_end = false;
                self.horizontal = 0;
            }
            KeyCode::End => {
                self.top = 0;
                self.from_end = true;
                self.horizontal = 0;
            }
            KeyCode::Char('[') => self.horizontal = self.horizontal.saturating_sub(32),
            KeyCode::Char(']') => self.horizontal = self.horizontal.saturating_add(32).min(columns),
            KeyCode::Char('0') => self.horizontal = 0,
            KeyCode::Char(',') => self.horizontal = self.horizontal.saturating_sub(1),
            KeyCode::Char('.') => self.horizontal = self.horizontal.saturating_add(1).min(columns),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_reaches_evidence_beyond_u16_without_copying_the_whole_document() {
        let mut view = Viewport::default();
        let text = format!("{}TAIL-EVIDENCE", "row\n".repeat(70_000));
        view.document(|| ("Evidence", text, None));
        view.handle_key(KeyCode::End);
        let doc = view.document(|| panic!("immutable evidence must be cached"));
        assert_eq!(view.top(doc), 70_000);
        assert_eq!(doc.window(view.top(doc), 0, 5, 80), "TAIL-EVIDENCE");
        view.handle_key(KeyCode::Home);
        assert_eq!(view.top(view.document(|| unreachable!())), 0);
    }

    #[test]
    fn viewport_preserves_graphemes_and_limits_visible_columns() {
        let doc = Document::new("Evidence", "a界e\u{301}🙂Z".into());
        assert_eq!(doc.window(0, 1, 1, 3), "界e\u{301}");
        assert_eq!(doc.window(0, 2, 1, 3), "e\u{301}🙂");
        assert_eq!(doc.window(0, 3, 1, 1), "");
        assert_eq!(doc.window(0, 0, 0, 80), "");
    }

    #[test]
    fn fine_panning_reaches_each_grapheme_even_in_a_one_column_window() {
        let mut view = Viewport::default();
        view.document(|| ("Evidence", "ABe\u{301}CD".into(), None));
        for expected in ["A", "B", "e\u{301}", "C", "D"] {
            let doc = view.document(|| unreachable!());
            assert_eq!(doc.window(0, view.horizontal(), 1, 1), expected);
            view.handle_key(KeyCode::Char('.'));
        }
        view.handle_key(KeyCode::Char(','));
        assert_eq!(view.horizontal(), 3);
        view.handle_key(KeyCode::Char('0'));
        assert_eq!(view.horizontal(), 0);
    }

    #[test]
    fn cloned_view_retains_position_and_new_default_drops_cached_page() {
        let mut view = Viewport::default();
        view.document(|| ("Evidence", format!("start\n{}tail", "x".repeat(96)), None));
        view.handle_key(KeyCode::Down);
        view.handle_key(KeyCode::Char(']'));
        let previous = view.clone();
        let doc = previous.document(|| unreachable!());
        assert_eq!(previous.top(doc), 1);
        assert_eq!(previous.horizontal(), 32);
        assert!(Arc::ptr_eq(&view.document, &previous.document));
        let next = Viewport::default();
        assert_eq!(
            next.document(|| ("Next", "different".into(), None)).title,
            "Next"
        );
    }

    #[test]
    fn end_before_first_frame_can_scroll_back_without_a_sentinel_offset() {
        let mut view = Viewport::default();
        view.handle_key(KeyCode::End);
        view.handle_key(KeyCode::Up);
        assert_eq!(
            view.top(view.document(|| ("Evidence", "a\nb\nc".into(), None))),
            1
        );
        view.handle_key(KeyCode::Up);
        assert_eq!(view.top(view.document(|| unreachable!())), 0);
        view.handle_key(KeyCode::Down);
        assert_eq!(view.top(view.document(|| unreachable!())), 1);
        view.handle_key(KeyCode::End);
        view.handle_key(KeyCode::PageUp);
        assert_eq!(view.top(view.document(|| unreachable!())), 0);
    }
}
