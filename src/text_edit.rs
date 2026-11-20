use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;

use crate::render::TextItem;

#[derive(Clone, Copy)]
pub(crate) enum Edit {
    Left,
    Right,
    Start,
    End,
    Backspace,
    Delete,
    Clear,
    DeleteToStart,
    SelectAll,
    Paste,
}

/// Where the caret can sit in the focused field, as the renderer laid it out
/// last frame: the window x of every grapheme boundary (by UTF-8 offset) and
/// the visible span. The renderer owns the font, so pointer picking reads
/// this instead of estimating glyph positions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FieldLayout {
    /// The text those offsets index.
    pub text: String,
    pub stops: Vec<(usize, f32)>,
    pub left: f32,
    pub right: f32,
}

impl FieldLayout {
    /// The boundary nearest `x`. Past either end of the visible span it is
    /// the first boundary scrolled out on that side, so a drag held outside
    /// the field walks the text into view a grapheme per pointer move.
    fn pick(&self, x: f32) -> usize {
        let stop = if x > self.right {
            self.stops.iter().find(|s| s.1 > self.right).or(self.stops.last())
        } else if x < self.left {
            self.stops.iter().rev().find(|s| s.1 < self.left).or(self.stops.first())
        } else {
            self.stops.iter().min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs()))
        };
        stop.map_or(0, |s| s.0)
    }

    /// Byte range of the word whose glyph is under `x`: a run of letters,
    /// digits and underscores, a run of whitespace, or one other grapheme.
    fn word(&self, x: f32) -> (usize, usize) {
        let ends = self.stops.len().saturating_sub(1);
        // The stop that starts the grapheme under x.
        let n = if x > self.right {
            self.stops.iter().position(|s| s.1 > self.right).unwrap_or(ends).saturating_sub(1)
        } else if x < self.left {
            self.stops.iter().rposition(|s| s.1 < self.left).unwrap_or(0)
        } else {
            self.stops.iter().rposition(|s| s.1 <= x).unwrap_or(0)
        }
        .min(ends.saturating_sub(1));
        let class = |n: usize| {
            let from = self.stops.get(n).map_or(0, |s| s.0);
            let to = self.stops.get(n + 1).map_or(self.text.len(), |s| s.0);
            match self.text.get(from..to).and_then(|g| g.chars().next()) {
                Some(c) if c.is_alphanumeric() || c == '_' => 0,
                Some(c) if c.is_whitespace() => 1,
                // Punctuation stands alone: never equal to its neighbour.
                _ => 2 + n,
            }
        };
        let (mut from, mut to) = (n, (n + 1).min(ends));
        while from > 0 && class(from - 1) == class(n) {
            from -= 1;
        }
        while to < ends && class(to) == class(n) {
            to += 1;
        }
        (self.stops.get(from).map_or(0, |s| s.0), self.stops.get(to).map_or(0, |s| s.0))
    }
}

/// Presses this close in time and place count as one multi-click.
const MULTI_CLICK: Duration = Duration::from_millis(500);
const MULTI_CLICK_SLOP: f32 = 4.0;

const BLINK_MS: u128 = 500;

pub(crate) struct Cursor {
    offset: Option<usize>,
    /// The other end of the selection; equal to the caret means none.
    anchor: Option<usize>,
    layout: FieldLayout,
    /// Pointer x of a press / drag the layout has not placed yet: a click
    /// that focuses a field arrives before the field has been laid out.
    press: Option<f32>,
    drag: Option<f32>,
    dragging: bool,
    /// 1 places the caret, 2 selects the word under it, 3 the whole field;
    /// a drag from there extends by the same unit.
    clicks: u8,
    last_press: Option<(Instant, f32)>,
    /// What the press selected: the end a drag extends from.
    origin: (usize, usize),
    /// Last edit or pointer placement; the blink restarts from it.
    touched: Instant,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            offset: None,
            anchor: None,
            layout: FieldLayout::default(),
            press: None,
            drag: None,
            dragging: false,
            clicks: 0,
            last_press: None,
            origin: (0, 0),
            touched: Instant::now(),
        }
    }
}

fn boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

impl Cursor {
    pub fn position(&self, text: &str) -> usize {
        self.offset.map_or(text.len(), |at| boundary(text, at))
    }

    /// The selected byte range, start before end; `None` for a bare caret.
    pub fn selection(&self, text: &str) -> Option<(usize, usize)> {
        let (a, b) = (boundary(text, self.anchor?), self.position(text));
        (a != b).then(|| (a.min(b), a.max(b)))
    }

    /// On for half a second, off for half, restarting at every edit.
    pub fn caret_visible(&self) -> bool {
        (self.touched.elapsed().as_millis() / BLINK_MS).is_multiple_of(2)
    }

    /// When the caret next changes state: the redraw a blink needs.
    pub fn next_blink(&self) -> Instant {
        let phases = self.touched.elapsed().as_millis() / BLINK_MS + 1;
        self.touched + Duration::from_millis((phases * BLINK_MS) as u64)
    }

    /// Put the caret and selection on the label that draws `text`.
    pub fn show(&self, item: &mut TextItem) {
        // A press the layout has not placed yet: draw the field from its
        // start, as it stood unfocused under the pointer, so the frame that
        // places the press measures the text the user clicked on.
        if self.press.is_some() {
            item.caret = Some(0);
            return;
        }
        item.caret = Some(self.position(&item.text));
        item.selection = self.selection(&item.text);
        item.caret_visible = item.selection.is_none() && self.caret_visible();
    }

    /// A press in the field: the caret goes to the boundary under `x` and a
    /// drag from here selects. A second press in the same place selects the
    /// word there, a third the whole field.
    pub fn press(&mut self, x: f32) {
        self.touched = Instant::now();
        let again = self.last_press.is_some_and(|(at, px)| {
            self.touched - at < MULTI_CLICK && (px - x).abs() < MULTI_CLICK_SLOP
        });
        self.clicks = if again { (self.clicks + 1).min(3) } else { 1 };
        self.last_press = Some((self.touched, x));
        self.dragging = true;
        self.anchor = None;
        self.press = Some(x);
        self.drag = None;
        self.resolve();
    }

    /// The pointer moved with the button held since `press`.
    pub fn drag(&mut self, x: f32) -> bool {
        if !self.dragging {
            return false;
        }
        self.touched = Instant::now();
        self.drag = Some(x);
        self.resolve();
        true
    }

    pub fn release(&mut self) {
        self.dragging = false;
    }

    /// The frame's layout of this field (empty when it drew no caret).
    /// True when a waiting press or drag moved the caret, which needs
    /// another frame to show.
    pub fn layout(&mut self, layout: FieldLayout) -> bool {
        self.layout = layout;
        self.resolve()
    }

    fn resolve(&mut self) -> bool {
        if self.layout.stops.is_empty() {
            return false;
        }
        let before = (self.offset, self.anchor);
        let unit = |layout: &FieldLayout, x: f32| match self.clicks {
            2 => layout.word(x),
            3 => (0, layout.text.len()),
            _ => {
                let at = layout.pick(x);
                (at, at)
            }
        };
        if let Some(x) = self.press.take() {
            self.origin = unit(&self.layout, x);
            self.anchor = Some(self.origin.0);
            self.offset = Some(self.origin.1);
        }
        if let Some(x) = self.drag.take() {
            // The selection grows from what the press took, a unit at a time.
            let to = unit(&self.layout, x);
            if to.0 < self.origin.0 {
                self.anchor = Some(self.origin.1);
                self.offset = Some(to.0);
            } else {
                self.anchor = Some(self.origin.0);
                self.offset = Some(to.1.max(self.origin.1));
            }
        }
        before != (self.offset, self.anchor)
    }

    pub fn insert(&mut self, value: &mut String, text: &str, max_bytes: usize) {
        let at = self.position(value);
        let (start, end) = self.selection(value).unwrap_or((at, at));
        if value.len() - (end - start) + text.len() > max_bytes {
            return;
        }
        value.replace_range(start..end, text);
        // Inserting a combining mark or joiner can merge adjacent graphemes.
        let end = start + text.len();
        self.anchor = None;
        self.offset = value
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= end);
        self.touched = Instant::now();
    }

    pub fn apply(&mut self, value: &mut String, edit: Edit) {
        self.touched = Instant::now();
        let at = self.position(value);
        let selection = self.selection(value);
        let before = || {
            value
                .grapheme_indices(true)
                .rev()
                .find(|&(i, _)| i < at)
                .map_or(0, |(i, _)| i)
        };
        let after = || {
            value[at..]
                .graphemes(true)
                .next()
                .map_or(at, |g| at + g.len())
        };
        let next = match edit {
            Edit::SelectAll => {
                self.anchor = Some(0);
                self.offset = Some(value.len());
                return;
            }
            Edit::Paste => return,
            // An arrow collapses a selection to that end of it.
            Edit::Left => selection.map_or_else(before, |s| s.0),
            Edit::Right => selection.map_or_else(after, |s| s.1),
            Edit::Start => 0,
            Edit::End => value.len(),
            _ => {
                let (start, end) = match (edit, selection) {
                    (Edit::Clear, _) => (0, value.len()),
                    (_, Some(selection)) => selection,
                    (Edit::Backspace, None) => (before(), at),
                    (Edit::Delete, None) => (at, after()),
                    (Edit::DeleteToStart, None) => (0, at),
                    _ => unreachable!(),
                };
                value.replace_range(start..end, "");
                // Deletion can join graphemes across the removed range.
                value
                    .grapheme_indices(true)
                    .map(|(i, _)| i)
                    .find(|&i| i >= start)
                    .unwrap_or(value.len())
            }
        };
        self.anchor = None;
        self.offset = Some(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_and_deletes_whole_graphemes() {
        let mut value = String::from("a👩‍💻e\u{301}z");
        let mut cursor = Cursor::default();
        cursor.apply(&mut value, Edit::Start);
        cursor.apply(&mut value, Edit::Right);
        cursor.apply(&mut value, Edit::Delete);
        assert_eq!(value, "ae\u{301}z");
        cursor.apply(&mut value, Edit::End);
        cursor.apply(&mut value, Edit::Left);
        cursor.insert(&mut value, "!", usize::MAX);
        assert_eq!(value, "ae\u{301}!z");
        cursor.apply(&mut value, Edit::DeleteToStart);
        assert_eq!(value, "z");
        assert_eq!(cursor.position(&value), 0);
        cursor.apply(&mut value, Edit::Clear);
        assert!(value.is_empty());
    }

    /// "abcdef" in a field 10px a glyph, starting at x=100.
    fn laid(text: &str) -> FieldLayout {
        FieldLayout {
            text: text.into(),
            stops: (0..=text.len()).map(|i| (i, 100.0 + 10.0 * i as f32)).collect(),
            left: 100.0,
            right: 100.0 + 10.0 * text.len() as f32,
        }
    }

    #[test]
    fn a_press_places_the_caret_and_a_drag_selects() {
        let mut value = String::from("abcdef");
        let mut cursor = Cursor::default();
        cursor.layout(laid(&value));
        cursor.press(121.0);
        assert_eq!(cursor.position(&value), 2);
        assert_eq!(cursor.selection(&value), None);
        assert!(cursor.drag(147.0));
        assert_eq!(cursor.selection(&value), Some((2, 5)));
        // Dragging back past the anchor selects the other way.
        cursor.drag(108.0);
        assert_eq!(cursor.selection(&value), Some((1, 2)));
        cursor.drag(147.0);
        cursor.release();
        assert!(!cursor.drag(100.0), "no button held: a move is not a drag");
        cursor.insert(&mut value, "X", usize::MAX);
        assert_eq!(value, "abXf");
        assert_eq!((cursor.position(&value), cursor.selection(&value)), (3, None));
    }

    #[test]
    fn a_press_before_the_field_is_laid_out_waits_for_the_layout() {
        let value = String::from("abcdef");
        let mut cursor = Cursor::default();
        cursor.press(131.0);
        cursor.drag(152.0);
        assert_eq!(cursor.position(&value), 6, "at the end until it is placed");
        assert!(cursor.layout(laid(&value)), "the caret moved: draw again");
        assert_eq!(cursor.selection(&value), Some((3, 5)));
        assert!(!cursor.layout(laid(&value)));
    }

    #[test]
    fn edits_act_on_the_selection() {
        let mut value = String::from("abcdef");
        let mut cursor = Cursor::default();
        cursor.layout(laid(&value));
        cursor.press(110.0);
        cursor.drag(130.0);
        cursor.apply(&mut value, Edit::Right);
        assert_eq!((cursor.position(&value), cursor.selection(&value)), (3, None));
        cursor.last_press = None;
        cursor.press(110.0);
        cursor.drag(130.0);
        cursor.apply(&mut value, Edit::Backspace);
        assert_eq!((value.as_str(), cursor.position(&value)), ("adef", 1));
        cursor.apply(&mut value, Edit::SelectAll);
        assert_eq!(cursor.selection(&value), Some((0, 4)));
        cursor.insert(&mut value, "z", usize::MAX);
        assert_eq!(value, "z");
    }

    #[test]
    fn a_drag_outside_the_field_steps_into_the_scrolled_out_text() {
        let value = String::from("abcdef");
        let mut cursor = Cursor::default();
        // Only "cd" is in view: the field shows x 120..140.
        cursor.layout(FieldLayout { left: 120.0, right: 140.0, ..laid(&value) });
        cursor.press(130.0);
        cursor.drag(400.0);
        assert_eq!(cursor.selection(&value), Some((3, 5)));
        cursor.drag(-50.0);
        assert_eq!(cursor.selection(&value), Some((1, 3)));
    }

    #[test]
    fn a_double_click_selects_the_word_and_a_triple_click_everything() {
        let mut value = String::from("my clip_2.export.mp4");
        let mut cursor = Cursor::default();
        cursor.layout(laid(&value));
        // x 150 is the "i" of "clip_2".
        cursor.press(150.0);
        assert_eq!(cursor.selection(&value), None);
        cursor.press(151.0);
        assert_eq!(cursor.selection(&value), Some((3, 9)));
        // Dragging on from a double-click takes whole words.
        cursor.drag(215.0);
        assert_eq!(cursor.selection(&value), Some((3, 16)));
        cursor.drag(105.0);
        assert_eq!(cursor.selection(&value), Some((0, 9)));
        cursor.release();
        cursor.press(150.0);
        assert_eq!(cursor.selection(&value), Some((0, value.len())));
        cursor.insert(&mut value, "x", usize::MAX);
        assert_eq!(value, "x");
        // Punctuation and spaces are their own words; a press elsewhere or
        // later starts a new count.
        let value = String::from("my clip_2.export.mp4");
        cursor.layout(laid(&value));
        cursor.press(192.0);
        assert_eq!(cursor.selection(&value), None);
        cursor.press(192.0);
        assert_eq!(cursor.selection(&value), Some((9, 10)));
        cursor.last_press = Some((Instant::now() - MULTI_CLICK, 192.0));
        cursor.press(192.0);
        assert_eq!((cursor.position(&value), cursor.selection(&value)), (9, None));
    }

    #[test]
    fn the_caret_blinks_from_the_last_edit() {
        let mut cursor = Cursor::default();
        assert!(cursor.caret_visible());
        cursor.touched = Instant::now() - Duration::from_millis(700);
        assert!(!cursor.caret_visible());
        let wait = cursor.next_blink() - Instant::now();
        assert!(wait <= Duration::from_millis(300), "{wait:?}");
        cursor.press(0.0);
        assert!(cursor.caret_visible(), "a click shows the caret at once");
    }
}
