//! Fullscreen viewer: one photo or video at a time, shrink-to-fit, GPU zoom
//! and pan, and the neighbors preloaded (decoded and uploaded) so moving
//! between items is instant. Until an item decodes, its grid thumbnail
//! stands in, stretched to the same place.

mod canvas;
mod load;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib, graphene};

use crate::loader::texture_from;
use crate::media::Kind;
use crate::model::FileItem;
use crate::trace;
use canvas::Canvas;
use load::{Loaded, Pool, Request};

/// Zoom factor per wheel notch.
const ZOOM_STEP: f64 = 1.25;
/// Pointer travel that turns a click into a drag.
const DRAG_THRESHOLD: f64 = 4.0;
const CURSOR_HIDE: Duration = Duration::from_millis(1500);

/// A decoded image, ready to draw.
#[derive(Clone)]
struct Shown {
    texture: gdk::Texture,
    width: u32,
    height: u32,
    full: bool,
    frames: Option<Rc<Vec<(gdk::Texture, u32)>>>,
}

type ItemHandler = Box<dyn Fn(Option<FileItem>)>;
type DeleteHandler = Box<dyn Fn(FileItem)>;
type BackgroundHandler = Box<dyn Fn(bool)>;

pub struct Viewer {
    me: Weak<Self>,
    pub widget: gtk::Overlay,
    canvas: Canvas,
    info: gtk::Label,
    note: gtk::Label,
    pool: Pool,
    model: RefCell<Option<gtk::SelectionModel>>,
    current: RefCell<Option<FileItem>>,
    position: Cell<u32>,
    /// Screen size in device pixels, which previews are decoded for.
    screen: Cell<(u32, u32)>,
    /// Decoded current item and neighbors, by item id.
    cache: RefCell<HashMap<u64, Shown>>,
    /// Item whose decoded image (not stand-in) the canvas shows.
    showing: Cell<u64>,
    animation: RefCell<Option<glib::SourceId>>,
    cursor_timer: RefCell<Option<glib::SourceId>>,
    pointer: Cell<(f64, f64)>,
    show_info: Cell<bool>,
    /// The current item couldn't be decoded.
    failed: Cell<bool>,
    /// When the current item was asked for, until it is drawn (for the trace).
    switched: Cell<Option<Instant>>,
    on_exit: RefCell<Option<ItemHandler>>,
    on_delete: RefCell<Option<DeleteHandler>>,
    on_background: RefCell<Option<BackgroundHandler>>,
}

impl Viewer {
    pub fn new(black: bool) -> Rc<Self> {
        let canvas = Canvas::default();
        canvas.set_black(black);
        let info = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_start(16)
            .margin_top(16)
            .xalign(0.0)
            .css_classes(["fv-info"])
            .visible(false)
            .build();
        let note = gtk::Label::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .margin_bottom(24)
            .css_classes(["fv-info"])
            .visible(false)
            .build();
        let widget = gtk::Overlay::builder()
            .child(&canvas)
            .focusable(true)
            .build();
        widget.add_overlay(&info);
        widget.add_overlay(&note);

        let viewer = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            widget,
            canvas,
            info,
            note,
            pool: Pool::new(2),
            model: RefCell::default(),
            current: RefCell::default(),
            position: Cell::new(0),
            screen: Cell::new((1920, 1080)),
            cache: RefCell::default(),
            showing: Cell::new(0),
            animation: RefCell::default(),
            cursor_timer: RefCell::default(),
            pointer: Cell::new((0.0, 0.0)),
            show_info: Cell::new(false),
            failed: Cell::new(false),
            switched: Cell::new(None),
            on_exit: RefCell::default(),
            on_delete: RefCell::default(),
            on_background: RefCell::default(),
        });
        viewer.connect_input();

        let weak = Rc::downgrade(&viewer);
        let results = viewer.pool.results.clone();
        glib::spawn_future_local(async move {
            while let Ok(loaded) = results.recv().await {
                let Some(viewer) = weak.upgrade() else {
                    return;
                };
                viewer.loaded(loaded);
            }
        });
        viewer
    }

    /// Called with the item last shown when the viewer closes.
    pub fn connect_exit(&self, f: impl Fn(Option<FileItem>) + 'static) {
        self.on_exit.replace(Some(Box::new(f)));
    }

    /// Called to delete an item; the viewer has already moved on from it.
    pub fn connect_delete(&self, f: impl Fn(FileItem) + 'static) {
        self.on_delete.replace(Some(Box::new(f)));
    }

    /// Called when B switches the background (true = black).
    pub fn connect_background(&self, f: impl Fn(bool) + 'static) {
        self.on_background.replace(Some(Box::new(f)));
    }

    pub fn set_black(&self, black: bool) {
        self.canvas.set_black(black);
    }

    /// Opens the item at `position` of `model`; `screen` is in device pixels.
    pub fn open(&self, model: &gtk::SelectionModel, position: u32, screen: (u32, u32)) {
        self.model.replace(Some(model.clone()));
        self.screen.set(screen);
        if let Some(item) = model.item(position).and_downcast::<FileItem>() {
            self.show(item, position);
        }
        self.widget.grab_focus();
        self.pointer_moved();
    }

    fn exit(&self) {
        self.stop_animation();
        if let Some(timer) = self.cursor_timer.take() {
            timer.remove();
        }
        self.widget.set_cursor(None);
        let item = self.current.take();
        self.cache.borrow_mut().clear();
        self.pool.request(Vec::new());
        self.canvas.set_image(None, 0, 0);
        self.showing.set(0);
        if let Some(f) = self.on_exit.borrow().as_ref() {
            f(item);
        }
    }

    fn show(&self, item: FileItem, position: u32) {
        self.stop_animation();
        self.position.set(position);
        self.current.replace(Some(item.clone()));
        self.failed.set(false);
        self.switched.set(Some(Instant::now()));
        let cached = self.cache.borrow().get(&item.id()).cloned();
        match cached {
            Some(shown) => self.display(item.id(), &shown),
            None => {
                // Stand in with the grid thumbnail, at the real image's size.
                let thumbnail = item.texture();
                let (width, height) = item
                    .meta()
                    .map(|m| (m.width, m.height))
                    .or_else(|| {
                        thumbnail
                            .as_ref()
                            .map(|t| (t.width() as u32, t.height() as u32))
                    })
                    .unwrap_or((0, 0));
                self.canvas.set_image(thumbnail.as_ref(), width, height);
                self.showing.set(0);
            }
        }
        self.update_overlays();
        self.request();
    }

    fn display(&self, id: u64, shown: &Shown) {
        if self.showing.replace(id) == id {
            self.canvas.replace_texture(&shown.texture);
        } else {
            self.canvas
                .set_image(Some(&shown.texture), shown.width, shown.height);
        }
        if let Some(frames) = &shown.frames {
            self.animate(frames.clone(), 0);
        }
        if let Some(started) = self.switched.take() {
            trace!(
                "fullscreen: drawn {:.1} ms after switching{}",
                started.elapsed().as_secs_f64() * 1e3,
                if shown.full { "" } else { " (preview)" }
            );
        }
        self.update_overlays();
    }

    /// Asks for the current item (and full detail if zoomed past the
    /// preview), then the next and previous ones; drops other decoded items.
    fn request(&self) {
        let Some(current) = self.current.borrow().clone() else {
            return;
        };
        let mut wanted = vec![(current.clone(), 0)];
        if let Some((_, next)) = self.neighbor(1) {
            wanted.push((next, 1));
        }
        if let Some((_, previous)) = self.neighbor(-1) {
            wanted.push((previous, 2));
        }
        self.cache
            .borrow_mut()
            .retain(|id, _| wanted.iter().any(|(item, _)| item.id() == *id));

        let screen = Some(self.screen.get());
        let request = |item: &FileItem, screen, priority| {
            let entry = item.entry();
            Request {
                id: item.id(),
                path: entry.path.clone(),
                kind: entry.kind,
                format: entry.format,
                screen,
                priority,
            }
        };
        let mut requests = Vec::new();
        let cache = self.cache.borrow();
        for (item, priority) in &wanted {
            if !cache.contains_key(&item.id()) {
                requests.push(request(item, screen, *priority));
            }
        }
        let current_full = cache.get(&current.id()).is_some_and(|s| s.full);
        if !current_full && self.canvas.is_zoomed() && self.canvas.needs_more_detail() {
            requests.push(request(&current, None, 0));
        }
        drop(cache);
        self.pool.request(requests);
    }

    fn loaded(&self, loaded: Loaded) {
        let (id, shown) = match loaded {
            Loaded::Still {
                id,
                pixels,
                width,
                height,
                full,
            } => {
                let (texture, _) = texture_from(pixels);
                let shown = Shown {
                    texture,
                    width,
                    height,
                    full,
                    frames: None,
                };
                (id, shown)
            }
            Loaded::Animation { id, frames } => {
                let (width, height) = frames[0].0.dimensions();
                let frames: Vec<(gdk::Texture, u32)> = frames
                    .into_iter()
                    .map(|(frame, delay)| {
                        (texture_from(crate::decode::Pixels::Rgba(frame)).0, delay)
                    })
                    .collect();
                let shown = Shown {
                    texture: frames[0].0.clone(),
                    width,
                    height,
                    full: true,
                    frames: Some(Rc::new(frames)),
                };
                (id, shown)
            }
            Loaded::Failed { id, full } => {
                if !full && self.current_id() == Some(id) {
                    self.canvas.set_image(None, 0, 0);
                    self.failed.set(true);
                    self.update_overlays();
                }
                return;
            }
        };
        let current = self.current_id() == Some(id);
        let wanted = current
            || [1, -1]
                .iter()
                .any(|&step| self.neighbor(step).is_some_and(|(_, item)| item.id() == id));
        if !wanted {
            return;
        }
        // Keep the best version: a late preview never replaces a full image.
        let better = self
            .cache
            .borrow()
            .get(&id)
            .is_none_or(|old| shown.full || !old.full);
        if !better {
            return;
        }
        let refined = shown.full && self.showing.get() == id;
        self.cache.borrow_mut().insert(id, shown.clone());
        if current {
            if refined {
                trace!(
                    "fullscreen: full resolution {} × {} shown",
                    shown.width, shown.height
                );
            }
            self.display(id, &shown);
        } else {
            self.preupload(&shown.texture);
        }
    }

    /// Uploads a texture now (drawing it once, 1 px, offscreen), so showing
    /// it later doesn't wait for the GPU copy.
    fn preupload(&self, texture: &gdk::Texture) {
        let Some(renderer) = self.widget.native().and_then(|n| n.renderer()) else {
            return;
        };
        let snapshot = gtk::Snapshot::new();
        let pixel = graphene::Rect::new(0.0, 0.0, 1.0, 1.0);
        snapshot.append_texture(texture, &pixel);
        if let Some(node) = snapshot.to_node() {
            let _ = renderer.render_texture(node, Some(&pixel));
        }
    }

    fn animate(&self, frames: Rc<Vec<(gdk::Texture, u32)>>, index: usize) {
        let (texture, delay) = &frames[index];
        self.canvas.replace_texture(texture);
        let me = self.me.clone();
        let next = (index + 1) % frames.len();
        let id =
            glib::timeout_add_local_once(Duration::from_millis(u64::from(*delay)), move || {
                if let Some(viewer) = me.upgrade() {
                    viewer.animation.take();
                    viewer.animate(frames.clone(), next);
                }
            });
        if let Some(old) = self.animation.replace(Some(id)) {
            old.remove();
        }
    }

    fn stop_animation(&self) {
        if let Some(id) = self.animation.take() {
            id.remove();
        }
    }

    fn current_id(&self) -> Option<u64> {
        self.current.borrow().as_ref().map(FileItem::id)
    }

    /// The current item's position, found again if the grid changed.
    fn locate(&self) -> Option<u32> {
        let model = self.model.borrow().clone()?;
        let current = self.current.borrow().clone()?;
        let position = self.position.get();
        if model.item(position).as_ref() == Some(current.upcast_ref()) {
            return Some(position);
        }
        let found =
            (0..model.n_items()).find(|&p| model.item(p).as_ref() == Some(current.upcast_ref()))?;
        self.position.set(found);
        Some(found)
    }

    /// The nearest photo or video `step` (+1 or -1) away from the current one.
    fn neighbor(&self, step: i64) -> Option<(u32, FileItem)> {
        let start = self.locate()?;
        self.media_from(i64::from(start) + step, step)
    }

    /// The first photo or video at or after `position`, moving by `step`.
    fn media_from(&self, mut position: i64, step: i64) -> Option<(u32, FileItem)> {
        let model = self.model.borrow().clone()?;
        while position >= 0 && position < i64::from(model.n_items()) {
            if let Some(item) = model.item(position as u32).and_downcast::<FileItem>()
                && item.entry().kind.is_media()
            {
                return Some((position as u32, item));
            }
            position += step;
        }
        None
    }

    fn go(&self, target: Option<(u32, FileItem)>) {
        if let Some((position, item)) = target {
            self.show(item, position);
        }
    }

    fn delete_current(&self) {
        let Some(item) = self.current.borrow().clone() else {
            return;
        };
        match self.neighbor(1).or_else(|| self.neighbor(-1)) {
            Some((position, next)) => self.show(next, position),
            None => self.exit(),
        }
        if let Some(f) = self.on_delete.borrow().as_ref() {
            f(item);
        }
    }

    fn toggle_background(&self) {
        let black = !self.canvas.is_black();
        self.canvas.set_black(black);
        if let Some(f) = self.on_background.borrow().as_ref() {
            f(black);
        }
    }

    fn update_overlays(&self) {
        let Some(item) = self.current.borrow().clone() else {
            return;
        };
        let entry = item.entry();
        let note = if self.failed.get() {
            Some("Can't open this file")
        } else if entry.kind == Kind::Video {
            Some("Video playback isn't built yet (Stage 5)")
        } else {
            None
        };
        self.note.set_text(note.unwrap_or_default());
        self.note.set_visible(note.is_some());
        if !self.show_info.get() {
            self.info.set_visible(false);
            return;
        }
        let date = glib::DateTime::from_unix_local(item.date_taken())
            .and_then(|d| d.format("%Y-%m-%d %H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let size = self
            .cache
            .borrow()
            .get(&item.id())
            .map(|s| (s.width, s.height))
            .or_else(|| item.meta().map(|m| (m.width, m.height)));
        let mut details = vec![date, item.format().to_owned()];
        if let Some((width, height)) = size {
            details.push(format!("{width} × {height}"));
        }
        details.push(glib::format_size(entry.size).to_string());
        let (index, total) = self.media_index();
        let text = format!(
            "{}\n{}\n{index} of {total}",
            glib::markup_escape_text(&entry.name),
            details.join("  ·  ")
        );
        self.info.set_markup(&text);
        self.info.set_visible(true);
    }

    /// The current item's number among the folder's photos and videos, and their count.
    fn media_index(&self) -> (u32, u32) {
        let (Some(model), Some(position)) = (self.model.borrow().clone(), self.locate()) else {
            return (0, 0);
        };
        let mut index = 0;
        let mut total = 0;
        for p in 0..model.n_items() {
            let media = model
                .item(p)
                .and_downcast::<FileItem>()
                .is_some_and(|item| item.entry().kind.is_media());
            if media {
                total += 1;
                if p <= position {
                    index = total;
                }
            }
        }
        (index, total)
    }

    /// Shows the pointer, and hides it again after a moment of stillness.
    fn pointer_moved(&self) {
        self.widget.set_cursor(None);
        if let Some(timer) = self.cursor_timer.take() {
            timer.remove();
        }
        let me = self.me.clone();
        let timer = glib::timeout_add_local_once(CURSOR_HIDE, move || {
            if let Some(viewer) = me.upgrade() {
                viewer.cursor_timer.take();
                viewer.widget.set_cursor_from_name(Some("none"));
            }
        });
        self.cursor_timer.replace(Some(timer));
    }

    fn connect_input(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| {
                if modifiers
                    .intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK)
                {
                    return glib::Propagation::Proceed;
                }
                use gdk::Key;
                match key {
                    Key::Escape => viewer.exit(),
                    Key::Right | Key::Page_Down | Key::KP_Page_Down => {
                        viewer.go(viewer.neighbor(1))
                    }
                    Key::Left | Key::Page_Up | Key::KP_Page_Up | Key::BackSpace => {
                        viewer.go(viewer.neighbor(-1))
                    }
                    Key::Home | Key::KP_Home => viewer.go(viewer.media_from(0, 1)),
                    Key::End | Key::KP_End => {
                        let last = viewer.model.borrow().as_ref().map_or(0, |m| m.n_items());
                        viewer.go(viewer.media_from(i64::from(last) - 1, -1));
                    }
                    Key::_0 | Key::KP_0 => viewer.canvas.fit(),
                    Key::b | Key::B => viewer.toggle_background(),
                    Key::i | Key::I => {
                        viewer.show_info.set(!viewer.show_info.get());
                        viewer.update_overlays();
                    }
                    Key::Delete | Key::KP_Delete => viewer.delete_current(),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        ));
        self.widget.add_controller(keys);

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            move |_, x, y| {
                if viewer.pointer.replace((x, y)) != (x, y) {
                    viewer.pointer_moved();
                }
            }
        ));
        self.widget.add_controller(motion);

        // The plain wheel zooms around the pointer (videos get volume in Stage 5).
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |controller, _, dy| {
                let notches = match controller.unit() {
                    gdk::ScrollUnit::Wheel => dy,
                    _ => dy / 40.0,
                };
                let (x, y) = viewer.pointer.get();
                viewer.canvas.zoom_at(ZOOM_STEP.powf(-notches), x, y);
                viewer.request();
                glib::Propagation::Stop
            }
        ));
        self.widget.add_controller(scroll);

        // Drag pans when zoomed; a click without movement toggles 100%.
        let drag = gtk::GestureDrag::new();
        let last = Rc::new(Cell::new((0.0, 0.0)));
        drag.connect_drag_begin(glib::clone!(
            #[strong]
            last,
            move |_, _, _| last.set((0.0, 0.0))
        ));
        drag.connect_drag_update(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            #[strong]
            last,
            move |_, dx, dy| {
                let (lx, ly) = last.replace((dx, dy));
                viewer.canvas.pan(dx - lx, dy - ly);
            }
        ));
        drag.connect_drag_end(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            move |drag, dx, dy| {
                if dx.hypot(dy) < DRAG_THRESHOLD
                    && let Some((x, y)) = drag.start_point()
                {
                    viewer.canvas.toggle_actual_size(x, y);
                    viewer.request();
                }
            }
        ));
        self.widget.add_controller(drag);

        // Mouse back/forward buttons.
        let buttons = gtk::GestureClick::builder().button(0).build();
        buttons.connect_pressed(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            move |gesture, _, _, _| match gesture.current_button() {
                8 => viewer.go(viewer.neighbor(-1)),
                9 => viewer.go(viewer.neighbor(1)),
                _ => {}
            }
        ));
        self.widget.add_controller(buttons);
    }
}
