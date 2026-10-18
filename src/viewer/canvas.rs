//! The fullscreen image surface: a texture drawn shrink-to-fit or zoomed and
//! panned, all as a GPU transform. Zoom and pan are kept in the full image's
//! coordinates, so a low-resolution preview can be swapped for the full
//! image without the view moving.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Most device pixels per image pixel when zooming in.
const MAX_ZOOM: f64 = 32.0;

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
                gdk::RGBA::new(0.5, 0.5, 0.5, 1.0)
            };
            snapshot.append_color(&background, &graphene::Rect::new(0.0, 0.0, width, height));

            let Some(texture) = self.texture.borrow().clone() else {
                return;
            };
            let (iw, ih) = self.image.get();
            if iw <= 0.0 || ih <= 0.0 {
                return;
            }
            let scale = canvas.scale();
            let (cx, cy) = canvas.clamped_center(scale);
            let rect = graphene::Rect::new(
                (f64::from(width) / 2.0 - cx * scale) as f32,
                (f64::from(height) / 2.0 - cy * scale) as f32,
                (iw * scale) as f32,
                (ih * scale) as f32,
            );
            // Shrinking: trilinear (mipmaps) keeps detail without shimmer.
            let density = f64::from(rect.width()) * f64::from(canvas.scale_factor())
                / f64::from(texture.width());
            let filter = if density < 1.0 {
                gsk::ScalingFilter::Trilinear
            } else {
                gsk::ScalingFilter::Linear
            };
            snapshot.append_scaled_texture(&texture, filter, &rect);
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
    /// Shows a new image (or a stand-in for it) at fit.
    pub fn set_image(&self, texture: Option<&gdk::Texture>, width: u32, height: u32) {
        let imp = self.imp();
        imp.texture.replace(texture.cloned());
        imp.image.set((f64::from(width), f64::from(height)));
        imp.zoom.set(None);
        self.queue_draw();
    }

    /// The same image at another resolution (or the next animation frame):
    /// zoom and pan stay where they are.
    pub fn replace_texture(&self, texture: &gdk::Texture) {
        self.imp().texture.replace(Some(texture.clone()));
        self.queue_draw();
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
        self.queue_draw();
    }

    pub fn is_zoomed(&self) -> bool {
        self.imp().zoom.get().is_some()
    }

    /// Shrink-to-fit: never larger than 100% (one image pixel per device
    /// pixel), unless filling.
    fn fit_scale(&self) -> f64 {
        let imp = self.imp();
        let (iw, ih) = imp.image.get();
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
        let (iw, ih) = imp.image.get();
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
        let imp = self.imp();
        let fit = self.fit_scale();
        let max = MAX_ZOOM / f64::from(self.scale_factor());
        let old = self.scale();
        let new = (old * factor).clamp(fit, max.max(fit));
        if new <= fit * 1.0001 {
            imp.zoom.set(None);
            self.queue_draw();
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
        self.queue_draw();
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
        self.queue_draw();
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
