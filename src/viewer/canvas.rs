//! The fullscreen image surface: a texture drawn shrink-to-fit or zoomed and
//! panned, all as a GPU transform. Zoom and pan are kept in the full image's
//! coordinates, so a low-resolution preview can be swapped for the full
//! image without the view moving. A turn (quarter turns clockwise) is drawn
//! the same way, so a rotated picture shows before its file is rewritten.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Smallest zoom, as a fraction of the fit scale.
const MIN_ZOOM_OF_FIT: f64 = 0.05;

/// Most device pixels per image pixel when zooming in.
const MAX_ZOOM: f64 = 32.0;
/// The background unless B switched it to black: the grid's dark gray
/// (`@view_bg_color` in style.css, #1e1e1e).
const GRAY: f32 = 30.0 / 255.0;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Canvas {
        pub texture: RefCell<Option<gdk::Texture>>,
        /// The whole image's size; the texture may be a smaller preview.
        pub image: Cell<(f64, f64)>,
        /// Logical pixels per image pixel while zoomed; `None` means fit.
        pub zoom: Cell<Option<f64>>,
        /// The image point at the middle of the view while zoomed.
        pub center: Cell<(f64, f64)>,
        pub black: Cell<bool>,
        /// Fit by scaling up too (videos, matching mpv), not only down.
        pub fill: Cell<bool>,
        /// Quarter turns clockwise to draw the texture with.
        pub turn: Cell<u8>,
        pub on_changed: RefCell<Option<Box<dyn Fn()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Canvas {
        const NAME: &'static str = "FvCanvas";
        type Type = super::Canvas;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Canvas {}

    impl WidgetImpl for Canvas {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let canvas = self.obj();
            let (width, height) = (canvas.width() as f32, canvas.height() as f32);
            let background = if self.black.get() {
                gdk::RGBA::BLACK
            } else {
                gdk::RGBA::new(GRAY, GRAY, GRAY, 1.0)
            };
            snapshot.append_color(&background, &graphene::Rect::new(0.0, 0.0, width, height));

            let Some(texture) = self.texture.borrow().clone() else {
                return;
            };
            let Some(shown) = canvas.image_rect() else {
                return;
            };
            let (iw, ih) = self.image.get();
            let scale = canvas.scale();
            // The texture's own rectangle, turned about the shown one's center.
            let (tw, th) = ((iw * scale) as f32, (ih * scale) as f32);
            let rect = graphene::Rect::new(-tw / 2.0, -th / 2.0, tw, th);
            // Shrinking: trilinear (mipmaps) keeps detail without shimmer.
            let density =
                f64::from(tw) * f64::from(canvas.scale_factor()) / f64::from(texture.width());
            let filter = if density < 1.0 {
                gsk::ScalingFilter::Trilinear
            } else {
                gsk::ScalingFilter::Linear
            };
            snapshot.save();
            snapshot.translate(&graphene::Point::new(
                shown.x() + shown.width() / 2.0,
                shown.y() + shown.height() / 2.0,
            ));
            snapshot.rotate(90.0 * f32::from(self.turn.get()));
            snapshot.append_scaled_texture(&texture, filter, &rect);
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct Canvas(ObjectSubclass<imp::Canvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Canvas {
    fn default() -> Self {
        glib::Object::builder()
            .property("hexpand", true)
            .property("vexpand", true)
            .build()
    }
}

impl Canvas {
    /// Called when the zoom changes (or the image, which starts at fit).
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.imp().on_changed.replace(Some(Box::new(f)));
    }

    fn changed(&self) {
        self.queue_draw();
        if let Some(f) = self.imp().on_changed.borrow().as_ref() {
            f();
        }
    }

    /// Shows a new image (or a stand-in for it) at fit, unturned. `width` x
    /// `height` is the whole image's size, which the texture may be smaller than.
    pub fn set_image(&self, texture: Option<&gdk::Texture>, width: u32, height: u32) {
        let imp = self.imp();
        imp.texture.replace(texture.cloned());
        imp.image.set((f64::from(width), f64::from(height)));
        imp.zoom.set(None);
        imp.turn.set(0);
        self.changed();
    }

    /// Draws the image `turn` quarter turns clockwise, at fit.
    pub fn set_turn(&self, turn: u8) {
        let imp = self.imp();
        if imp.turn.replace(turn % 4) != turn % 4 {
            imp.zoom.set(None);
            self.changed();
        }
    }

    pub fn turn(&self) -> u8 {
        self.imp().turn.get()
    }

    /// The whole image's size as drawn (turned).
    pub fn shown_size(&self) -> (f64, f64) {
        let imp = self.imp();
        let (width, height) = imp.image.get();
        if imp.turn.get() % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// Where the image is drawn, in the canvas's coordinates.
    pub fn image_rect(&self) -> Option<graphene::Rect> {
        let (iw, ih) = self.shown_size();
        if iw <= 0.0 || ih <= 0.0 {
            return None;
        }
        let scale = self.scale();
        let (cx, cy) = self.clamped_center(scale);
        Some(graphene::Rect::new(
            (f64::from(self.width()) / 2.0 - cx * scale) as f32,
            (f64::from(self.height()) / 2.0 - cy * scale) as f32,
            (iw * scale) as f32,
            (ih * scale) as f32,
        ))
    }

    /// The zoom in percent: 100 is one image pixel per screen pixel.
    pub fn zoom_percent(&self) -> f64 {
        self.scale() * f64::from(self.scale_factor()) * 100.0
    }

    /// The same image at another resolution (or the next animation frame):
    /// zoom and pan stay where they are.
    pub fn replace_texture(&self, texture: &gdk::Texture) {
        self.imp().texture.replace(Some(texture.clone()));
        self.queue_draw();
    }

    /// Whether `width` x `height` turned `turn` times draws at the size the
    /// image has now (so zoom and pan carry over to it).
    pub fn same_layout(&self, width: u32, height: u32, turn: u8) -> bool {
        let (w, h) = if turn % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        };
        self.shown_size() == (f64::from(w), f64::from(h))
    }

    /// The same image again, maybe decoded at another resolution or turned
    /// another way (a turned picture's file was rewritten): zoom and pan stay
    /// if it draws at the same size, else it starts at fit.
    pub fn replace_image(&self, texture: &gdk::Texture, width: u32, height: u32, turn: u8) {
        let imp = self.imp();
        let same = self.same_layout(width, height, turn);
        imp.texture.replace(Some(texture.clone()));
        imp.image.set((f64::from(width), f64::from(height)));
        imp.turn.set(turn % 4);
        if same {
            self.queue_draw();
        } else {
            imp.zoom.set(None);
            self.changed();
        }
    }

    pub fn set_black(&self, black: bool) {
        self.imp().black.set(black);
        self.queue_draw();
    }

    pub fn is_black(&self) -> bool {
        self.imp().black.get()
    }

    /// Videos fit the screen like mpv draws them, small ones scaled up.
    pub fn set_fill(&self, fill: bool) {
        self.imp().fill.set(fill);
        self.changed();
    }

    pub fn is_zoomed(&self) -> bool {
        self.imp().zoom.get().is_some()
    }

    /// Shrink-to-fit: never larger than 100% (one image pixel per device
    /// pixel), unless filling.
    fn fit_scale(&self) -> f64 {
        let imp = self.imp();
        let (iw, ih) = self.shown_size();
        let actual = 1.0 / f64::from(self.scale_factor());
        if iw <= 0.0 || ih <= 0.0 {
            return actual;
        }
        let fit = (f64::from(self.width()) / iw).min(f64::from(self.height()) / ih);
        if imp.fill.get() { fit } else { fit.min(actual) }
    }

    fn scale(&self) -> f64 {
        self.imp().zoom.get().unwrap_or_else(|| self.fit_scale())
    }

    /// The view's center in image coordinates, kept so the image covers the
    /// view where it can and stays centered where it is smaller.
    fn clamped_center(&self, scale: f64) -> (f64, f64) {
        let imp = self.imp();
        let (iw, ih) = self.shown_size();
        if imp.zoom.get().is_none() {
            return (iw / 2.0, ih / 2.0);
        }
        let (cx, cy) = imp.center.get();
        let clamp = |center: f64, length: f64, view: f64| {
            let half = view / 2.0 / scale;
            if length <= 2.0 * half {
                length / 2.0
            } else {
                center.clamp(half, length - half)
            }
        };
        (
            clamp(cx, iw, f64::from(self.width())),
            clamp(cy, ih, f64::from(self.height())),
        )
    }

    /// Zooms by `factor`, keeping the image point under (x, y) in place.
    pub fn zoom_at(&self, factor: f64, x: f64, y: f64) {
        let fit = self.fit_scale();
        let old = self.scale();
        let mut new = old * factor;
        // Passing through fit stops there once, so fit is easy to land on.
        if (old < fit * 0.9999 && new > fit) || (old > fit * 1.0001 && new < fit) {
            new = fit;
        }
        self.zoom_to(new, x, y);
    }

    /// Zooms to `percent` (100 = one image pixel per screen pixel) around
    /// the middle of the view.
    pub fn zoom_percent_to(&self, percent: f64) {
        let scale = percent / 100.0 / f64::from(self.scale_factor());
        self.zoom_to(
            scale,
            f64::from(self.width()) / 2.0,
            f64::from(self.height()) / 2.0,
        );
    }

    /// Sets the zoom to `new` logical pixels per image pixel, keeping the
    /// image point under (x, y) in place.
    fn zoom_to(&self, new: f64, x: f64, y: f64) {
        let imp = self.imp();
        let fit = self.fit_scale();
        let max = MAX_ZOOM / f64::from(self.scale_factor());
        let old = self.scale();
        let new = new.clamp(fit * MIN_ZOOM_OF_FIT, max.max(fit));
        if (new / fit - 1.0).abs() < 0.0001 {
            imp.zoom.set(None);
            self.changed();
            return;
        }
        let (cx, cy) = self.clamped_center(old);
        let (dx, dy) = (
            x - f64::from(self.width()) / 2.0,
            y - f64::from(self.height()) / 2.0,
        );
        let (px, py) = (cx + dx / old, cy + dy / old);
        imp.zoom.set(Some(new));
        imp.center.set((px - dx / new, py - dy / new));
        imp.center.set(self.clamped_center(new));
        self.changed();
    }

    /// Click: 100% around the point, or back to fit.
    pub fn toggle_actual_size(&self, x: f64, y: f64) {
        if self.is_zoomed() {
            self.fit();
            return;
        }
        let actual = 1.0 / f64::from(self.scale_factor());
        let fit = self.fit_scale();
        if fit < actual {
            self.zoom_at(actual / fit, x, y);
        }
    }

    pub fn fit(&self) {
        self.imp().zoom.set(None);
        self.changed();
    }

    /// Drag by (dx, dy) logical pixels while zoomed.
    pub fn pan(&self, dx: f64, dy: f64) {
        let imp = self.imp();
        let Some(scale) = imp.zoom.get() else {
            return;
        };
        let (cx, cy) = self.clamped_center(scale);
        imp.center.set((cx - dx / scale, cy - dy / scale));
        imp.center.set(self.clamped_center(scale));
        self.queue_draw();
    }

    /// Whether the shown texture is too coarse for the current zoom.
    pub fn needs_more_detail(&self) -> bool {
        let imp = self.imp();
        let Some(texture) = imp.texture.borrow().clone() else {
            return false;
        };
        let (iw, _) = imp.image.get();
        let device_per_image = self.scale() * f64::from(self.scale_factor());
        let texture_per_image = f64::from(texture.width()) / iw.max(1.0);
        device_per_image > texture_per_image * 1.05
    }
}
