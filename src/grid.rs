//! Virtualized thumbnail grid. Each tile is a square thumbnail area with the
//! filename underneath and a play badge on videos. Tiles tell the
//! thumbnailer what they show; textures are drawn GPU-scaled to the tile.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::loader::Thumbnailer;
use crate::media::Kind;
use crate::model::FileItem;

pub const MIN_SIZE: i32 = 48;
pub const MAX_SIZE: i32 = 512;
/// Size factor per Ctrl+wheel notch.
const ZOOM_STEP: f64 = 1.1;
/// Horizontal padding around each tile; must match `.fv-grid > child` in style.css.
const TILE_PADDING: i32 = 12;

pub struct Grid {
    pub view: gtk::GridView,
    pub scroller: gtk::ScrolledWindow,
    tiles: Rc<RefCell<Vec<glib::WeakRef<Tile>>>>,
    size: Rc<Cell<i32>>,
    width: Rc<Cell<f64>>,
    thumbnailer: Rc<Thumbnailer>,
}

impl Grid {
    /// `size` drives the thumbnail edge length; Ctrl+wheel over the grid changes it.
    pub fn new(
        model: &impl IsA<gtk::SelectionModel>,
        size: &gtk::Adjustment,
        thumbnailer: &Rc<Thumbnailer>,
    ) -> Self {
        let current = Rc::new(Cell::new(size.value().round() as i32));
        let tiles: Rc<RefCell<Vec<glib::WeakRef<Tile>>>> = Rc::default();

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(glib::clone!(
            #[strong]
            current,
            #[strong]
            tiles,
            #[weak]
            thumbnailer,
            move |_, item| {
                let item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("grid list item");
                let tile = Tile::new(current.get());
                tiles.borrow_mut().push(tile.downgrade());
                // Positions are only known once GTK maps a tile on screen.
                tile.connect_map(glib::clone!(
                    #[weak]
                    thumbnailer,
                    move |_| thumbnailer.queue_sync()
                ));
                let thumb = tile.thumb();
                for property in ["texture", "native", "failed"] {
                    item.property_expression("item")
                        .chain_property::<FileItem>(property)
                        .bind(&thumb, property, Some(item));
                }
                item.set_child(Some(&tile));
            }
        ));
        factory.connect_bind(glib::clone!(
            #[weak]
            thumbnailer,
            move |_, item| {
                let item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("grid list item");
                if let (Some(tile), Some(file)) = (
                    item.child().and_downcast::<Tile>(),
                    item.item().and_downcast::<FileItem>(),
                ) {
                    tile.bind(&file);
                    thumbnailer.bind(&tile, item, &file);
                }
            }
        ));
        factory.connect_unbind(glib::clone!(
            #[weak]
            thumbnailer,
            move |_, item| {
                let item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("grid list item");
                if let Some(file) = item.item().and_downcast::<FileItem>() {
                    thumbnailer.unbind(&file);
                }
            }
        ));

        let view = gtk::GridView::builder()
            .model(model)
            .factory(&factory)
            .max_columns(1)
            .enable_rubberband(true)
            .css_classes(["fv-grid"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&view)
            .hexpand(true)
            .vexpand(true)
            .build();

        // Ctrl+wheel resizes thumbnails; the plain wheel keeps scrolling.
        let zoom = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        zoom.set_propagation_phase(gtk::PropagationPhase::Capture);
        zoom.connect_scroll(glib::clone!(
            #[weak]
            size,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |controller, _dx, dy| {
                let state = controller.current_event_state();
                if !state.contains(gdk::ModifierType::CONTROL_MASK) {
                    return glib::Propagation::Proceed;
                }
                // Touchpads report pixels; treat ~40 px as one wheel notch.
                let notches = match controller.unit() {
                    gdk::ScrollUnit::Wheel => dy,
                    _ => dy / 40.0,
                };
                size.set_value(size.value() * ZOOM_STEP.powf(-notches));
                glib::Propagation::Stop
            }
        ));
        scroller.add_controller(zoom);
        thumbnailer.set_viewport(&scroller);
        thumbnailer.set_size(current.get());
        scroller.vadjustment().connect_value_changed(glib::clone!(
            #[weak]
            thumbnailer,
            move |adj| thumbnailer.scrolled(adj.value())
        ));

        // GTK pre-creates tile widgets for max_columns x ~30 rows, so a generous
        // fixed max_columns builds a widget for every item and defeats
        // virtualization. Keep it at the number of columns that actually fit.
        let width = Rc::new(Cell::new(0.0));
        scroller.hadjustment().connect_changed(glib::clone!(
            #[weak]
            view,
            #[strong]
            width,
            #[strong]
            current,
            move |adj| {
                if width.replace(adj.page_size()) == adj.page_size() {
                    return;
                }
                // This fires during allocation; changing columns then would re-enter layout.
                let (width, size) = (adj.page_size(), current.get());
                glib::idle_add_local_once(move || fit_columns(&view, width, size));
            }
        ));

        Self {
            view,
            scroller,
            tiles,
            size: current,
            width,
            thumbnailer: thumbnailer.clone(),
        }
    }

    /// Resizes every live tile; the grid relayouts on the next frame.
    pub fn set_size(&self, size: i32) {
        if self.size.replace(size) == size {
            return;
        }
        fit_columns(&self.view, self.width.get(), size);
        self.thumbnailer.set_size(size);
        self.thumbnailer.queue_sync();
        self.tiles.borrow_mut().retain(|weak| match weak.upgrade() {
            Some(tile) => {
                tile.set_size(size);
                true
            }
            None => false,
        });
    }
}

fn fit_columns(view: &gtk::GridView, width: f64, size: i32) {
    let columns = (width / f64::from(size + TILE_PADDING)).floor().max(1.0) as u32;
    if view.max_columns() != columns {
        view.set_max_columns(columns);
    }
}

mod tile_imp {
    use std::cell::OnceCell;

    use super::*;

    #[derive(Default)]
    pub struct Tile {
        pub thumb: OnceCell<super::Thumb>,
        pub label: OnceCell<gtk::Label>,
        pub badge: OnceCell<gtk::Image>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Tile {
        const NAME: &'static str = "FvTile";
        type Type = super::Tile;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for Tile {
        fn constructed(&self) {
            self.parent_constructed();
            let tile = self.obj();
            tile.set_orientation(gtk::Orientation::Vertical);
            tile.set_spacing(4);
            tile.add_css_class("fv-tile");

            let thumb = super::Thumb::default();
            let badge = gtk::Image::builder()
                .icon_name("media-playback-start-symbolic")
                .pixel_size(12)
                .halign(gtk::Align::End)
                .valign(gtk::Align::End)
                .margin_end(6)
                .margin_bottom(6)
                .css_classes(["fv-badge"])
                .visible(false)
                .build();
            let overlay = gtk::Overlay::builder()
                .child(&thumb)
                .halign(gtk::Align::Center)
                .build();
            overlay.add_overlay(&badge);
            // A one-char max width keeps long names from widening the column;
            // the label still fills the tile and ellipsizes.
            let label = gtk::Label::builder()
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(1)
                .hexpand(true)
                .build();
            tile.append(&overlay);
            tile.append(&label);

            let _ = self.thumb.set(thumb);
            let _ = self.label.set(label);
            let _ = self.badge.set(badge);
        }
    }

    impl WidgetImpl for Tile {}
    impl BoxImpl for Tile {}
}

glib::wrapper! {
    pub struct Tile(ObjectSubclass<tile_imp::Tile>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Tile {
    fn new(size: i32) -> Self {
        let tile: Self = glib::Object::new();
        tile.set_size(size);
        tile
    }

    fn set_size(&self, size: i32) {
        self.thumb().set_size(size);
    }

    fn thumb(&self) -> Thumb {
        self.imp().thumb.get().expect("tile thumb").clone()
    }

    fn bind(&self, file: &FileItem) {
        let imp = self.imp();
        let entry = file.entry();
        if let Some(label) = imp.label.get() {
            label.set_text(&entry.name);
        }
        if let Some(badge) = imp.badge.get() {
            badge.set_visible(entry.kind == Kind::Video);
        }
    }
}

mod thumb_imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::Thumb)]
    pub struct Thumb {
        pub size: Cell<i32>,
        #[property(get, set, nullable)]
        texture: RefCell<Option<gdk::Texture>>,
        /// Draw at 100% at most: the texture is the whole (small) image.
        #[property(get, set)]
        native: Cell<bool>,
        #[property(get, set)]
        failed: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Thumb {
        const NAME: &'static str = "FvThumb";
        type Type = super::Thumb;
        type ParentType = gtk::Widget;
    }

    #[glib::derived_properties]
    impl ObjectImpl for Thumb {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj()
                .connect_notify_local(None, |thumb, _| thumb.queue_draw());
        }
    }

    impl WidgetImpl for Thumb {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let size = self.size.get();
            (size, size, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let (width, height) = (widget.width() as f32, widget.height() as f32);

            if let Some(texture) = self.texture.borrow().as_ref() {
                let (tw, th) = (texture.width() as f32, texture.height() as f32);
                let mut scale = (width / tw).min(height / th);
                if self.native.get() {
                    scale = scale.min(1.0 / widget.scale_factor() as f32);
                }
                let (w, h) = (tw * scale, th * scale);
                let rect = graphene::Rect::new((width - w) / 2.0, (height - h) / 2.0, w, h);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(rect, 3.0));
                // Trilinear uses mipmaps, so any downscale stays smooth.
                snapshot.append_scaled_texture(texture, gsk::ScalingFilter::Trilinear, &rect);
                snapshot.pop();
                return;
            }

            // Neutral placeholder while loading, and for videos (until Stage 5).
            let bounds = graphene::Rect::new(0.0, 0.0, width, height);
            let failed = self.failed.get();
            let mut color = widget.color();
            color.set_alpha(if failed { 0.03 } else { 0.06 });
            snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(bounds, 6.0));
            snapshot.append_color(&color, &bounds);
            snapshot.pop();
            if failed {
                draw_missing_icon(&widget, snapshot, width, height);
            }
        }
    }

    /// A dim "broken image" icon for files that can't be decoded.
    fn draw_missing_icon(widget: &super::Thumb, snapshot: &gtk::Snapshot, width: f32, height: f32) {
        let size = (width.min(height) * 0.3).clamp(16.0, 48.0);
        let icon = gtk::IconTheme::for_display(&widget.display()).lookup_icon(
            "image-missing-symbolic",
            &[],
            size as i32,
            widget.scale_factor(),
            widget.direction(),
            gtk::IconLookupFlags::empty(),
        );
        let mut color = widget.color();
        color.set_alpha(0.35);
        snapshot.save();
        snapshot.translate(&graphene::Point::new(
            (width - size) / 2.0,
            (height - size) / 2.0,
        ));
        icon.snapshot_symbolic(snapshot, size.into(), size.into(), &[color]);
        snapshot.restore();
    }
}

glib::wrapper! {
    pub struct Thumb(ObjectSubclass<thumb_imp::Thumb>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Thumb {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl Thumb {
    fn set_size(&self, size: i32) {
        if self.imp().size.replace(size) != size {
            self.queue_resize();
        }
    }
}
