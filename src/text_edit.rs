use unicode_segmentation::UnicodeSegmentation;

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

#[derive(Default)]
pub(crate) struct Cursor {
    offset: Option<usize>,
    selected: bool,
}

impl Cursor {
    pub fn position(&self, text: &str) -> usize {
        self.offset.unwrap_or(text.len())
    }

    pub fn selected(&self) -> bool {
        self.selected
    }

    pub fn insert(&mut self, value: &mut String, text: &str, max_bytes: usize) {
        let replaced = if self.selected { value.len() } else { 0 };
        if value.len() - replaced + text.len() > max_bytes {
            return;
        }
        if self.selected {
            value.clear();
            *self = Self::default();
        }
        let at = self.position(value);
        value.insert_str(at, text);
        // Inserting a combining mark or joiner can merge adjacent graphemes.
        let end = at + text.len();
        self.offset = value
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= end);
    }

    pub fn apply(&mut self, value: &mut String, edit: Edit) {
        let at = self.position(value);
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
                self.selected = true;
                return;
            }
            Edit::Paste => return,
            Edit::Left => {
                if self.selected {
                    0
                } else {
                    before()
                }
            }
            Edit::Right => {
                if self.selected {
                    value.len()
                } else {
                    after()
                }
            }
            Edit::Start => 0,
            Edit::End => value.len(),
            _ => {
                let (start, end) = match edit {
                    Edit::Clear => (0, value.len()),
                    _ if self.selected => (0, value.len()),
                    Edit::Backspace => (before(), at),
                    Edit::Delete => (at, after()),
                    Edit::DeleteToStart => (0, at),
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
        self.selected = false;
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
}
