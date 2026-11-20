//! The catalog is a draft editor, not the processing chain. Browsing never
//! changes enabled stages, preview cache keys, exports, or a running job.
use super::*;
use figleaf::{Shadow, StrokeAlign, Text, paint::Align as Place, text::TextTruncation};

const CATALOG_PITCH: f32 = 45.0;
const SEARCH_ICON: usize = 13;
const CATALOG_ICON: usize = 16;
const PROCESSOR_GLYPH: usize = 17;
const PRESET_ICONS: [usize; 4] = [18, 19, 20, 21];
const HEADER_HEIGHT: f32 = 90.0;

pub(super) struct Picker {
    pub(super) focus: usize,
    pub(super) preset: usize,
    pub(super) scroll: f32,
    pub(super) drafts: BTreeMap<String, Value>,
    pub(super) search: String,
    pub(super) search_focus: bool,
    pub(super) list_scroll: f32,
    pub(super) message: String,
}

#[derive(Clone, Copy)]
pub(super) struct PickerLayout {
    pub(super) panel: RectPx,
    pub(super) body: RectPx,
    pub(super) browser: RectPx,
    pub(super) search: RectPx,
    pub(super) list: RectPx,
    pub(super) heading: RectPx,
    pub(super) settings: RectPx,
    pub(super) about: RectPx,
    pub(super) fields: RectPx,
    pub(super) add: RectPx,
    pub(super) cancel: RectPx,
    pub(super) close: RectPx,
}
impl PickerLayout {
    pub(super) fn new(vp: (f32, f32), chip_bottom: f32) -> Self {
        let w = (vp.0 - 32.0).clamp(320.0, 880.0);
        let h = (vp.1 - 72.0).clamp(300.0, 644.0);
        let panel = RectPx {
            x: (vp.0 - w) / 2.0,
            y: ((vp.1 - h) / 2.0 + 30.0).min(vp.1 - h - 16.0).max(16.0),
            w,
            h,
        };
        let body = RectPx {
            x: panel.x + 1.0,
            y: chip_bottom,
            w: w - 2.0,
            h: (panel.y + h - 74.0 - chip_bottom).max(1.0),
        };
        let bw = if w >= 820.0 { 300.0 } else { 220.0 };
        let browser = RectPx { w: bw, ..body };
        let search = RectPx {
            x: browser.x + 20.0,
            y: body.y + 20.0,
            w: bw - 40.0,
            h: 34.0,
        };
        let list = RectPx {
            x: browser.x + 20.0,
            y: search.y + 48.0,
            w: bw - 40.0,
            h: (body.h - 88.0).clamp(1.0, 291.0),
        };
        let short = h < 500.0;
        let heading = RectPx {
            x: browser.x + bw + 24.0,
            y: body.y + if short { 10.0 } else { 22.0 },
            w: w - bw - 48.0,
            h: if short { 72.0 } else { 84.0 },
        };
        let sw = if w >= 820.0 {
            258.0
        } else {
            (heading.w * 0.53).max(160.0)
        };
        let settings_y = heading.y + heading.h + if short { 6.0 } else { 21.0 };
        let settings = RectPx {
            x: heading.x,
            y: settings_y,
            w: sw,
            h: (body.y + body.h - settings_y - if short { 4.0 } else { 10.0 }).max(1.0),
        };
        let about = RectPx {
            x: settings.x + sw + 28.0,
            y: settings.y,
            w: (heading.w - sw - 28.0).max(1.0),
            h: settings.h,
        };
        let fields_top = if short { 50.0 } else { 71.0 };
        let fields = RectPx {
            x: settings.x,
            y: settings.y + fields_top,
            w: sw,
            h: (settings.h - fields_top).max(1.0),
        };
        let add = RectPx {
            x: panel.x + w - 172.0,
            y: panel.y + h - 57.0,
            w: 148.0,
            h: 40.0,
        };
        Self {
            panel,
            body,
            browser,
            search,
            list,
            heading,
            settings,
            about,
            fields,
            add,
            cancel: RectPx {
                x: add.x - 86.0,
                w: 74.0,
                ..add
            },
            close: RectPx {
                x: panel.x + w - 55.0,
                y: panel.y + 30.5,
                w: 30.0,
                h: 30.0,
            },
        }
    }
    fn list_content(self) -> RectPx {
        RectPx {
            x: self.list.x + 2.0,
            y: self.list.y + 12.0,
            w: self.list.w - 4.0,
            h: (self.list.h - 24.0).max(1.0),
        }
    }
    pub(super) fn presets(self) -> [RectPx; 5] {
        let x = self.settings.x;
        let y = self.settings.y + 23.0;
        let w = self.settings.w;
        [
            RectPx {
                x: x + 25.0,
                y,
                w: w - 108.0,
                h: 23.0,
            },
            RectPx {
                x,
                y,
                w: 25.0,
                h: 23.0,
            },
            RectPx {
                x: x + w - 83.0,
                y,
                w: 25.0,
                h: 23.0,
            },
            RectPx {
                x: x + w - 54.0,
                y,
                w: 25.0,
                h: 23.0,
            },
            RectPx {
                x: x + w - 25.0,
                y,
                w: 25.0,
                h: 23.0,
            },
        ]
    }
}
impl Panel {
    pub(crate) fn modal_open(&self) -> bool {
        self.open && self.picker.is_some()
    }
    pub(super) fn chain_focus(&self) -> usize {
        self.picker.as_ref().map_or(self.selected, |p| p.focus)
    }
    pub(super) fn search_focused(&self) -> bool {
        self.picker.as_ref().is_some_and(|p| p.search_focus)
    }
    pub(crate) fn open_picker(&mut self) {
        if self.picker.is_some() {
            return;
        }
        self.card_drag = None;
        self.dragging = None;
        self.scrubbing = false;
        self.editing = None;
        self.preset_name = None;
        self.filter = None;
        self.picker = Some(Picker {
            focus: self.selected,
            preset: self.preset,
            scroll: self.scroll,
            drafts: BTreeMap::new(),
            search: String::new(),
            search_focus: true,
            list_scroll: 0.0,
            message: String::new(),
        });
        self.text_cursor = Default::default();
        self.open = true;
        if let Some(i) = (0..self.descriptors.len())
            .find(|i| !self.added[*i])
            .or_else(|| (!self.descriptors.is_empty()).then_some(0))
        {
            self.pick_candidate(i);
        }
    }
    pub(super) fn pick_candidate(&mut self, i: usize) {
        if self.picker.is_none() {
            return;
        }
        if let Some(d) = self.descriptors.get(self.selected) {
            self.picker
                .as_mut()
                .unwrap()
                .drafts
                .insert(d.id.clone(), self.settings.clone());
        }
        self.select(i);
        if let Some(draft) = self
            .picker
            .as_ref()
            .unwrap()
            .drafts
            .get(&self.descriptors[i].id)
        {
            self.settings = draft.clone();
        }
    }
    pub(super) fn cancel_picker(&mut self) {
        let Some(p) = self.picker.take() else {
            return;
        };
        self.filter = None;
        self.select(p.focus);
        self.preset = p.preset;
        self.scroll = p.scroll;
        self.editing = None;
        self.preset_name = None;
        self.dragging = None;
    }
    pub(super) fn add_candidate(&mut self) {
        if self.picker.is_none()
            || !self.catalog_visible().contains(&self.selected)
            || self.added[self.selected]
        {
            return;
        }
        if !self.finish_field_edit() {
            return;
        }
        let settings = self.settings.clone();
        let index = self.selected;
        self.picker = None;
        self.filter = None;
        self.saved[index] = settings;
        self.added[index] = true;
        self.enabled[index] = true;
        // New steps append after all existing members, even when catalog order differs.
        let to = self
            .added
            .iter()
            .enumerate()
            .filter(|(i, v)| **v && *i != index)
            .map(|(i, _)| i)
            .max()
            .map_or(0, |last| if index < last { last } else { last + 1 });
        self.move_selected_to(to);
        // Restore spatial controls even when appending did not reorder the catalog.
        self.select(self.selected);
        self.changed();
        self.list_scroll = 0.0;
    }
    pub(super) fn remove_selected(&mut self) {
        if !self.added.get(self.selected).copied().unwrap_or(false) {
            return;
        }
        self.added[self.selected] = false;
        self.enabled[self.selected] = false;
        if let Some(i) = self.visible().last().copied() {
            self.select(i);
        } else {
            self.changed();
        }
    }
    pub(super) fn catalog_visible(&self) -> Vec<usize> {
        let query = self
            .picker
            .as_ref()
            .map_or("", |p| p.search.as_str())
            .trim()
            .to_lowercase();
        (0..self.descriptors.len())
            .filter(|i| {
                self.filter
                    .as_ref()
                    .is_none_or(|c| self.categories[*i].contains(c))
                    && (query.is_empty()
                        || self.descriptors[*i].name.to_lowercase().contains(&query)
                        || self.descriptors[*i].id.to_lowercase().contains(&query)
                        || category_label(&self.categories[*i])
                            .to_lowercase()
                            .contains(&query))
            })
            .collect()
    }
    pub(super) fn search_changed(&mut self) {
        if let Some(p) = &mut self.picker {
            p.list_scroll = 0.0;
        }
        let results = self.catalog_visible();
        if let Some(&i) = results.first()
            && !results.contains(&self.selected)
        {
            self.pick_candidate(i);
        }
    }
    pub(super) fn picker_chips(&self, vp: (f32, f32)) -> Vec<(RectPx, Option<Category>, String)> {
        if self.picker.is_none() {
            return vec![];
        }
        let panel = PickerLayout::new(vp, 0.0).panel;
        use figleaf::layout::{Node, Tree};

        let width = panel.w - 39.0;
        let mut tree = Tree::new(Node::row().gap(2.0).wrap(6.0));
        let root = tree.root();
        let chips: Vec<_> = std::iter::once(None)
            .chain(self.chip_categories().into_iter().map(Some))
            .map(|category| {
                let label = category
                    .as_ref()
                    .map_or("ALL".into(), |c| c.label().to_uppercase());
                let w = (label.chars().count() as f32 * 6.7 + 20.0).clamp(53.0, width);
                (tree.add(root, Node::fixed([w, 18.0])), category, label)
            })
            .collect();
        let solved = tree
            .solve([width, 0.0])
            .expect("valid category chip layout");
        chips
            .into_iter()
            .map(|(id, category, label)| {
                let r = solved[id];
                (
                    RectPx {
                        x: panel.x + 23.0 + r.x,
                        y: panel.y + HEADER_HEIGHT + 5.0 + r.y,
                        w: r.w,
                        h: r.h,
                    },
                    category,
                    label,
                )
            })
            .collect()
    }
    pub(super) fn picker_layout(&self, vp: (f32, f32)) -> PickerLayout {
        let bottom = self.picker_chips(vp).last().map_or(
            PickerLayout::new(vp, 0.0).panel.y + HEADER_HEIGHT + 28.0,
            |(r, ..)| r.y + r.h + 5.0,
        );
        PickerLayout::new(vp, bottom)
    }
    pub(super) fn picker_pitch(&self, i: usize) -> f32 {
        match self.fields[i].kind {
            Kind::Spatial { .. } => 104.0,
            Kind::Number { .. } => 44.0,
            _ => 28.0,
        }
    }
    pub(super) fn picker_row(&self, i: usize, vp: (f32, f32)) -> RectPx {
        self.picker_row_in(i, self.picker_layout(vp).fields)
    }
    pub(super) fn picker_row_in(&self, i: usize, r: RectPx) -> RectPx {
        RectPx {
            x: r.x,
            y: r.y + (0..i).map(|j| self.picker_pitch(j)).sum::<f32>() - self.scroll,
            w: r.w,
            h: self.picker_pitch(i) - 8.0,
        }
    }
    pub(super) fn fields_clip(&self, vp: (f32, f32)) -> RectPx {
        if self.picker.is_some() {
            return self.picker_layout(vp).fields;
        }
        RectPx {
            x: vp.0 - self.right(vp),
            y: HEADER_H + 158.0,
            w: self.right(vp),
            h: (self.info(vp).y - HEADER_H - 158.0).max(1.0),
        }
    }
    pub(super) fn finish_field_edit(&mut self) -> bool {
        let Some((i, value)) = self.editing.clone() else {
            return true;
        };
        match text_value(
            &self.fields[i],
            &self.descriptors[self.selected].settings_schema,
            &value,
        ) {
            Ok(Some(v)) => self.settings[&self.fields[i].key] = v,
            Ok(None) => {
                self.settings
                    .as_object_mut()
                    .unwrap()
                    .remove(&self.fields[i].key);
            }
            Err(e) => {
                self.status = e.clone();
                if let Some(p) = &mut self.picker {
                    p.message = e;
                }
                return false;
            }
        }
        self.editing = None;
        self.changed();
        true
    }
    pub(super) fn picker_key(&mut self, key: Key) -> bool {
        match key {
            Key::Escape => self.cancel_picker(),
            Key::Enter => self.add_candidate(),
            Key::Up | Key::Down => {
                let rows = self.catalog_visible();
                if !rows.is_empty() {
                    if !self.finish_field_edit() {
                        return true;
                    }
                    let at = rows.iter().position(|i| *i == self.selected).unwrap_or(0);
                    let at = if key == Key::Up {
                        at.saturating_sub(1)
                    } else {
                        (at + 1).min(rows.len() - 1)
                    };
                    self.pick_candidate(rows[at]);
                    // Conservative visible range works at the minimum window height.
                    self.picker.as_mut().unwrap().list_scroll =
                        (at.saturating_sub(1)) as f32 * CATALOG_PITCH;
                }
            }
            Key::Tab => {
                self.picker.as_mut().unwrap().search_focus = !self.search_focused();
                self.text_cursor = Default::default();
            }
            Key::Char(c) => {
                if !self.search_focused() {
                    self.picker.as_mut().unwrap().search_focus = true;
                    self.text_cursor = Default::default();
                }
                self.text_input(&c.to_string());
            }
            Key::Space if self.search_focused() => {
                self.text_input(" ");
            }
            _ => {}
        }
        true
    }
    pub(super) fn picker_scroll(&mut self, dy: f32, cursor: (f32, f32), vp: (f32, f32)) {
        let l = self.picker_layout(vp);
        if hit(l.browser, cursor.0, cursor.1) {
            let max =
                (self.catalog_visible().len() as f32 * CATALOG_PITCH + 21.0 - l.list.h).max(0.0);
            let p = self.picker.as_mut().unwrap();
            p.list_scroll = (p.list_scroll - dy).clamp(0.0, max);
        } else if hit(l.fields, cursor.0, cursor.1) {
            let max = ((0..self.fields.len())
                .map(|i| self.picker_pitch(i))
                .sum::<f32>()
                - l.fields.h)
                .max(0.0);
            self.scroll = (self.scroll - dy).clamp(0.0, max);
        }
    }
    pub(super) fn picker_click(&mut self, x: f32, y: f32, vp: (f32, f32)) -> bool {
        let l = self.picker_layout(vp);
        if hit(l.cancel, x, y) || hit(l.close, x, y) {
            self.cancel_picker();
            return true;
        }
        if !hit(l.panel, x, y) {
            return true;
        }
        if hit(l.add, x, y) {
            self.add_candidate();
            return true;
        }
        if let Some((i, _)) = self.editing.as_ref()
            && hit(self.row(*i, vp), x, y)
        {
            return self.settings_click(x, y, vp);
        }
        if !self.finish_field_edit() {
            return true;
        }
        if hit(l.search, x, y) {
            self.preset_name = None;
            self.picker.as_mut().unwrap().search_focus = true;
            self.text_cursor.press(x);
            return true;
        }
        self.picker.as_mut().unwrap().search_focus = false;
        if let Some((_, c, _)) = self
            .picker_chips(vp)
            .into_iter()
            .find(|(r, ..)| hit(*r, x, y))
        {
            self.set_filter(c);
            return true;
        }
        if hit(l.list_content(), x, y) {
            for (n, i) in self.catalog_visible().into_iter().enumerate() {
                if hit(self.catalog_row(n, l.list), x, y) {
                    self.pick_candidate(i);
                    return true;
                }
            }
            return true;
        }
        if !self.catalog_visible().contains(&self.selected) {
            return true;
        }
        self.settings_click(x, y, vp)
    }
    pub(super) fn catalog_row(&self, n: usize, r: RectPx) -> RectPx {
        RectPx {
            x: r.x + 12.0,
            y: r.y + 12.0 + n as f32 * CATALOG_PITCH
                - self.picker.as_ref().map_or(0.0, |p| p.list_scroll),
            w: r.w - 22.0,
            h: 42.0,
        }
    }

    pub(super) fn add_button(&self, vp: (f32, f32)) -> RectPx {
        let empty = !self.added.iter().any(|v| *v);
        RectPx {
            x: (Self::left(vp) - 174.0) / 2.0,
            y: if empty {
                self.group_top(vp) + (self.detail(vp).0.y - 16.0 - self.group_top(vp)) * 0.328
            } else {
                let list = self.list(vp);
                let rows = self.visible().len() as f32 * CARD_PITCH - 3.0;
                list.y + rows.min(list.h) + 12.0
            },
            w: 174.0,
            h: 32.0,
        }
    }
    pub(super) fn draw_add_button(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let r = self.add_button(vp);
        items.push(Item::Rect(RectItem {
            radius: 7.0,
            border_w: 1.0,
            border_color: hex_color(0x686868),
            ..RectItem::new(
                r,
                hex_color(if hit(r, self.cursor.0, self.cursor.1) {
                    0x242424
                } else {
                    0x0f0f0f
                }),
            )
        }));
        text_center(items, r, "+ Add Processor", 11.0, 0xa3a3a3);
    }
    pub(super) fn draw_empty_chain(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let w = Self::left(vp);
        items.push(Item::Rect(RectItem::new(
            RectPx {
                x: 0.0,
                y: HEADER_H,
                w,
                h: Self::bottom(vp) - HEADER_H,
            },
            hex_color(0x050505),
        )));
        text(
            items,
            15.0,
            HEADER_H + 17.0,
            16.0,
            "Processors",
            hex_color(0xe6e6e6),
            w - 70.0,
        );
        let badge = RectPx {
            x: w - 48.0,
            y: HEADER_H + 15.0,
            w: 32.0,
            h: 22.0,
        };
        items.push(Item::Rect(RectItem {
            radius: 12.0,
            ..RectItem::new(badge, hex_color(0x333333))
        }));
        text_center(items, badge, "0", 12.0, 0xd6d6d6);
        let box_r = RectPx {
            x: 15.0,
            y: self.group_top(vp),
            w: w - 30.0,
            h: (self.detail(vp).0.y - 16.0 - self.group_top(vp)).max(1.0),
        };
        items.push(Item::Rect(RectItem {
            radius: 10.0,
            ..RectItem::new(box_r, hex_color(0x0f0f0f))
        }));
        self.draw_add_button(items, vp);
        let b = self.add_button(vp);
        for (i, line) in [
            "Build a chain for this clip.",
            "Choose a processor and",
            "adjust its settings before",
            "adding it.",
        ]
        .iter()
        .enumerate()
        {
            interface_center(
                items,
                RectPx {
                    x: 30.0,
                    y: b.y + 49.0 + i as f32 * 17.0,
                    w: w - 60.0,
                    h: 17.0,
                },
                line,
                12.0,
                0xd6d6d6,
            );
        }
        self.draw_renders(items, vp);
    }
    pub(crate) fn draw_picker(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let Some(p) = self.picker.as_ref().filter(|_| self.open) else {
            return;
        };
        let l = self.picker_layout(vp);
        let r = l.panel;
        items.push(Item::Clip(None));
        items.push(Item::Shape(ui_shape(
            RectPx {
                x: 0.0,
                y: 0.0,
                w: vp.0,
                h: vp.1,
            },
            [0.0, 0.0, 0.0, 0.72],
        )));
        items.push(Item::Shape(
            ui_shape(r, hex_color(0x171717)).radius(14.0).stroke(
                1.0,
                hex_color(0x303234),
                StrokeAlign::Inside,
            ),
        ));
        picker_text(
            items,
            r.x + 25.0,
            r.y + 22.0,
            22.0,
            "Add processor",
            hex_color(0xd6d6d6),
            r.w - 80.0,
        );
        interface_text(
            items,
            r.x + 25.0,
            r.y + 56.0,
            12.0,
            "Choose a processor, configure it, then add it to your chain.",
            0x999999,
            r.w - 80.0,
        );
        items.push(Item::Shape(ui_shape(l.close, [0.0; 4]).radius(7.0).stroke(
            1.0,
            hex_color(0x343434),
            StrokeAlign::Inside,
        )));
        picker_center(items, l.close, "×", 18.0, 0x999999, INTERFACE_FONT);
        let category_bar = RectPx {
            x: r.x + 1.0,
            y: r.y + HEADER_HEIGHT,
            w: r.w - 2.0,
            h: l.body.y - r.y - HEADER_HEIGHT,
        };
        items.push(Item::Shape(ui_shape(category_bar, hex_color(0x232323))));
        for y in [category_bar.y, l.body.y - 1.0] {
            items.push(Item::Shape(ui_shape(
                RectPx {
                    y,
                    h: 1.0,
                    ..category_bar
                },
                hex_color(0x4d4d4d),
            )));
        }
        for (r, c, name) in self.picker_chips(vp) {
            let selected = c == self.filter;
            if selected {
                items.push(Item::Shape(ui_shape(r, [0.0; 4]).radius(9.0).stroke(
                    1.5,
                    hex_color(SOURCE_RING),
                    StrokeAlign::Inside,
                )));
            }
            let ink = hex_color(if selected { 0xf0f0f0 } else { 0xc8c8c8 });
            items.push(Item::Label(
                label(name, r, 9.5, ink, MONO_FONT)
                    .letter_spacing(1.0)
                    .align(Place::Center, Place::Center),
            ));
        }
        items.push(Item::Shape(ui_shape(l.browser, hex_color(0x0f0f0f))));
        items.push(Item::Shape(
            ui_shape(l.search, hex_color(0x9fa89f))
                .radius(17.0)
                .shadow(Shadow::inner([0.0, 0.0, 0.0, 1.0], [0.0, 3.0], 9.0).spread(-1.0))
                .stroke(2.0, [1.0, 1.0, 1.0, 0.62], StrokeAlign::Inside),
        ));
        items.push(Item::FrameIcon {
            x: l.search.x + 16.0,
            y: l.search.y + 11.25,
            index: SEARCH_ICON,
        });
        let search = RectPx {
            x: l.search.x + 33.0,
            w: l.search.w - 49.0,
            ..l.search
        };
        let ink = hex_color(0x3c3a3a);
        if p.search_focus {
            // The text editor positions the caret and selection from measured glyphs.
            items.push(Item::Text(TextItem {
                inspector: 2,
                valign: VAlign::Middle,
                max_width: Some(search.w),
                ..TextItem::new(search.x, search.y + search.h / 2.0, 13.0, ink, &p.search)
            }));
            set_caret(items, &self.text_cursor);
        }
        if p.search.is_empty() || !p.search_focus {
            let value = if p.search.is_empty() {
                "Search…"
            } else {
                &p.search
            };
            items.push(Item::Label(
                label(value, search, 13.0, ink, 1).align(Place::Start, Place::Center),
            ));
        }
        items.push(Item::Shape(
            ui_shape(l.list, hex_color(0x121212)).radius(14.0).stroke(
                2.0,
                hex_color(0x787878),
                StrokeAlign::Inside,
            ),
        ));
        items.push(Item::Clip(Some(l.list_content())));
        let visible = self.catalog_visible();
        for (n, &i) in visible.iter().enumerate() {
            let row = self.catalog_row(n, l.list);
            if row.y + row.h < l.list.y || row.y > l.list.y + l.list.h {
                continue;
            }
            if i == self.selected {
                items.push(Item::Shape(
                    ui_shape(row, hex_color(0x030303)).radius(7.0).stroke(
                        2.0,
                        hex_color(SOURCE_RING),
                        StrokeAlign::Inside,
                    ),
                ));
            }
            items.push(Item::FrameIcon {
                x: row.x + 12.0,
                y: row.y + 7.0,
                index: CATALOG_ICON,
            });
            let name = RectPx {
                x: row.x + 43.0,
                y: row.y + 6.0,
                w: row.w - 49.0,
                h: 15.0,
            };
            items.push(Item::Label(label(
                &self.descriptors[i].name,
                name,
                11.0,
                hex_color(0xf0f0f0),
                0, // JetBrains Mono Medium, the bundled heavier face.
            )));
            picker_text(
                items,
                name.x,
                row.y + 20.0,
                11.0,
                &category_label(&self.categories[i]),
                hex_color(0xff8d28),
                name.w,
            );
        }
        if visible.is_empty() {
            interface_text(
                items,
                l.list.x + 15.0,
                l.list.y + 20.0,
                12.0,
                if self.descriptors.is_empty() {
                    "No processors installed."
                } else {
                    "No matching processors."
                },
                0x999999,
                l.list.w - 30.0,
            );
        }
        items.push(Item::Clip(None));
        if visible.contains(&self.selected) {
            let d = &self.descriptors[self.selected];
            let h = l.heading;
            items.push(Item::FrameIcon {
                x: h.x,
                y: h.y + 3.5,
                index: PROCESSOR_GLYPH,
            });
            picker_text(
                items,
                h.x + 23.0,
                h.y + 2.0,
                18.0,
                &d.name,
                hex_color(0xd6d6d6),
                h.w - 23.0,
            );
            interface_text(
                items,
                h.x,
                h.y + 31.0,
                12.0,
                &category_label(&self.categories[self.selected]),
                0xff8d28,
                h.w,
            );
            // Older SeedVR2 adapters omit the description shown in the catalog design.
            let description = self.info_description().or(match d.id.as_str() {
                "bytedance.seedvr2-research" => {
                    Some("Restore and upscale footage a few frames at a time.")
                }
                _ => None,
            });
            if let Some(description) = description {
                interface_text(items, h.x, h.y + 51.0, 12.0, description, 0xa0a0a0, h.w);
            }
            items.push(Item::Shape(ui_shape(
                RectPx {
                    x: h.x,
                    y: h.y + h.h,
                    w: h.w,
                    h: 1.0,
                },
                hex_color(0x303234),
            )));
            picker_text(
                items,
                l.settings.x,
                l.settings.y,
                10.0,
                "SETTINGS",
                hex_color(0xd6d6d6),
                l.settings.w,
            );
            let name = self.preset_name.as_deref().unwrap_or_else(|| {
                d.presets
                    .get(self.preset)
                    .map_or("Default", |p| p.name.as_str())
            });
            for (i, b) in l.presets().into_iter().enumerate() {
                match i {
                    0 => {
                        items.push(Item::Shape(ui_shape(b, hex_color(0x0a0a0a))));
                        for y in [b.y, b.y + b.h - 1.0] {
                            items.push(Item::Shape(ui_shape(
                                RectPx { y, h: 1.0, ..b },
                                hex_color(0x303234),
                            )));
                        }
                        if self.preset_name.is_some() {
                            items.push(Item::Text(TextItem {
                                regular: true,
                                align: Align::Center,
                                valign: VAlign::Middle,
                                max_width: Some(b.w - 8.0),
                                ..TextItem::new(
                                    b.x + b.w / 2.0,
                                    b.y + b.h / 2.0,
                                    10.0,
                                    hex_color(0xc8c8c8),
                                    name,
                                )
                            }));
                            set_caret(items, &self.text_cursor);
                        } else {
                            interface_center(items, b, name, 10.0, 0xc8c8c8);
                        }
                    }
                    _ => {
                        items.push(Item::FrameIcon {
                            x: b.x,
                            y: b.y,
                            index: PRESET_ICONS[i - 1],
                        });
                        if i == 4
                            && !d
                                .presets
                                .get(self.preset)
                                .is_some_and(|p| self.user_presets.contains_key(&p.id))
                        {
                            items.push(Item::Shape(ui_shape(b, [0.0, 0.0, 0.0, 0.42]).radius(4.0)));
                        }
                    }
                }
            }
            items.push(Item::Clip(Some(l.about)));
            picker_text(
                items,
                l.about.x,
                l.about.y,
                10.0,
                "ABOUT",
                hex_color(0xd6d6d6),
                l.about.w,
            );
            for (n, (key, value)) in [
                ("Category", category_label(&self.categories[self.selected])),
                ("Role", role_label(self.capabilities[self.selected]).into()),
                (
                    "Preview",
                    if self.supported[self.selected] {
                        "Frame preview".into()
                    } else {
                        "Not supported".into()
                    },
                ),
                (
                    "Settings",
                    format!("{} fields · {} presets", self.fields.len(), d.presets.len()),
                ),
            ]
            .into_iter()
            .enumerate()
            {
                picker_text(
                    items,
                    l.about.x,
                    l.about.y + 27.0 + n as f32 * 20.0,
                    10.0,
                    key,
                    hex_color(0x909090),
                    60.0,
                );
                items.push(Item::Label(
                    label(
                        value,
                        RectPx {
                            x: l.about.x + 65.0,
                            y: l.about.y + 27.0 + n as f32 * 20.0,
                            w: l.about.w - 65.0 - if l.panel.w >= 820.0 { 20.0 } else { 0.0 },
                            h: 13.0,
                        },
                        10.0,
                        hex_color(0xc8c8c8),
                        MONO_FONT,
                    )
                    .align(Place::End, Place::Start),
                ));
            }
            picker_text(
                items,
                l.about.x,
                l.about.y + 115.0,
                8.0,
                &d.id,
                hex_color(INSPECTOR_PATH),
                l.about.w,
            );
            let status = self.build_statuses.get(&d.id);
            if let Some(status) = status {
                interface_text(
                    items,
                    l.about.x,
                    l.about.y + 151.0,
                    10.0,
                    status,
                    0xff8d28,
                    l.about.w,
                );
            }
            let note = if self.added[self.selected] {
                "Already in your chain. Select its row in the workspace to edit it."
            } else {
                "Adds one step to your chain. You can change these settings after adding it."
            };
            let note_y = l.about.y + if status.is_some() { 170.0 } else { 151.0 };
            let note_box = RectPx {
                y: note_y,
                h: l.about.y + l.about.h - note_y,
                ..l.about
            };
            if note_box.h >= 15.0 {
                items.push(Item::Label(
                    label(note, note_box, 10.0, hex_color(0x999999), INTERFACE_FONT)
                        .size(10.0, 19.0)
                        .wrap(),
                ));
            }
            items.push(Item::Clip(None));
            self.draw_fields(items, vp, l.fields);
        }
        let footer = RectPx {
            x: r.x + 1.0,
            y: r.y + r.h - 74.0,
            w: r.w - 2.0,
            h: 73.0,
        };
        items.push(Item::Shape(
            ui_shape(footer, hex_color(0x242424)).radius(14.0),
        ));
        items.push(Item::Shape(ui_shape(
            RectPx { h: 20.0, ..footer },
            hex_color(0x242424),
        )));
        items.push(Item::Shape(ui_shape(
            RectPx { h: 1.0, ..footer },
            hex_color(0x454545),
        )));
        let enabled = visible.contains(&self.selected)
            && !self.added.get(self.selected).copied().unwrap_or(false);
        items.push(Item::Shape(
            ui_shape(l.add, hex_color(if enabled { 0x0059ff } else { 0x303238 })).radius(7.0),
        ));
        interface_center(
            items,
            l.add,
            if self.added.get(self.selected).copied().unwrap_or(false) {
                "Already added"
            } else {
                "Add Processor"
            },
            13.0,
            if enabled { 0xffffff } else { 0x888888 },
        );
        interface_center(items, l.cancel, "Cancel", 13.0, 0xd6d6d6);
        let message = if p.message.is_empty() {
            self.status.as_str()
        } else {
            &p.message
        };
        if self.editing.is_some() || self.preset_name.is_some() || !p.message.is_empty() {
            interface_text(
                items,
                footer.x + 24.0,
                footer.y + 26.0,
                11.0,
                message,
                0xff8d28,
                (l.cancel.x - footer.x - 40.0).max(1.0),
            );
        }
    }
}
fn text_center(items: &mut Vec<Item>, r: RectPx, label: &str, size: f32, color: u32) {
    items.push(Item::Text(TextItem {
        regular: true,
        align: Align::Center,
        valign: VAlign::Middle,
        max_width: Some(r.w - 8.0),
        ..TextItem::new(
            r.x + r.w / 2.0,
            r.y + r.h / 2.0,
            size,
            hex_color(color),
            label,
        )
    }));
}
const MONO_FONT: usize = 3;
const INTERFACE_FONT: usize = 2;

fn figleaf_rect(r: RectPx) -> figleaf::Rect {
    figleaf::Rect::new(r.x, r.y, r.w.max(0.0), r.h.max(0.0))
}

pub(super) fn ui_shape(r: RectPx, fill: [f32; 4]) -> figleaf::Shape {
    figleaf::Shape::rect(figleaf_rect(r)).fill(fill)
}

fn label(value: impl Into<String>, r: RectPx, size: f32, color: [f32; 4], font: usize) -> Text {
    Text::new(value, figleaf_rect(r))
        .font(font)
        .size(size, (size + 4.0).min(r.h.max(1.0)))
        .color(color)
        .text_truncation(TextTruncation::Ending)
}

pub(super) fn picker_text(
    items: &mut Vec<Item>,
    x: f32,
    y: f32,
    size: f32,
    value: &str,
    color: [f32; 4],
    w: f32,
) {
    items.push(Item::Label(label(
        value,
        RectPx {
            x,
            y,
            w,
            h: size + 4.0,
        },
        size,
        color,
        MONO_FONT,
    )));
}

pub(super) fn field_title(
    items: &mut Vec<Item>,
    row: RectPx,
    value: &str,
    ink: [f32; 4],
    in_picker: bool,
) {
    let r = RectPx {
        w: row.w - 48.0,
        h: 20.0,
        ..row
    };
    items.push(Item::Label(
        label(
            value,
            r,
            if in_picker { 10.0 } else { 11.0 },
            ink,
            MONO_FONT,
        )
        .align(Place::Start, Place::Center),
    ));
}

pub(super) fn field_label(
    items: &mut Vec<Item>,
    r: RectPx,
    value: &str,
    ink: [f32; 4],
    in_picker: bool,
) {
    items.push(Item::Label(
        label(
            value,
            r,
            10.0,
            ink,
            if in_picker { MONO_FONT } else { INTERFACE_FONT },
        )
        .align(Place::Center, Place::Center),
    ));
}

fn interface_text(
    items: &mut Vec<Item>,
    x: f32,
    y: f32,
    size: f32,
    value: &str,
    color: u32,
    w: f32,
) {
    items.push(Item::Label(label(
        value,
        RectPx {
            x,
            y,
            w,
            h: size + 4.0,
        },
        size,
        hex_color(color),
        INTERFACE_FONT,
    )));
}

fn picker_center(
    items: &mut Vec<Item>,
    r: RectPx,
    value: &str,
    size: f32,
    color: u32,
    font: usize,
) {
    let r = RectPx {
        x: r.x + 4.0,
        w: r.w - 8.0,
        ..r
    };
    items.push(Item::Label(
        label(value, r, size, hex_color(color), font).align(Place::Center, Place::Center),
    ));
}

fn interface_center(items: &mut Vec<Item>, r: RectPx, value: &str, size: f32, color: u32) {
    picker_center(items, r, value, size, color, INTERFACE_FONT);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Example(&'static str);
    impl abner_processor_api::Factory for Example {
        fn build_status(&self) -> Option<&str> {
            match self.0 {
                "example.a" => Some("  Research build  "),
                "example.c" => Some("  "),
                _ => None,
            }
        }
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: self.0.into(),
                name: self.0.into(),
                settings_schema: json!({"properties":{"amount":{"type":"number","minimum":0.0,"maximum":1.0,"default":0.5},"mode":{"type":"string","enum":["a","b"],"default":"a"}}}),
                presets: vec![],
            }
        }
        fn create(&self, _: usize, _: usize, _: &Value) -> Result<Box<dyn Session>, String> {
            Err("UI fixture does not decode".into())
        }
    }
    fn catalog() -> Panel {
        let mut r = Registry::default();
        for id in ["example.a", "example.b", "example.c"] {
            r.register(Box::new(Example(id))).unwrap();
        }
        Panel::with_presets(r, None)
    }
    #[test]
    fn build_status_follows_the_selected_processor_and_hides_unspecified_values() {
        let mut p = catalog();
        p.enter();
        p.open_picker();
        let vp = (1280.0, 800.0);
        for (id, expected) in [
            ("example.a", true),
            ("example.b", false),
            ("example.c", false),
        ] {
            let i = p.descriptors.iter().position(|d| d.id == id).unwrap();
            p.pick_candidate(i);
            let mut items = Vec::new();
            p.draw_picker(&mut items, vp);
            assert_eq!(
                items
                    .iter()
                    .any(|item| matches!(item, Item::Label(t) if t.string == "Research build")),
                expected
            );
        }
        assert_eq!(p.build_statuses.len(), 1);
    }
    #[test]
    fn populated_rail_box_contains_rows_and_does_not_cover_add_button() {
        let mut registry = Registry::default();
        for id in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"] {
            registry.register(Box::new(Example(id))).unwrap();
        }
        let mut p = Panel::with_presets(registry, None);
        p.enter();
        for count in [1, 3, 12] {
            p.added.fill(false);
            p.added[..count].fill(true);
            for vp in [(720.0, 480.0), (1280.0, 800.0)] {
                let mut items = Vec::new();
                p.draw_rail(&mut items, vp);
                let (box_at, group) = items
                    .iter()
                    .enumerate()
                    .find_map(|(i, item)| match item {
                        Item::Rect(r) if r.color == hex_color(0x121212) => Some((i, r.r)),
                        _ => None,
                    })
                    .unwrap();
                let button = p.add_button(vp);
                let button_at = items.iter().position(|item| matches!(item, Item::Rect(r)
                    if r.r.x == button.x && r.r.y == button.y && r.r.w == button.w && r.r.h == button.h)).unwrap();
                assert!(
                    box_at < button_at,
                    "the box must be painted behind Add Processor"
                );
                assert!(button.y >= group.y && button.y + button.h <= group.y + group.h);
                let list = p.list(vp);
                for i in p.visible() {
                    let row = p.card(i, vp);
                    assert!((row.y + row.h).min(list.y + list.h) <= button.y - 12.0);
                    assert!(row.y.max(list.y) >= group.y);
                    assert!((row.y + row.h).min(list.y + list.h) <= group.y + group.h);
                }
            }
        }
    }
    #[test]
    fn empty_chain_is_not_the_installed_catalog() {
        let mut p = catalog();
        p.enter();
        assert!(p.visible().is_empty());
        assert!(p.chain().is_empty());
        assert!(!p.modifies_video());
        assert_eq!(p.viewer((1280.0, 800.0)).w, 1000.0);
        let b = p.add_button((1280.0, 800.0));
        assert!(p.click(b.x + 10.0, b.y + 10.0, (1280.0, 800.0)));
        assert!(p.modal_open());
        assert_eq!(p.catalog_visible().len(), 3);
        let key = p.chain_key();
        p.pick_candidate(2);
        p.settings["amount"] = json!(0.8);
        p.changed();
        assert_eq!(p.chain_key(), key);
        assert!(p.export_setup().1.is_empty());
        p.cancel_picker();
        assert!(p.visible().is_empty());
        assert_eq!(p.saved[2]["amount"], json!(0.5));
    }
    #[test]
    fn drafts_add_in_chosen_order_and_cancel_preserves_the_live_chain() {
        let mut p = catalog();
        p.open_picker();
        p.pick_candidate(2);
        p.settings["amount"] = json!(0.8);
        p.changed();
        p.add_candidate();
        assert_eq!(
            p.chain()[0],
            ("example.c".into(), json!({"amount":0.8,"mode":"a"}))
        );
        let chain = p.chain();
        let key = p.chain_key();
        let saved = p.saved.clone();
        p.open_picker();
        p.pick_candidate(0);
        p.settings["amount"] = json!(0.1);
        p.changed();
        assert_eq!(p.export_setup().1, chain);
        assert_eq!(p.chain_key(), key);
        p.cancel_picker();
        assert_eq!(p.saved, saved);
        assert_eq!(p.chain(), chain);
        p.open_picker();
        let i = p
            .descriptors
            .iter()
            .position(|d| d.id == "example.a")
            .unwrap();
        p.pick_candidate(i);
        p.add_candidate();
        assert_eq!(
            p.chain().iter().map(|s| s.0.as_str()).collect::<Vec<_>>(),
            ["example.c", "example.a"]
        );
        // Removal is session-local; it neither removes the catalog entry nor the registry factory.
        p.remove_selected();
        assert_eq!(p.chain().len(), 1);
        assert_eq!(p.descriptors.len(), 3);
        p.remove_selected();
        assert!(p.chain().is_empty());
        assert!(p.visible().is_empty());
    }
    #[test]
    fn search_no_results_and_modal_input_never_mutate_the_chain() {
        let mut p = catalog();
        p.open_picker();
        p.text_input("example.b");
        assert_eq!(p.catalog_visible(), [1]);
        p.key(Key::Char('r'));
        assert!(p.dirty.is_none());
        assert!(p.catalog_visible().is_empty());
        p.key(Key::Enter);
        assert!(p.modal_open());
        assert!(p.chain().is_empty());
        p.edit_text(crate::text_edit::Edit::SelectAll);
        p.text_input("example");
        p.key(Key::Down);
        assert_eq!(p.selected, 2);
        p.click(20.0, 20.0, (1280.0, 800.0));
        assert!(p.modal_open());
        p.key(Key::Escape);
        assert!(!p.modal_open());
        assert!(p.visible().is_empty());
    }
    #[test]
    fn add_commits_typed_values_but_invalid_input_keeps_the_dialog_open() {
        let mut p = catalog();
        p.open_picker();
        let i = p.fields.iter().position(|f| f.key == "amount").unwrap();
        p.editing = Some((i, "not a number".into()));
        p.add_candidate();
        assert!(p.modal_open());
        assert!(p.chain().is_empty());
        p.editing = Some((i, "0.25".into()));
        p.add_candidate();
        assert!(!p.modal_open());
        assert_eq!(p.chain()[0].1["amount"], json!(0.25));
    }
    #[test]
    fn restoring_a_render_restores_membership_as_well_as_bypass() {
        let mut p = catalog();
        p.open_picker();
        p.add_candidate();
        p.toggle_enabled(0);
        p.begin_render(0.0, "frame");
        let render = p.rendering;
        p.open_picker();
        p.pick_candidate(1);
        p.add_candidate();
        assert_eq!(p.visible().len(), 2);
        p.open_render(render);
        assert_eq!(p.visible().len(), 1);
        assert!(!p.enabled[0]);
        assert!(p.export_setup().1.is_empty());
    }
    #[test]
    fn modal_geometry_keeps_controls_inside_small_and_large_windows() {
        let mut p = catalog();
        p.open_picker();
        for vp in [(720.0, 480.0), (1280.0, 800.0), (1920.0, 1080.0)] {
            let l = p.picker_layout(vp);
            for r in [
                l.panel, l.search, l.list, l.fields, l.about, l.add, l.cancel, l.close,
            ] {
                assert!(
                    r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= vp.0 && r.y + r.h <= vp.1,
                    "{vp:?}: {r:?}"
                );
            }
            assert!(l.fields.y + l.fields.h < l.add.y);
            p.scroll(10000.0, (l.fields.x + 2.0, l.fields.y + 2.0), vp);
            assert_eq!(p.scroll, 0.0);
            let mut items = vec![];
            p.draw_picker(&mut items, vp);
            assert!(
                items
                    .iter()
                    .any(|i| matches!(i,Item::Label(t) if t.string=="Add Processor"))
            );
        }
    }
    #[test]
    fn wrapped_category_chip_filters_at_its_figleaf_position() {
        let mut p = catalog();
        p.open_picker();
        let categories = [
            Category::Other("Custom processor category with a wide first label".into()),
            Category::Other("Custom processor category with a wide second label".into()),
            Category::Other("Custom processor category with a wide third label".into()),
        ];
        for (entry, category) in p.categories.iter_mut().zip(&categories) {
            *entry = vec![category.clone()];
        }
        let vp = (720.0, 480.0);
        let chips = p.picker_chips(vp);
        let (r, category, _) = chips.last().unwrap();
        assert!(r.y > chips[0].0.y, "the last chip wraps on a narrow popup");
        let layout = p.picker_layout(vp);
        assert!(r.y + r.h <= layout.body.y);
        assert!(layout.fields.y + layout.fields.h <= layout.body.y + layout.body.h);
        assert!(layout.fields.h >= 20.0);
        p.click(r.x + r.w / 2.0, r.y + r.h / 2.0, vp);
        assert_eq!(&p.filter, category);
        assert_eq!(p.catalog_visible().len(), 1);
    }

    #[test]
    fn empty_registry_dialog_stays_usable() {
        let mut p = Panel::default();
        p.open_picker();
        p.key(Key::Enter);
        p.key(Key::Down);
        assert!(p.modal_open());
        let mut items = vec![];
        p.draw_picker(&mut items, (720.0, 480.0));
        assert!(
            items
                .iter()
                .any(|i| matches!(i,Item::Label(t) if t.string=="No processors installed."))
        );
        p.key(Key::Escape);
        assert!(!p.modal_open());
    }
}
