//! Selecting text in the preview pane: press and drag across lines,
//! double-click a word, triple-click a line; Ctrl+C or the pane's copy
//! button copies it. GPUI's text can't be selected by itself, so each line
//! keeps its `TextLayout` (filled in when drawn) to turn the pointer into a
//! character.

use std::ops::Range;

use super::*;

/// A place in the pane's text: line, byte in that line.
type At = (usize, usize);

#[derive(Default)]
pub(super) struct PaneSelection {
    /// What the selection belongs to (the previewed file or clipboard
    /// entry); anything else in the pane starts it over.
    key: String,
    anchor: Option<At>,
    head: At,
    dragging: bool,
    /// Each line's layout from the last draw, for hit testing.
    pub(super) layouts: Vec<gpui::TextLayout>,
}

impl PaneSelection {
    /// Forgets the selection when the pane shows something else.
    pub(super) fn follow(&mut self, key: &str) {
        if self.key != key {
            *self = PaneSelection { key: key.to_string(), ..Default::default() };
        }
    }

    /// Start and end, in order; `None` when nothing is selected.
    fn ordered(&self) -> Option<(At, At)> {
        let anchor = self.anchor?;
        let (start, end) = if anchor <= self.head { (anchor, self.head) } else { (self.head, anchor) };
        (start != end).then_some((start, end))
    }

    /// The selected bytes of line `ix` (`len` bytes long).
    pub(super) fn line_range(&self, ix: usize, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered()?;
        if ix < start.0 || ix > end.0 {
            return None;
        }
        let from = if ix == start.0 { start.1.min(len) } else { 0 };
        let to = if ix == end.0 { end.1.min(len) } else { len };
        Some(from..to)
    }

    /// Whether line `ix` lies inside the selection (for empty lines).
    pub(super) fn covers_line(&self, ix: usize) -> bool {
        self.ordered().is_some_and(|(start, end)| start.0 <= ix && ix < end.0)
    }

    /// The selected text, lines joined with `\n`.
    pub(super) fn text(&self, lines: &[SharedString]) -> Option<String> {
        let (start, end) = self.ordered()?;
        let parts: Vec<&str> = (start.0..=end.0.min(lines.len().saturating_sub(1)))
            .filter_map(|ix| {
                let line = lines.get(ix)?;
                let range = self.line_range(ix, line.len())?;
                line.get(range)
            })
            .collect();
        Some(parts.join("\n"))
    }

    /// The line and byte under `point` (above: the start; below: the end).
    fn hit(&self, point: Point<gpui::Pixels>, lines: &[SharedString]) -> At {
        for (ix, layout) in self.layouts.iter().enumerate() {
            let bounds = layout.bounds();
            if point.y < bounds.top() && ix == 0 {
                return (0, 0);
            }
            if point.y <= bounds.bottom() {
                let len = lines.get(ix).map_or(0, |l| l.len());
                let byte = layout.index_for_position(point).unwrap_or_else(|i| i).min(len);
                return (ix, floor_boundary(lines.get(ix).map_or("", |l| l.as_ref()), byte));
            }
        }
        let last = lines.len().saturating_sub(1);
        (last, lines.get(last).map_or(0, |l| l.len()))
    }

    pub(super) fn press(&mut self, point: Point<gpui::Pixels>, clicks: usize, shift: bool, lines: &[SharedString]) {
        let at = self.hit(point, lines);
        let line = lines.get(at.0).map_or("", |l| l.as_ref());
        match clicks {
            2 => {
                let word = crate::input::surrounding_word(line, at.1);
                (self.anchor, self.head) = (Some((at.0, word.start)), (at.0, word.end));
            }
            n if n >= 3 => (self.anchor, self.head) = (Some((at.0, 0)), (at.0, line.len())),
            _ if shift && self.anchor.is_some() => self.head = at,
            _ => (self.anchor, self.head) = (Some(at), at),
        }
        self.dragging = clicks == 1;
    }

    /// Extends the selection while the button is held.
    pub(super) fn drag(&mut self, point: Point<gpui::Pixels>, lines: &[SharedString]) -> bool {
        if !self.dragging {
            return false;
        }
        self.head = self.hit(point, lines);
        true
    }

    pub(super) fn release(&mut self) {
        self.dragging = false;
    }
}

/// Largest char boundary of `text` at or before `byte`.
fn floor_boundary(text: &str, mut byte: usize) -> usize {
    byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

impl Overlay {
    /// The previewed text's lines and what they belong to, if the pane shows text.
    pub(super) fn pane_text(&self) -> Option<(String, Vec<SharedString>)> {
        let (key, body) = match self.mode {
            Mode::Search => {
                let p = self.preview.as_ref()?;
                (p.path.clone(), &p.body)
            }
            Mode::Clipboard => {
                let (id, body) = self.clip_preview.as_ref()?;
                (format!("clip:{id}"), body)
            }
            Mode::Settings => return None,
        };
        match body {
            Body::Text { lines, .. } => Some((key, lines.clone())),
            _ => None,
        }
    }

    /// Ctrl+C with nothing selected in the search box: the pane's selection.
    pub(super) fn on_copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.pane_text().and_then(|(_, lines)| self.pane_sel.text(&lines)) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<SharedString> {
        text.iter().map(|l| SharedString::from(l.to_string())).collect()
    }

    #[test]
    fn selection_spans_lines_in_either_direction() {
        let text = lines(&["first line", "", "third line"]);
        let mut sel = PaneSelection::default();
        (sel.anchor, sel.head) = (Some((2, 5)), (0, 6));
        assert_eq!(sel.text(&text).as_deref(), Some("line\n\nthird"));
        assert_eq!(sel.line_range(0, 10), Some(6..10));
        assert_eq!(sel.line_range(2, 10), Some(0..5));
        assert!(sel.covers_line(1) && !sel.covers_line(2));
        // Nothing selected when anchor and head meet.
        sel.head = (2, 5);
        assert_eq!(sel.text(&text), None);
        sel.follow("other");
        assert!(sel.anchor.is_none(), "another body starts over");
    }

    #[test]
    fn byte_positions_stay_on_char_boundaries() {
        assert_eq!(floor_boundary("Отчёт", 3), 2);
        assert_eq!(floor_boundary("abc", 99), 3);
    }
}
