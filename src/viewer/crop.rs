//! The crop frame over the picture: the area outside it dimmed, handles on
//! its corners and edges. Dragging a handle resizes it, dragging inside
//! moves it, dragging outside draws a new one; an aspect ratio, when set,
//! holds while resizing. The frame is kept in the picture's pixels (as
//! displayed), so its size reads in real pixels.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene};

/// How close (logical pixels) the pointer must be to grab an edge or corner.
const GRAB: f64 = 14.0;
/// The smallest frame, in screen pixels.
const MIN_SCREEN: f64 = 24.0;

/// A rectangle in picture pixels (fractional while dragging).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Area {
    fn right(&self) -> f64 {
        self.x + self.width
    }
    fn bottom(&self) -> f64 {
        self.y + self.height
    }
}

/// A drag in progress: what it holds, the area and the point it started at.
#[derive(Clone, Copy, Debug)]
struct Drag {
    grab: Grab,
    start: Area,
    point: (f64, f64),
}

/// What a drag holds: which edges move, the whole frame, or a new frame.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Grab {
    Edges {
        left: bool,
        right: bool,
        top: bool,
        bottom: bool,
    },
    Move,
    New,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct CropView {
        /// Where the picture is drawn, in our coordinates.
        pub frame: Cell<Option<graphene::Rect>>,
        /// The picture's size in pixels (as displayed).
        pub size: Cell<(f64, f64)>,
        pub area: Cell<Area>,
        /// Width / height to keep, or free.
        pub aspect: Cell<Option<f64>>,
        pub(super) drag: Cell<Option<Drag>>,
        /// Saving: the frame stays, without handles, and takes no input.
        pub frozen: Cell<bool>,
        pub on_changed: RefCell<Option<Box<dyn Fn()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CropView {
        const NAME: &'static str = "FvCropView";
        type Type = super::CropView;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for CropView {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().connect_input();
        }
    }

    impl WidgetImpl for CropView {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().draw(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct CropView(ObjectSubclass<imp::CropView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for CropView {
    fn default() -> Self {
        glib::Object::builder()
            .property("hexpand", true)
            .property("vexpand", true)
            .property("visible", false)
            .build()
    }
}

impl CropView {
    /// Starts cropping a `size` picture drawn at `frame`, with all of it selected.
    pub fn start(&self, frame: graphene::Rect, size: (f64, f64)) {
        let imp = self.imp();
        imp.frame.set(Some(frame));
        imp.size.set(size);
        imp.aspect.set(None);
        imp.drag.set(None);
        imp.frozen.set(false);
        imp.area.set(Area {
            x: 0.0,
            y: 0.0,
            width: size.0,
            height: size.1,
        });
        self.changed();
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.imp().on_changed.replace(Some(Box::new(f)));
    }

    fn changed(&self) {
        self.queue_draw();
        if let Some(f) = self.imp().on_changed.borrow().as_ref() {
            f();
        }
    }

    pub fn set_frozen(&self, frozen: bool) {
        self.imp().frozen.set(frozen);
        self.imp().drag.set(None);
        self.set_cursor(None);
        self.queue_draw();
    }

    /// The area in whole pixels: (x, y, width, height).
    pub fn rect(&self) -> (u32, u32, u32, u32) {
        let imp = self.imp();
        let area = imp.area.get();
        let (width, height) = imp.size.get();
        let x0 = area.x.round().clamp(0.0, width);
        let y0 = area.y.round().clamp(0.0, height);
        let x1 = area.right().round().clamp(x0 + 1.0, width.max(x0 + 1.0));
        let y1 = area.bottom().round().clamp(y0 + 1.0, height.max(y0 + 1.0));
        (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32)
    }

    /// Whether the frame covers the whole picture (nothing to crop).
    pub fn is_whole(&self) -> bool {
        let (width, height) = self.imp().size.get();
        self.rect() == (0, 0, width.round() as u32, height.round() as u32)
    }

    /// Keeps `aspect` (width / height) from now on; the frame becomes the
    /// largest such area, centered in the picture. `None` frees it.
    pub fn set_aspect(&self, aspect: Option<f64>) {
        let imp = self.imp();
        imp.aspect.set(aspect);
        if let Some(aspect) = aspect {
            let (width, height) = imp.size.get();
            let (w, h) = if width / height > aspect {
                (height * aspect, height)
            } else {
                (width, width / aspect)
            };
            imp.area.set(Area {
                x: (width - w) / 2.0,
                y: (height - h) / 2.0,
                width: w,
                height: h,
            });
        }
        self.changed();
    }

    /// Screen pixels per picture pixel.
    fn scale(&self) -> f64 {
        let imp = self.imp();
        match imp.frame.get() {
            Some(frame) if imp.size.get().0 > 0.0 => f64::from(frame.width()) / imp.size.get().0,
            _ => 1.0,
        }
    }

    /// The area on screen: (left, top, right, bottom).
    fn on_screen(&self) -> Option<(f64, f64, f64, f64)> {
        let frame = self.imp().frame.get()?;
        let area = self.imp().area.get();
        let scale = self.scale();
        let (fx, fy) = (f64::from(frame.x()), f64::from(frame.y()));
        Some((
            fx + area.x * scale,
            fy + area.y * scale,
            fx + area.right() * scale,
            fy + area.bottom() * scale,
        ))
    }

    fn grab_at(&self, x: f64, y: f64) -> Grab {
        let Some((left, top, right, bottom)) = self.on_screen() else {
            return Grab::New;
        };
        let along_x = x > left - GRAB && x < right + GRAB;
        let along_y = y > top - GRAB && y < bottom + GRAB;
        let near = |a: f64, b: f64| (a - b).abs() <= GRAB;
        // A small frame: grab the nearer of two edges that are both in reach.
        let (near_left, near_right) = if right - left < 2.0 * GRAB {
            let middle = (left + right) / 2.0;
            (near(x, left) && x < middle, near(x, right) && x >= middle)
        } else {
            (near(x, left), near(x, right))
        };
        let (near_top, near_bottom) = if bottom - top < 2.0 * GRAB {
            let middle = (top + bottom) / 2.0;
            (near(y, top) && y < middle, near(y, bottom) && y >= middle)
        } else {
            (near(y, top), near(y, bottom))
        };
        let edges = Grab::Edges {
            left: near_left && along_y,
            right: near_right && along_y,
            top: near_top && along_x,
            bottom: near_bottom && along_x,
        };
        match edges {
            Grab::Edges {
                left: false,
                right: false,
                top: false,
                bottom: false,
            } if x > left && x < right && y > top && y < bottom => Grab::Move,
            Grab::Edges {
                left: false,
                right: false,
                top: false,
                bottom: false,
            } => Grab::New,
            edges => edges,
        }
    }

    fn cursor_for(grab: Grab) -> &'static str {
        match grab {
            Grab::Move => "move",
            Grab::New => "crosshair",
            Grab::Edges {
                left,
                right,
                top,
                bottom,
            } => match (left || right, top || bottom) {
                (true, true) if (left && top) || (right && bottom) => "nwse-resize",
                (true, true) => "nesw-resize",
                (true, false) => "ew-resize",
                _ => "ns-resize",
            },
        }
    }

    fn connect_input(&self) {
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, x, y| {
                if view.imp().frozen.get() || view.imp().drag.get().is_some() {
                    return;
                }
                view.set_cursor_from_name(Some(Self::cursor_for(view.grab_at(x, y))));
            }
        ));
        self.add_controller(motion);

        let drag = gtk::GestureDrag::new();
        drag.connect_drag_begin(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |gesture, x, y| {
                let imp = view.imp();
                if imp.frozen.get() || imp.frame.get().is_none() {
                    gesture.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                // Ours alone: the viewer's own drag would pan, its click zoom.
                gesture.set_state(gtk::EventSequenceState::Claimed);
                imp.drag.set(Some(Drag {
                    grab: view.grab_at(x, y),
                    start: imp.area.get(),
                    point: (x, y),
                }));
                view.queue_draw();
            }
        ));
        drag.connect_drag_update(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, dx, dy| view.dragged(dx, dy)
        ));
        drag.connect_drag_end(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, _, _| {
                view.imp().drag.set(None);
                view.changed();
            }
        ));
        self.add_controller(drag);
    }

    fn dragged(&self, dx: f64, dy: f64) {
        let imp = self.imp();
        let Some(Drag {
            grab,
            start,
            point: (px, py),
        }) = imp.drag.get()
        else {
            return;
        };
        let Some(frame) = imp.frame.get() else {
            return;
        };
        let scale = self.scale();
        let (dx, dy) = (dx / scale, dy / scale);
        let (width, height) = imp.size.get();
        let min = (MIN_SCREEN / scale).min(width).min(height);
        let aspect = imp.aspect.get();
        let area = match grab {
            Grab::Move => Area {
                x: (start.x + dx).clamp(0.0, width - start.width),
                y: (start.y + dy).clamp(0.0, height - start.height),
                ..start
            },
            Grab::Edges {
                left,
                right,
                top,
                bottom,
            } => resize(
                start,
                (left, right, top, bottom),
                (dx, dy),
                aspect,
                (width, height),
                min,
            ),
            Grab::New => {
                // A frame from the starting point, growing the way the pointer goes.
                let x = (px - f64::from(frame.x())) / scale;
                let y = (py - f64::from(frame.y())) / scale;
                let point = Area {
                    x: x.clamp(0.0, width),
                    y: y.clamp(0.0, height),
                    width: 0.0,
                    height: 0.0,
                };
                let edges = (dx < 0.0, dx >= 0.0, dy < 0.0, dy >= 0.0);
                resize(point, edges, (dx, dy), aspect, (width, height), min)
            }
        };
        imp.area.set(area);
        self.changed();
    }

    fn draw(&self, snapshot: &gtk::Snapshot) {
        let Some((left, top, right, bottom)) = self.on_screen() else {
            return;
        };
        let imp = self.imp();
        let (w, h) = (f64::from(self.width()), f64::from(self.height()));
        let rect = |x: f64, y: f64, width: f64, height: f64| {
            graphene::Rect::new(
                x as f32,
                y as f32,
                width.max(0.0) as f32,
                height.max(0.0) as f32,
            )
        };
        // Dim everything outside the frame.
        let dim = gdk::RGBA::new(0.0, 0.0, 0.0, 0.6);
        snapshot.append_color(&dim, &rect(0.0, 0.0, w, top));
        snapshot.append_color(&dim, &rect(0.0, bottom, w, h - bottom));
        snapshot.append_color(&dim, &rect(0.0, top, left, bottom - top));
        snapshot.append_color(&dim, &rect(right, top, w - right, bottom - top));

        let line = gdk::RGBA::new(1.0, 1.0, 1.0, 0.95);
        // A dark edge around the white marks keeps them visible on light pictures.
        let edge = gdk::RGBA::new(0.0, 0.0, 0.0, 0.5);
        let (fw, fh) = (right - left, bottom - top);
        // The frame, 1 px inside the area.
        let mut marks = vec![
            (left, top, fw, 1.0),
            (left, bottom - 1.0, fw, 1.0),
            (left, top, 1.0, fh),
            (right - 1.0, top, 1.0, fh),
        ];
        // Thirds while dragging, for composition.
        if !imp.frozen.get() && imp.drag.get().is_some() {
            let faint = gdk::RGBA::new(1.0, 1.0, 1.0, 0.4);
            for i in 1..3 {
                let t = f64::from(i) / 3.0;
                snapshot.append_color(&faint, &rect(left + fw * t, top, 1.0, fh));
                snapshot.append_color(&faint, &rect(left, top + fh * t, fw, 1.0));
            }
        }
        if !imp.frozen.get() {
            // Corner brackets and edge ticks, 3 px thick, inside the frame.
            let arm = 18.0_f64.min(fw / 2.0).min(fh / 2.0);
            let t = 3.0;
            for (x, y, sx, sy) in [
                (left, top, 1.0, 1.0),
                (right, top, -1.0, 1.0),
                (left, bottom, 1.0, -1.0),
                (right, bottom, -1.0, -1.0),
            ] {
                let x0 = if sx > 0.0 { x } else { x - arm };
                let y0 = if sy > 0.0 { y } else { y - t };
                marks.push((x0, y0, arm, t));
                let x0 = if sx > 0.0 { x } else { x - t };
                let y0 = if sy > 0.0 { y } else { y - arm };
                marks.push((x0, y0, t, arm));
            }
            let tick = 18.0_f64.min(fw / 4.0).min(fh / 4.0);
            let (mx, my) = ((left + right) / 2.0, (top + bottom) / 2.0);
            marks.push((mx - tick / 2.0, top, tick, t));
            marks.push((mx - tick / 2.0, bottom - t, tick, t));
            marks.push((left, my - tick / 2.0, t, tick));
            marks.push((right - t, my - tick / 2.0, t, tick));
        }
        for &(x, y, w, h) in &marks {
            snapshot.append_color(&edge, &rect(x - 1.0, y - 1.0, w + 2.0, h + 2.0));
        }
        for &(x, y, w, h) in &marks {
            snapshot.append_color(&line, &rect(x, y, w, h));
        }
    }
}

/// `start` with the given edges (left, right, top, bottom) moved by `delta`,
/// at least `min` across and inside `bounds`, keeping `aspect` if set: the
/// opposite corner (or edge, centered) stays put.
fn resize(
    start: Area,
    (left, right, top, bottom): (bool, bool, bool, bool),
    (dx, dy): (f64, f64),
    aspect: Option<f64>,
    (width, height): (f64, f64),
    min: f64,
) -> Area {
    let (mut x0, mut y0, mut x1, mut y1) = (start.x, start.y, start.right(), start.bottom());
    if left {
        x0 = (x0 + dx).clamp(0.0, (x1 - min).max(0.0));
    }
    if right {
        x1 = (x1 + dx).clamp((x0 + min).min(width), width);
    }
    if top {
        y0 = (y0 + dy).clamp(0.0, (y1 - min).max(0.0));
    }
    if bottom {
        y1 = (y1 + dy).clamp((y0 + min).min(height), height);
    }
    let Some(aspect) = aspect else {
        return Area {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        };
    };
    // Which way each side grows from the fixed one: +1 right/down, -1
    // left/up, 0 both ways around the starting center.
    let horizontal = left || right;
    let vertical = top || bottom;
    let (ax, gx) = match (left, right) {
        (true, false) => (start.right(), -1.0),
        (false, true) => (start.x, 1.0),
        _ => (start.x + start.width / 2.0, 0.0),
    };
    let (ay, gy) = match (top, bottom) {
        (true, false) => (start.bottom(), -1.0),
        (false, true) => (start.y, 1.0),
        _ => (start.y + start.height / 2.0, 0.0),
    };
    let (w, h) = (x1 - x0, y1 - y0);
    let (mut w, mut h) = if horizontal && !vertical {
        (w, w / aspect)
    } else if vertical && !horizontal {
        (h * aspect, h)
    } else if w / aspect >= h {
        (w, w / aspect)
    } else {
        (h * aspect, h)
    };
    // Room from the fixed side to the picture's edges.
    let room = |anchor: f64, grow: f64, length: f64| match grow {
        g if g > 0.0 => length - anchor,
        g if g < 0.0 => anchor,
        _ => 2.0 * anchor.min(length - anchor),
    };
    let fit = (room(ax, gx, width) / w)
        .min(room(ay, gy, height) / h)
        .min(1.0);
    w *= fit;
    h *= fit;
    // Not below the smallest size, unless the picture can't fit it.
    let grow = (min / w).max(min / h).max(1.0);
    let fit_back = (room(ax, gx, width) / (w * grow))
        .min(room(ay, gy, height) / (h * grow))
        .min(1.0);
    w *= grow * fit_back;
    h *= grow * fit_back;
    let place = |anchor: f64, grow: f64, length: f64| match grow {
        g if g > 0.0 => anchor,
        g if g < 0.0 => anchor - length,
        _ => anchor - length / 2.0,
    };
    Area {
        x: place(ax, gx, w),
        y: place(ay, gy, h),
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: Area = Area {
        x: 100.0,
        y: 100.0,
        width: 400.0,
        height: 300.0,
    };

    #[test]
    fn free_resizing_stays_inside() {
        let area = resize(
            START,
            (false, true, false, false),
            (5000.0, 0.0),
            None,
            (1000.0, 800.0),
            10.0,
        );
        assert_eq!(area.right(), 1000.0);
        let area = resize(
            START,
            (true, false, true, false),
            (500.0, 500.0),
            None,
            (1000.0, 800.0),
            10.0,
        );
        assert_eq!((area.width, area.height), (10.0, 10.0));
    }

    #[test]
    fn aspect_holds_with_the_opposite_corner_fixed() {
        // Dragging the bottom-right corner keeps the top-left one.
        let area = resize(
            START,
            (false, true, false, true),
            (200.0, 0.0),
            Some(1.0),
            (1000.0, 800.0),
            10.0,
        );
        assert_eq!((area.x, area.y), (100.0, 100.0));
        assert_eq!(area.width, area.height);
        assert_eq!(area.width, 600.0);
        // Too big for the picture: shrunk to fit, still square.
        let area = resize(
            START,
            (false, true, false, true),
            (5000.0, 0.0),
            Some(1.0),
            (1000.0, 800.0),
            10.0,
        );
        assert_eq!(area.width, area.height);
        assert!(area.right() <= 1000.0 && area.bottom() <= 800.0);
        // An edge: the other side follows, around the middle, as far as the
        // picture allows (300 px to its left edge).
        let area = resize(
            START,
            (false, false, false, true),
            (0.0, 100.0),
            Some(2.0),
            (1000.0, 800.0),
            10.0,
        );
        assert_eq!((area.width, area.height), (600.0, 300.0));
        assert_eq!(area.x + area.width / 2.0, 300.0);
        let area = resize(
            START,
            (false, false, false, true),
            (0.0, -50.0),
            Some(2.0),
            (1000.0, 800.0),
            10.0,
        );
        assert_eq!((area.width, area.height), (500.0, 250.0));
    }
}
