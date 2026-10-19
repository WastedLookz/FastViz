//! Virtualized thumbnail grid. Each tile is a square thumbnail area with the
//! filename underneath and a play badge on videos. Tiles tell the
//! thumbnailer what they show; textures are drawn GPU-scaled to the tile.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib, graphene, gsk};

use crate::loader::Thumbnailer;
use crate::media::Kind;
use crate::model::FileItem;

pub const MIN_SIZE: i32 = 48;
pub const MAX_SIZE: i32 = 512;
/// Size factor per Ctrl+wheel notch.
const ZOOM_STEP: f64 = 1.1;
/// Space around each tile (its margins), set in Preferences.
pub const MAX_PADDING: i32 = 40;
/// The grid's own CSS padding (style.css), outside the tiles' margins.
const GRID_PADDING: i32 = 6;

/// Called on right-click with the widget clicked and the point in it.
type ContextHandler = Box<dyn Fn(&gtk::Widget, f64, f64)>;

pub struct Grid {
    pub view: gtk::GridView,
    pub scroller: gtk::ScrolledWindow,
    tiles: Rc<RefCell<Vec<glib::WeakRef<Tile>>>>,
    size: Rc<Cell<i32>>,
    padding: Rc<Cell<i32>>,
    width: Rc<Cell<f64>>,
    columns: Rc<Columns>,
    thumbnailer: Rc<Thumbnailer>,
    context: Rc<RefCell<Option<ContextHandler>>>,
}

impl Grid {
    /// `size` drives the thumbnail edge length; Ctrl+wheel over the grid changes it.
    /// `padding` is the space around each tile.
    pub fn new(
        model: &impl IsA<gtk::SelectionModel>,
        size: &gtk::Adjustment,
        padding: i32,
        thumbnailer: &Rc<Thumbnailer>,
    ) -> Self {
        let current = Rc::new(Cell::new(size.value().round() as i32));
        let padding = Rc::new(Cell::new(padding.clamp(0, MAX_PADDING)));
        let tiles: Rc<RefCell<Vec<glib::WeakRef<Tile>>>> = Rc::default();
        let selection = model.clone().upcast::<gtk::SelectionModel>();
        let context: Rc<RefCell<Option<ContextHandler>>> = Rc::default();

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(glib::clone!(
            #[strong]
            current,
            #[strong]
            padding,
            #[strong]
            tiles,
            #[weak]
            thumbnailer,
            #[weak]
            selection,
            #[strong]
            context,
            move |_, item| {
                let item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("grid list item");
                let tile = Tile::new(current.get(), padding.get());
                tile.add_controller(drag_source(item, &selection));
                tile.add_controller(context_click(item, &selection, &context));
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

        // Right-click on empty space: clear the selection, offer Paste.
        let background_click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        background_click.connect_pressed(glib::clone!(
            #[weak]
            selection,
            #[strong]
            context,
            move |gesture, _, x, y| {
                selection.unselect_all();
                if let (Some(f), Some(widget)) = (context.borrow().as_ref(), gesture.widget()) {
                    f(&widget, x, y);
                }
            }
        ));
        view.add_controller(background_click);
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
        let columns = Rc::new(Columns::new());
        scroller.hadjustment().connect_changed(glib::clone!(
            #[weak]
            view,
            #[strong]
            width,
            #[strong]
            current,
            #[strong]
            padding,
            #[strong]
            columns,
            move |_| {
                // The whole width, which the spare padding set below doesn't change.
                let Some(bounds) = view.compute_bounds(&view) else {
                    return;
                };
                let full = f64::from(bounds.width());
                if width.replace(full) == full {
                    return;
                }
                // This fires during allocation; changing columns then would re-enter layout.
                let (size, padding, columns) = (current.get(), padding.get(), columns.clone());
                glib::idle_add_local_once(move || columns.fit(&view, full, size, padding));
            }
        ));

        Self {
            view,
            scroller,
            tiles,
            size: current,
            padding,
            width,
            columns,
            thumbnailer: thumbnailer.clone(),
            context,
        }
    }

    pub fn connect_context_menu(&self, f: impl Fn(&gtk::Widget, f64, f64) + 'static) {
        self.context.replace(Some(Box::new(f)));
    }

    /// Resizes every live tile; the grid relayouts on the next frame.
    pub fn set_size(&self, size: i32) {
        if self.size.replace(size) == size {
            return;
        }
        self.columns
            .fit(&self.view, self.width.get(), size, self.padding.get());
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

    pub fn padding(&self) -> i32 {
        self.padding.get()
    }

    /// Changes the space around every tile.
    pub fn set_padding(&self, padding: i32) {
        let padding = padding.clamp(0, MAX_PADDING);
        if self.padding.replace(padding) == padding {
            return;
        }
        self.columns
            .fit(&self.view, self.width.get(), self.size.get(), padding);
        self.thumbnailer.queue_sync();
        self.tiles.borrow_mut().retain(|weak| match weak.upgrade() {
            Some(tile) => {
                tile.set_padding(padding);
                true
            }
            None => false,
        });
    }
}

/// Keeps every column exactly one tile wide, so the gaps between columns are
/// the padding, as between rows. GtkGridView stretches its columns to fill
/// its width, so the width left over goes to the grid's right padding (CSS
/// padding rather than a margin, so it still takes clicks and rubberbands).
/// Reloading a stylesheet restyles the whole window and dropped frames while
/// resizing small thumbnails, so each padding is a class written once.
struct Columns {
    spare: Cell<i32>,
}

impl Columns {
    fn new() -> Self {
        let css = gtk::CssProvider::new();
        // The spare width is always less than one column.
        let rules: String = (0..=MAX_SIZE + 2 * MAX_PADDING)
            .map(|spare| {
                format!(
                    ".fv-grid.{} {{ padding-right: {}px; }}\n",
                    spare_class(spare),
                    GRID_PADDING + spare
                )
            })
            .collect();
        css.load_from_string(&rules);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }
        Self {
            spare: Cell::new(0),
        }
    }

    /// `width` is the grid's whole width, its padding included.
    fn fit(&self, view: &gtk::GridView, width: f64, size: i32, padding: i32) {
        let inner = width as i32 - 2 * GRID_PADDING;
        let pitch = size + 2 * padding;
        let columns = (inner / pitch).max(1);
        if view.max_columns() != columns as u32 {
            view.set_max_columns(columns as u32);
        }
        let spare = (inner - columns * pitch).max(0);
        let old = self.spare.replace(spare);
        if old != spare {
            view.remove_css_class(&spare_class(old));
            view.add_css_class(&spare_class(spare));
        }
    }
}

fn spare_class(spare: i32) -> String {
    format!("fv-spare-{spare}")
}

/// Dragging a tile hands the files to other apps, as a file manager does:
/// the whole selection if the tile is part of it, otherwise just that file.
fn drag_source(list_item: &gtk::ListItem, selection: &gtk::SelectionModel) -> gtk::DragSource {
    let source = gtk::DragSource::builder()
        .actions(gdk::DragAction::COPY)
        .build();
    source.connect_prepare(glib::clone!(
        #[weak]
        list_item,
        #[weak]
        selection,
        #[upgrade_or]
        None,
        move |source, _x, _y| {
            let item = list_item.item().and_downcast::<FileItem>()?;
            let paths: Vec<gio::File> = if selection.is_selected(list_item.position()) {
                selected(&selection)
                    .map(|i| gio::File::for_path(&i.entry().path))
                    .collect()
            } else {
                vec![gio::File::for_path(&item.entry().path)]
            };
            if let Some(texture) = item.texture() {
                source.set_icon(Some(&texture), texture.width() / 2, texture.height() / 2);
            } else if let Some(tile) = list_item.child() {
                let paintable = gtk::WidgetPaintable::new(Some(&tile));
                source.set_icon(Some(&paintable), tile.width() / 2, tile.height() / 2);
            }
            let files = gdk::FileList::from_array(&paths);
            Some(gdk::ContentProvider::for_value(&files.to_value()))
        }
    ));
    source
}

/// Right-click on a tile: select it (unless it is part of the selection),
/// then ask for the context menu.
fn context_click(
    list_item: &gtk::ListItem,
    selection: &gtk::SelectionModel,
    context: &Rc<RefCell<Option<ContextHandler>>>,
) -> gtk::GestureClick {
    let click = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    click.connect_pressed(glib::clone!(
        #[weak]
        list_item,
        #[weak]
        selection,
        #[strong]
        context,
        move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let position = list_item.position();
            if !selection.is_selected(position) {
                selection.select_item(position, true);
            }
            if let (Some(f), Some(widget)) = (context.borrow().as_ref(), gesture.widget()) {
                f(&widget, x, y);
            }
        }
    ));
    click
}

/// The selected items, in grid order.
pub fn selected(selection: &gtk::SelectionModel) -> impl Iterator<Item = FileItem> + '_ {
    let set = selection.selection();
    let positions: Vec<u32> = match gtk::BitsetIter::init_first(&set) {
        Some((iter, first)) => std::iter::once(first).chain(iter).collect(),
        None => Vec::new(),
    };
    positions
        .into_iter()
        .filter_map(|p| selection.item(p).and_downcast::<FileItem>())
}

mod tile_imp {
    use std::cell::OnceCell;

    use super::*;

    #[derive(Default)]
    pub struct Tile {
        pub thumb: OnceCell<super::Thumb>,
        pub label: OnceCell<gtk::Label>,
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
            thumb.set_halign(gtk::Align::Center);
            // A one-char max width keeps long names from widening the column;
            // the label still fills the tile and ellipsizes.
            let label = gtk::Label::builder()
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(1)
                .hexpand(true)
                .build();
            tile.append(&thumb);
            tile.append(&label);

            let _ = self.thumb.set(thumb);
            let _ = self.label.set(label);
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
    fn new(size: i32, padding: i32) -> Self {
        let tile: Self = glib::Object::new();
        tile.set_size(size);
        tile.set_padding(padding);
        tile
    }

    fn set_padding(&self, padding: i32) {
        self.set_margin_top(padding);
        self.set_margin_bottom(padding);
        self.set_margin_start(padding);
        self.set_margin_end(padding);
    }

    fn set_size(&self, size: i32) {
        self.thumb().set_size(size);
    }

    fn thumb(&self) -> Thumb {
        self.imp().thumb.get().expect("tile thumb").clone()
    }

    fn bind(&self, file: &FileItem) {
        let entry = file.entry();
        if let Some(label) = self.imp().label.get() {
            label.set_text(&entry.name);
        }
        let thumb = self.thumb();
        thumb.set_video(entry.kind == Kind::Video);
        let icon = match (entry.kind, entry.content_type.as_deref()) {
            (Kind::Folder, _) => Some(gio::ThemedIcon::new("folder").upcast()),
            (Kind::Other, Some(content_type)) => Some(gio::content_type_get_icon(content_type)),
            (Kind::Other, None) => Some(gio::ThemedIcon::new("text-x-generic").upcast()),
            _ => None,
        };
        thumb.set_icon(icon);
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
        /// Folders and other files show their type's icon instead.
        #[property(get, set, nullable)]
        icon: RefCell<Option<gio::Icon>>,
        /// Draw a play badge.
        #[property(get, set)]
        video: Cell<bool>,
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
            let bounds = graphene::Rect::new(0.0, 0.0, width, height);

            if let Some(texture) = self.texture.borrow().as_ref() {
                let (tw, th) = (texture.width() as f32, texture.height() as f32);
                let mut scale = (width / tw).min(height / th);
                if self.native.get() {
                    scale = scale.min(1.0 / widget.scale_factor() as f32);
                }
                let (w, h) = (tw * scale, th * scale);
                // Centered in the square: landscape pictures get even space
                // above and below (portrait ones fill the height anyway).
                let rect = graphene::Rect::new((width - w) / 2.0, (height - h) / 2.0, w, h);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(rect, 3.0));
                // Trilinear uses mipmaps, so any downscale stays smooth.
                snapshot.append_scaled_texture(texture, gsk::ScalingFilter::Trilinear, &rect);
                snapshot.pop();
                if self.video.get() {
                    draw_play_badge(&widget, snapshot, &rect);
                }
                return;
            }

            if let Some(icon) = self.icon.borrow().as_ref() {
                // The whole square, centered like a thumbnail: themed icons
                // carry their own margins, so this matches a photo's size.
                draw_icon(&widget, snapshot, icon, width.min(height), &bounds);
                return;
            }

            // Neutral placeholder while loading.
            let failed = self.failed.get();
            let mut color = widget.color();
            color.set_alpha(if failed { 0.03 } else { 0.06 });
            snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(bounds, 6.0));
            snapshot.append_color(&color, &bounds);
            snapshot.pop();
            if failed {
                draw_missing_icon(&widget, snapshot, width, height);
            }
            if self.video.get() {
                draw_play_badge(&widget, snapshot, &bounds);
            }
        }
    }

    /// A full-color themed icon, centered in `area`.
    fn draw_icon(
        widget: &super::Thumb,
        snapshot: &gtk::Snapshot,
        icon: &gio::Icon,
        size: f32,
        area: &graphene::Rect,
    ) {
        let paintable = gtk::IconTheme::for_display(&widget.display()).lookup_by_gicon(
            icon,
            size as i32,
            widget.scale_factor(),
            widget.direction(),
            gtk::IconLookupFlags::empty(),
        );
        snapshot.save();
        snapshot.translate(&graphene::Point::new(
            area.x() + (area.width() - size) / 2.0,
            area.y() + (area.height() - size) / 2.0,
        ));
        paintable.snapshot(snapshot, size.into(), size.into());
        snapshot.restore();
    }

    /// A small dark circle with a play triangle, in `rect`'s bottom-right corner.
    fn draw_play_badge(widget: &super::Thumb, snapshot: &gtk::Snapshot, rect: &graphene::Rect) {
        let diameter = (rect.width().min(rect.height()) * 0.18).clamp(16.0, 28.0);
        let margin = 6.0_f32.min(rect.width() / 10.0);
        let circle = graphene::Rect::new(
            rect.x() + rect.width() - diameter - margin,
            rect.y() + rect.height() - diameter - margin,
            diameter,
            diameter,
        );
        snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(circle, diameter / 2.0));
        snapshot.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.6), &circle);
        snapshot.pop();

        let icon_size = diameter * 0.55;
        let icon = gtk::IconTheme::for_display(&widget.display()).lookup_icon(
            "media-playback-start-symbolic",
            &[],
            icon_size as i32,
            widget.scale_factor(),
            widget.direction(),
            gtk::IconLookupFlags::empty(),
        );
        snapshot.save();
        snapshot.translate(&graphene::Point::new(
            circle.x() + (diameter - icon_size) / 2.0,
            circle.y() + (diameter - icon_size) / 2.0,
        ));
        icon.snapshot_symbolic(
            snapshot,
            icon_size.into(),
            icon_size.into(),
            &[gdk::RGBA::WHITE],
        );
        snapshot.restore();
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
