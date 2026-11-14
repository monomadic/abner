//! Shared video surface and its optional top and bottom attachments.
//! Pages own media/actions; this module owns geometry and figleaf painting.
use crate::render::{Item, RectPx};
use figleaf::{Rect, Shadow, Shape, hex as hex_color};

#[derive(Clone, Copy)]
pub(crate) struct ButtonBar {
    pub r: RectPx,
}

/// A bottom attachment may span beyond the video's columns (Timeline does).
#[derive(Clone, Copy)]
pub(crate) struct TimelineAttachment {
    pub x: f32,
    pub width: f32,
    pub transport_height: f32,
    pub lanes_height: f32,
}
#[derive(Clone, Copy)]
pub(crate) struct TimelineLayout {
    pub transport: RectPx,
    pub lanes: RectPx,
}

#[derive(Clone, Copy)]
pub(crate) struct Canvas {
    pub r: RectPx,
    pub button_bar: Option<ButtonBar>,
    pub timeline: Option<TimelineLayout>,
}
impl Canvas {
    pub fn new(r: RectPx) -> Self {
        Self {
            r,
            button_bar: None,
            timeline: None,
        }
    }
    /// Solve once; drawing and pointer handlers consume these same rectangles.
    pub fn layout(
        bounds: RectPx,
        bar_height: Option<f32>,
        bottom: Option<TimelineAttachment>,
    ) -> Self {
        let bar_h = bar_height.unwrap_or(0.).clamp(0., bounds.h.max(0.));
        let remaining = (bounds.h - bar_h).max(0.);
        let timeline = bottom.map(|b| {
            let lanes_h = b.lanes_height.clamp(0., remaining);
            let transport_h = b.transport_height.clamp(0., remaining - lanes_h);
            let y = bounds.y + bounds.h - lanes_h - transport_h;
            TimelineLayout {
                transport: RectPx {
                    x: b.x,
                    y,
                    w: b.width.max(0.),
                    h: transport_h,
                },
                lanes: RectPx {
                    x: b.x,
                    y: y + transport_h,
                    w: b.width.max(0.),
                    h: lanes_h,
                },
            }
        });
        let end = timeline.map_or(bounds.y + bounds.h, |t| t.transport.y);
        Self {
            r: RectPx {
                x: bounds.x,
                y: bounds.y + bar_h,
                w: bounds.w,
                h: (end - bounds.y - bar_h).max(0.),
            },
            button_bar: bar_height.map(|_| ButtonBar {
                r: RectPx { h: bar_h, ..bounds },
            }),
            timeline,
        }
    }
    /// Preserve the document-style fit used by Timeline and Process.
    pub fn fit(&self, dims: (u32, u32)) -> RectPx {
        let c = self.r;
        let (dx, dy) = (c.w * 188. / 1140., c.h * 76.5 / 583.);
        fit_rect(
            dims,
            RectPx {
                x: c.x + dx,
                y: c.y + dy,
                w: c.w - 2. * dx,
                h: c.h - 2. * dy,
            },
        )
    }
    pub fn fit_inset(&self, dims: (u32, u32), inset: f32) -> RectPx {
        let c = self.r;
        let pad = inset.min(c.w.max(0.) / 2.).min(c.h.max(0.) / 2.);
        fit_rect(
            dims,
            RectPx {
                x: c.x + pad,
                y: c.y + pad,
                w: c.w - 2. * pad,
                h: c.h - 2. * pad,
            },
        )
    }
    pub fn picture(&self, dims: (u32, u32), view: (f32, (f32, f32))) -> RectPx {
        zoomed(self.fit(dims), view.0, view.1)
    }
    pub fn ground(&self, items: &mut Vec<Item>) {
        items.push(Item::Shape(
            Shape::rect(rect(self.r)).fill(hex_color(0x1d1b1b)),
        ));
    }
    pub fn clip(&self, items: &mut Vec<Item>) {
        items.push(Item::Clip(Some(self.r)));
    }
    pub fn lift(items: &mut Vec<Item>, picture: RectPx) {
        items.push(Item::Shape(
            Shape::rect(rect(picture)).shadow(Shadow::drop([0., 0., 0., 0.42], [0., 0.], 18.)),
        ));
    }
    pub fn recess(&self, items: &mut Vec<Item>) {
        items.push(Item::Shape(
            Shape::rect(rect(self.r)).shadow(Shadow::inner([0., 0., 0., 0.28], [0., 0.], 16.)),
        ));
    }
}
fn rect(r: RectPx) -> Rect {
    Rect::new(r.x, r.y, r.w, r.h)
}
pub(crate) fn fit_rect(dims: (u32, u32), c: RectPx) -> RectPx {
    let (w, h) = (dims.0.max(1) as f32, dims.1.max(1) as f32);
    let scale = (c.w / w).min(c.h / h).max(0.);
    let (w, h) = (w * scale, h * scale);
    RectPx {
        x: c.x + (c.w - w) / 2.,
        y: c.y + (c.h - h) / 2.,
        w,
        h,
    }
}
pub(crate) fn zoomed(base: RectPx, zoom: f32, center: (f32, f32)) -> RectPx {
    let (w, h) = (base.w * zoom, base.h * zoom);
    RectPx {
        x: base.x + base.w / 2. - center.0 * w,
        y: base.y + base.h / 2. - center.1 * h,
        w,
        h,
    }
}
impl ButtonBar {
    pub fn draw(&self, items: &mut Vec<Item>) {
        let r = self.r;
        use figleaf::{Rect, Shadow, Shape};
        let frame = Shape::rect(Rect::new(r.x, r.y, r.w, r.h)).radii([4.0, 4.0, 0.0, 0.0]);
        items.push(Item::Shape(frame.clone().fill(hex_color(0x3b3b3a))));
        items.push(Item::Shape(
            Shape::rect(Rect::new(r.x, r.y + r.h - 1.0, r.w, 1.0)).fill(hex_color(0x585454)),
        ));
        // The shadow falls over the stroke, as Figma paints it.
        items.push(Item::Shape(frame.shadow(Shadow::inner(
            [0.0, 0.0, 0.0, 0.29],
            [0.0, 2.0],
            2.9,
        ))));
    }

    pub fn button(
        items: &mut Vec<Item>,
        r: RectPx,
        label: &str,
        selected: bool,
        hovered: bool,
        disabled: bool,
    ) {
        Self::paint_button(items, r, label, selected, hovered, disabled, false);
    }
    /// Prominent action in the same bar, using the same figleaf text/shape path.
    pub fn primary(items: &mut Vec<Item>, r: RectPx, label: &str, hovered: bool, disabled: bool) {
        Self::paint_button(items, r, label, false, hovered, disabled, true);
    }
    fn paint_button(
        items: &mut Vec<Item>,
        r: RectPx,
        label: &str,
        selected: bool,
        hovered: bool,
        disabled: bool,
        primary: bool,
    ) {
        use figleaf::{Rect, Shadow, Shape, StrokeAlign, Text, paint::Align as Place};
        let selected = selected && !disabled;
        let hovered = hovered && !disabled;
        // The attached Figma bar: filled grey buttons with dark outlines; the
        // selected control is a darker, recessed surface. Values are sRGB.
        let fill = hex_color(if primary && !disabled {
            if hovered { 0x237ce5 } else { 0x0967cc }
        } else if selected {
            0x302d2b
        } else if hovered {
            0x5b5959
        } else {
            0x514f4f
        });
        // Follow figleaf's abner_button_bar example through the host's ordered
        // figleaf canvas, including CoreText labels (not the legacy quad atlas).
        let button = Shape::rect(Rect::new(r.x, r.y, r.w, r.h))
            .radius(if primary { 7.0 } else { 6.0 })
            .fill(fill)
            .stroke(
                if primary { 0.0 } else { 1.0 },
                hex_color(if selected { 0x211f1e } else { 0x383636 }),
                StrokeAlign::Inside,
            );
        items.push(Item::Shape(if selected {
            button.shadow(Shadow::inner([0.0, 0.0, 0.0, 0.22], [0.0, 2.0], 2.2))
        } else {
            button
        }));
        let ink = if disabled {
            0x858587
        } else if primary {
            0xffffff
        } else if selected || hovered {
            0xf1efed
        } else {
            0xbab8b8
        };
        let label_rect = Rect::new(r.x + 10.0, r.y, (r.w - 18.0).max(1.0), r.h);
        items.push(Item::Label(
            Text::new(label, label_rect)
                .font(1)
                .size(if primary { 13.0 } else { 12.0 }, 16.0)
                .color(hex_color(ink))
                .align(
                    if primary { Place::Center } else { Place::Start },
                    Place::Center,
                )
                .clip(label_rect),
        ));
    }
}

/// Typed host action plus its single drawing/hit-test rectangle.
pub(crate) struct Button<A> {
    pub r: RectPx,
    pub label: String,
    pub selected: bool,
    pub action: A,
    pub hint: &'static str,
}
impl<A> Button<A> {
    pub fn new(r: RectPx, label: &str, selected: bool, action: A) -> Self {
        Self {
            r,
            label: label.into(),
            selected,
            action,
            hint: "",
        }
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.r.x && y >= self.r.y && x < self.r.x + self.r.w && y < self.r.y + self.r.h
    }
    pub fn draw(&self, items: &mut Vec<Item>, hovered: bool, disabled: bool) {
        ButtonBar::button(items, self.r, &self.label, self.selected, hovered, disabled);
    }
}
impl ButtonBar {
    /// Equal-width controls with a cap, solved by figleaf auto layout.
    /// The returned rectangles are also the controls' hit targets.
    pub fn row(&self, count: usize, max_width: f32) -> Vec<RectPx> {
        use figleaf::layout::{Node, Tree};
        if count == 0 {
            return Vec::new();
        }
        let gap = 4.;
        let pad = 12f32.min(self.r.w.max(0.) / 2.);
        let width =
            ((self.r.w - 2. * pad - gap * (count - 1) as f32) / count as f32).clamp(0., max_width);
        let height = (self.r.h - 12.).max(0.);
        let mut tree = Tree::new(
            Node::row()
                .size([self.r.w, self.r.h])
                .paddings([pad, 6., pad, 6.])
                .gap(gap),
        );
        let root = tree.root();
        let ids: Vec<_> = (0..count)
            .map(|_| tree.add(root, Node::fixed([width, height])))
            .collect();
        let solved = tree
            .solve([self.r.w, self.r.h])
            .expect("finite canvas button row");
        ids.into_iter()
            .map(|id| {
                let r = solved[id];
                RectPx {
                    x: self.r.x + r.x,
                    y: self.r.y + r.y,
                    w: r.w,
                    h: r.h,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attachments_reserve_space_and_timeline_can_span_columns() {
        let bounds = RectPx {
            x: 280.,
            y: 44.,
            w: 700.,
            h: 700.,
        };
        let canvas = Canvas::layout(
            bounds,
            Some(40.),
            Some(TimelineAttachment {
                x: 0.,
                width: 1280.,
                transport_height: 50.,
                lanes_height: 198.,
            }),
        );
        let bar = canvas.button_bar.unwrap().r;
        let bottom = canvas.timeline.unwrap();
        assert_eq!(bar.y + bar.h, canvas.r.y);
        assert_eq!(canvas.r.y + canvas.r.h, bottom.transport.y);
        assert_eq!(bottom.transport.y + bottom.transport.h, bottom.lanes.y);
        assert_eq!(bottom.lanes.y + bottom.lanes.h, bounds.y + bounds.h);
        assert_eq!(bottom.lanes.w, 1280.);
        let fullscreen = Canvas::layout(bounds, None, None);
        assert_eq!(fullscreen.r.h, bounds.h);
        assert!(fullscreen.button_bar.is_none() && fullscreen.timeline.is_none());
    }
    #[test]
    fn tiny_layouts_never_produce_negative_viewports() {
        for height in [0., 20., 100., 480.] {
            let c = Canvas::layout(
                RectPx {
                    x: 0.,
                    y: 0.,
                    w: 720.,
                    h: height,
                },
                Some(40.),
                Some(TimelineAttachment {
                    x: 0.,
                    width: 720.,
                    transport_height: 50.,
                    lanes_height: 198.,
                }),
            );
            assert!(c.r.h >= 0.);
            assert!(c.r.y + c.r.h <= height);
            let fit = c.fit_inset((1920, 1080), 24.);
            assert!(fit.w >= 0. && fit.h >= 0.);
        }
    }
    #[test]
    fn button_row_draw_and_hit_share_figleaf_geometry() {
        let bar = ButtonBar {
            r: RectPx {
                x: 280.,
                y: 44.,
                w: 500.,
                h: 40.,
            },
        };
        let rects = bar.row(5, 116.);
        for (i, r) in rects.iter().enumerate() {
            let button = Button::new(*r, "Button", i == 0, i);
            assert!(button.contains(r.x + r.w / 2., r.y + r.h / 2.));
            assert!(!button.contains(r.x + r.w, r.y));
            assert!(r.x + r.w <= bar.r.x + bar.r.w);
            let mut items = Vec::new();
            button.draw(&mut items, false, false);
            assert!(matches!(items[0], Item::Shape(_)));
            assert!(matches!(items[1], Item::Label(_)));
        }
    }
}
