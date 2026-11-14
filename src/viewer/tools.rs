//! Rotating and cropping in the viewer, and the tools for it:
//!
//! - R / L turn the picture or video, at once on screen; the file follows
//!   on a worker (edits run one at a time, in order, and quick turns of the
//!   same item merge into one).
//! - C crops: a frame over the picture, a bar with the aspect ratio, the
//!   size and Cancel / Crop; Enter crops (after a confirmation, since the
//!   original is replaced), Esc cancels.
//! - Pushing the pointer against the bottom edge of a photo brings up a
//!   small tool strip: rotate, crop and the exact zoom. Videos have the
//!   rotate buttons in their controls bar. Right-click has the same in a menu.

use std::collections::VecDeque;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::Viewer;
use super::crop::CropView;
use crate::edit::{self, Edit, Rect};
use crate::media::Kind;
use crate::model::FileItem;

/// Space between the bottom of the screen and the tool strip or crop bar.
const BAR_MARGIN: i32 = 24;
/// The pointer pushing against the bottom edge (logical pixels) brings up the strip.
const EDGE: f64 = 6.0;

/// Zoom levels in the strip's menu, in percent.
const ZOOMS: [u32; 7] = [10, 25, 50, 100, 200, 400, 800];
/// Aspect ratios for cropping: (action target, label).
const ASPECTS: [(&str, &str); 9] = [
    ("free", "Free"),
    ("original", "Original"),
    ("1:1", "Square"),
    ("4:3", "4:3"),
    ("3:4", "3:4"),
    ("3:2", "3:2"),
    ("2:3", "2:3"),
    ("16:9", "16:9"),
    ("9:16", "9:16"),
];

/// A file edit waiting or running.
pub(super) struct Job {
    pub item: FileItem,
    pub edit: Edit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cropping {
    Off,
    /// The frame is up.
    On,
    /// The crop is being written; the frame stays until the result shows.
    Saving,
}

pub(super) type EditedHandler = Box<dyn Fn(&FileItem, crate::scan::Entry) -> Option<FileItem>>;
pub(super) type MessageHandler = Box<dyn Fn(&str)>;

/// The editing widgets.
pub(super) struct Tools {
    pub strip: gtk::Box,
    pub zoom: gtk::MenuButton,
    pub crop_view: CropView,
    pub crop_bar: gtk::Box,
    pub crop_size: gtk::Label,
    pub aspect: gtk::MenuButton,
    pub apply: gtk::Button,
    pub cancel: gtk::Button,
    pub actions: gio::SimpleActionGroup,
}

impl Tools {
    pub fn new(video_controls: &gtk::Box) -> Self {
        let strip = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .margin_bottom(BAR_MARGIN)
            .css_classes(["fv-controls"])
            .visible(false)
            .build();
        strip.append(&tool_button(
            "object-rotate-left-symbolic",
            "Rotate Left (L)",
            "viewer.rotate-left",
        ));
        strip.append(&tool_button(
            "object-rotate-right-symbolic",
            "Rotate Right (R)",
            "viewer.rotate-right",
        ));
        let crop = tool_button("image-crop-symbolic", "Crop (C)", "viewer.crop");
        if !has_icon(&crop, "image-crop-symbolic") {
            crop.set_child(Some(&crop_icon()));
        }
        strip.append(&crop);
        strip.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        let zooms = gio::Menu::new();
        let fit = gio::MenuItem::new(Some("Fit"), None);
        fit.set_action_and_target_value(Some("viewer.zoom"), Some(&"fit".to_variant()));
        fit.set_attribute_value("accel", Some(&"0".to_variant()));
        zooms.append_item(&fit);
        let levels = gio::Menu::new();
        for zoom in ZOOMS {
            let item = gio::MenuItem::new(Some(&format!("{zoom}%")), None);
            item.set_action_and_target_value(
                Some("viewer.zoom"),
                Some(&zoom.to_string().to_variant()),
            );
            levels.append_item(&item);
        }
        let menu = gio::Menu::new();
        menu.append_section(None, &zooms);
        menu.append_section(None, &levels);
        let zoom = gtk::MenuButton::builder()
            .label("100%")
            .menu_model(&menu)
            .direction(gtk::ArrowType::Up)
            .always_show_arrow(true)
            .tooltip_text("Zoom (Up/Down)")
            .css_classes(["flat", "fv-zoom"])
            .focusable(false)
            .build();
        strip.append(&zoom);

        // Videos turn from their controls bar.
        video_controls.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        video_controls.append(&tool_button(
            "object-rotate-left-symbolic",
            "Rotate Left (L)",
            "viewer.rotate-left",
        ));
        video_controls.append(&tool_button(
            "object-rotate-right-symbolic",
            "Rotate Right (R)",
            "viewer.rotate-right",
        ));

        let aspects = gio::Menu::new();
        for (target, label) in ASPECTS {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some("viewer.aspect"), Some(&target.to_variant()));
            aspects.append_item(&item);
        }
        let aspect = gtk::MenuButton::builder()
            .label("Free")
            .menu_model(&aspects)
            .direction(gtk::ArrowType::Up)
            .always_show_arrow(true)
            .tooltip_text("Aspect Ratio")
            .css_classes(["flat"])
            .focusable(false)
            .build();
        let crop_size = gtk::Label::builder()
            .css_classes(["fv-time"])
            .width_chars(13)
            .build();
        let cancel = gtk::Button::builder()
            .label("Cancel")
            .tooltip_text("Cancel (Esc)")
            .css_classes(["flat"])
            .focusable(false)
            .build();
        let apply = gtk::Button::builder()
            .label("Crop")
            .tooltip_text("Crop (Enter)")
            .css_classes(["suggested-action"])
            .focusable(false)
            .build();
        let crop_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .margin_bottom(BAR_MARGIN)
            .css_classes(["fv-controls"])
            .visible(false)
            .build();
        crop_bar.append(&aspect);
        crop_bar.append(&crop_size);
        crop_bar.append(&cancel);
        crop_bar.append(&apply);

        Self {
            strip,
            zoom,
            crop_view: CropView::default(),
            crop_bar,
            crop_size,
            aspect,
            apply,
            cancel,
            actions: gio::SimpleActionGroup::new(),
        }
    }
}

fn tool_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .action_name(action)
        .css_classes(["flat", "circular"])
        .focusable(false)
        .focus_on_click(false)
        .build()
}

fn has_icon(widget: &impl IsA<gtk::Widget>, name: &str) -> bool {
    gtk::IconTheme::for_display(&WidgetExt::display(widget)).has_icon(name)
}

/// A crop symbol (two interlocking corners) for icon themes without one,
/// drawn in the text color like a symbolic icon.
fn crop_icon() -> gtk::Widget {
    let area = gtk::DrawingArea::builder()
        .content_width(16)
        .content_height(16)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    area.set_draw_func(|area, cr, _, _| {
        let color = area.color();
        cr.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            f64::from(color.alpha()),
        );
        for (x, y, w, h) in [
            (3.0, 0.0, 2.0, 13.0),
            (3.0, 11.0, 13.0, 2.0),
            (11.0, 3.0, 2.0, 13.0),
            (0.0, 3.0, 13.0, 2.0),
        ] {
            cr.rectangle(x, y, w, h);
        }
        let _ = cr.fill();
    });
    area.upcast()
}

impl Viewer {
    pub(super) fn connect_tools(self: &Rc<Self>) {
        let tools = &self.tools;
        let simple = |name: &str, run: fn(&Viewer)| {
            let action = gio::SimpleAction::new(name, None);
            let me = self.me.clone();
            action.connect_activate(move |_, _| {
                if let Some(viewer) = me.upgrade() {
                    run(&viewer);
                }
            });
            tools.actions.add_action(&action);
        };
        simple("rotate-left", |viewer| viewer.rotate(3));
        simple("rotate-right", |viewer| viewer.rotate(1));
        simple("crop", |viewer| viewer.start_crop());

        let zoom = gio::SimpleAction::new("zoom", Some(glib::VariantTy::STRING));
        let me = self.me.clone();
        zoom.connect_activate(move |_, target| {
            let Some(viewer) = me.upgrade() else {
                return;
            };
            match target.and_then(|t| t.str()).map(str::parse::<f64>) {
                Some(Ok(percent)) => viewer.canvas.zoom_percent_to(percent),
                _ => viewer.canvas.fit(),
            }
            viewer.request();
        });
        tools.actions.add_action(&zoom);

        let aspect = gio::SimpleAction::new_stateful(
            "aspect",
            Some(glib::VariantTy::STRING),
            &"free".to_variant(),
        );
        let me = self.me.clone();
        aspect.connect_activate(move |action, target| {
            let (Some(viewer), Some(target)) = (me.upgrade(), target) else {
                return;
            };
            action.set_state(target);
            viewer.set_aspect(target.str().unwrap_or("free"));
        });
        tools.actions.add_action(&aspect);
        self.widget
            .insert_action_group("viewer", Some(&tools.actions));

        let me = self.me.clone();
        self.canvas.connect_changed(move || {
            if let Some(viewer) = me.upgrade() {
                viewer.update_zoom_label();
            }
        });
        let me = self.me.clone();
        tools.crop_view.connect_changed(move || {
            if let Some(viewer) = me.upgrade() {
                viewer.update_crop_bar();
            }
        });
        let me = self.me.clone();
        tools.apply.connect_clicked(move |_| {
            if let Some(viewer) = me.upgrade() {
                viewer.confirm_crop();
            }
        });
        let me = self.me.clone();
        tools.cancel.connect_clicked(move |_| {
            if let Some(viewer) = me.upgrade() {
                viewer.cancel_crop();
            }
        });
        // The strip stays up while its zoom menu is open (the pointer is
        // on the menu then, not on the viewer), and after: the pointer's next
        // move decides.
        let me = self.me.clone();
        tools.zoom.connect_active_notify(move |button| {
            if let Some(viewer) = me.upgrade()
                && !button.is_active()
            {
                viewer.widget.grab_focus();
            }
        });
        let me = self.me.clone();
        tools.aspect.connect_active_notify(move |button| {
            if let Some(viewer) = me.upgrade()
                && !button.is_active()
            {
                viewer.widget.grab_focus();
            }
        });
        self.update_edit_actions();
    }

    /// Called with an edited item and its new entry; returns the item that
    /// replaces it in the grid (`None` if it isn't there any more).
    pub fn connect_edited(
        &self,
        f: impl Fn(&FileItem, crate::scan::Entry) -> Option<FileItem> + 'static,
    ) {
        self.on_edited.replace(Some(Box::new(f)));
    }

    /// Called with a message for a toast (an edit failed, or can't be done).
    pub fn connect_message(&self, f: impl Fn(&str) + 'static) {
        self.on_message.replace(Some(Box::new(f)));
    }

    fn message(&self, text: &str) {
        if let Some(f) = self.on_message.borrow().as_ref() {
            f(text);
        }
    }

    pub(super) fn cropping(&self) -> bool {
        self.cropping.get() != Cropping::Off
    }

    /// Enables rotate and crop for what the current item's format allows.
    pub(super) fn update_edit_actions(&self) {
        let item = self.current.borrow().clone();
        let (rotate, crop) = item.map_or((false, false), |item| {
            let kind = item.entry().kind;
            (
                edit::can_rotate(kind, item.format()),
                edit::can_crop(kind, item.format()) && !self.failed.get(),
            )
        });
        let free = !self.cropping();
        let enable = |name: &str, enabled: bool| {
            if let Some(action) = self
                .tools
                .actions
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_enabled(enabled);
            }
        };
        enable("rotate-left", rotate && free);
        enable("rotate-right", rotate && free);
        enable("crop", crop && free);
        enable("zoom", free);
    }

    /// Right-click: the edit menu.
    pub(super) fn context_menu(&self, x: f64, y: f64) {
        if self.cropping() || self.current.borrow().is_none() {
            return;
        }
        let item = |label: &str, action: &str, accel: &str| {
            let item = gio::MenuItem::new(Some(label), Some(action));
            item.set_attribute_value("accel", Some(&accel.to_variant()));
            item
        };
        let rotate = gio::Menu::new();
        rotate.append_item(&item("Rotate Left", "viewer.rotate-left", "l"));
        rotate.append_item(&item("Rotate Right", "viewer.rotate-right", "r"));
        let menu = gio::Menu::new();
        menu.append_section(None, &rotate);
        if !self.video_current() {
            let crop = gio::Menu::new();
            crop.append_item(&item("Crop…", "viewer.crop", "c"));
            menu.append_section(None, &crop);
        }
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&self.widget);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        // Unparent later: the chosen item's action runs after "closed".
        let widget = self.widget.downgrade();
        popover.connect_closed(move |popover| {
            let popover = popover.clone();
            let widget = widget.clone();
            glib::idle_add_local_once(move || {
                popover.unparent();
                if let Some(widget) = widget.upgrade() {
                    widget.grab_focus();
                }
            });
        });
        popover.popup();
    }

    /// Whether (x, y) is on one of the bars (video controls, tool strip,
    /// crop bar), where clicks aren't the picture's.
    pub(super) fn on_bars(&self, x: f64, y: f64) -> bool {
        if self.playback.controls_at(&self.widget, x, y) {
            return true;
        }
        let Some(picked) = self.widget.pick(x, y, gtk::PickFlags::DEFAULT) else {
            return false;
        };
        [&self.tools.strip, &self.tools.crop_bar]
            .iter()
            .any(|bar| bar.is_visible() && (&picked == *bar || picked.is_ancestor(*bar)))
    }

    pub(super) fn tools_hovered(&self) -> bool {
        let (x, y) = self.pointer.get();
        self.tools_up.get() && self.on_bars(x, y)
    }

    /// Pushing the pointer against the bottom edge of a photo brings up the
    /// tool strip; moving above it puts it away.
    pub(super) fn tools_hover(&self, y: f64) {
        let height = f64::from(self.widget.height());
        let photo = self
            .current
            .borrow()
            .as_ref()
            .is_some_and(|item| item.entry().kind == Kind::Image);
        let menu_open = self.tools.zoom.is_active();
        let allowed = photo && !self.cropping() && height > 0.0 && self.pointer_in.get();
        let want = menu_open
            || (allowed && {
                let edge = if self.tools_up.get() {
                    height - f64::from(BAR_MARGIN) - f64::from(self.tools.strip.height()) - 32.0
                } else {
                    height - EDGE
                };
                y >= edge
            });
        if want == self.tools_up.get() {
            return;
        }
        self.tools_up.set(want);
        if want {
            self.update_zoom_label();
        }
        self.tools.strip.set_visible(want);
    }

    pub(super) fn hide_tools(&self) {
        if self.tools_up.replace(false) {
            self.tools.strip.set_visible(false);
        }
    }

    fn update_zoom_label(&self) {
        if self.tools.strip.is_visible() || self.tools_up.get() {
            let percent = self.canvas.zoom_percent();
            let label = if percent < 10.0 {
                format!("{percent:.1}%")
            } else {
                format!("{percent:.0}%")
            };
            self.tools.zoom.set_label(&label);
        }
    }

    /// Quarter turns of the current item's file not yet written, plus those
    /// being written if `running` (what a thumbnail of it needs on top).
    pub(super) fn pending_turns(&self, id: u64, running: bool) -> u8 {
        let turns = |job: &Job| match job.edit {
            Edit::Rotate(turns) if job.item.id() == id => turns,
            _ => 0,
        };
        let queued: u8 = self.edits.borrow().iter().map(turns).sum();
        let busy = if running {
            self.running.borrow().as_ref().map_or(0, turns)
        } else {
            0
        };
        (queued + busy) % 4
    }

    /// Whether a decode of item `id` could be of a file that is changing (or
    /// is about to be cropped): its result can't be trusted then.
    pub(super) fn being_edited(&self, id: u64) -> bool {
        self.running
            .borrow()
            .as_ref()
            .is_some_and(|job| job.item.id() == id)
            || self
                .edits
                .borrow()
                .iter()
                .any(|job| job.item.id() == id && matches!(job.edit, Edit::Crop(_)))
    }

    /// Turns the current item `turns` quarter turns clockwise: on screen
    /// now, in its file as soon as the edits before it are done.
    pub(super) fn rotate(&self, turns: u8) {
        let Some(item) = self.current.borrow().clone() else {
            return;
        };
        if self.cropping() {
            return;
        }
        let kind = item.entry().kind;
        if !edit::can_rotate(kind, item.format()) {
            self.message(&format!("{} files can't be rotated", item.format()));
            return;
        }
        self.canvas.set_turn(self.canvas.turn() + turns);
        if let Some(shown) = self.cache.borrow_mut().get_mut(&item.id()) {
            shown.turn = (shown.turn + turns) % 4;
        }
        if kind == Kind::Video {
            self.playback.rotate_by(turns);
        }
        self.queue_edit(item, Edit::Rotate(turns));
        self.update_overlays();
        self.request();
    }

    fn queue_edit(&self, item: FileItem, edit: Edit) {
        {
            let mut queue = self.edits.borrow_mut();
            // Quick turns of the same item add up to one rewrite.
            let merged = match (queue.back_mut(), edit) {
                (Some(last), Edit::Rotate(turns)) if last.item == item => match last.edit {
                    Edit::Rotate(earlier) => {
                        last.edit = Edit::Rotate((earlier + turns) % 4);
                        true
                    }
                    Edit::Crop(_) => false,
                },
                _ => false,
            };
            if !merged {
                queue.push_back(Job { item, edit });
            }
            // A full turn is no change.
            queue.retain(|job| job.edit != Edit::Rotate(0));
        }
        self.pump();
    }

    /// Starts the next edit if none is running.
    fn pump(&self) {
        if self.running.borrow().is_some() {
            return;
        }
        let Some(job) = self.edits.borrow_mut().pop_front() else {
            return;
        };
        let path = job.item.entry().path.clone();
        let edit = job.edit;
        self.running.replace(Some(job));
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || edit::apply(&path, edit))
                .await
                .unwrap_or_else(|_| Err("the edit failed unexpectedly".to_owned()));
            if let Some(viewer) = me.upgrade() {
                viewer.edit_done(result);
            }
        });
    }

    fn edit_done(&self, result: Result<crate::scan::Entry, String>) {
        let Some(job) = self.running.take() else {
            return;
        };
        match result {
            Ok(entry) => {
                let new = self
                    .on_edited
                    .borrow()
                    .as_ref()
                    .and_then(|f| f(&job.item, entry));
                match new {
                    Some(new) => self.replaced(&job, &new),
                    None => {
                        // It left the grid (another folder opened meanwhile).
                        self.edits.borrow_mut().retain(|j| j.item != job.item);
                        if self.cropping.get() == Cropping::Saving {
                            self.end_crop();
                        }
                    }
                }
            }
            Err(error) => {
                let name = &job.item.entry().name;
                let verb = match job.edit {
                    Edit::Rotate(_) => "rotate",
                    Edit::Crop(_) => "crop",
                };
                self.message(&format!("Couldn't {verb} “{name}”: {error}"));
                // Its later edits are dropped too; the view goes back to the file.
                let mut undone = 0;
                let mut crop = matches!(job.edit, Edit::Crop(_));
                if let Edit::Rotate(turns) = job.edit {
                    undone += turns;
                }
                self.edits.borrow_mut().retain(|j| {
                    if j.item != job.item {
                        return true;
                    }
                    match j.edit {
                        Edit::Rotate(turns) => undone += turns,
                        Edit::Crop(_) => crop = true,
                    }
                    false
                });
                self.unturn(&job.item, undone % 4);
                if crop && self.cropping.get() == Cropping::Saving {
                    self.cropping.set(Cropping::On);
                    self.tools.crop_view.set_frozen(false);
                    self.update_crop_bar();
                }
            }
        }
        self.update_edit_actions();
        self.pump();
    }

    /// Takes back `turns` quarter turns shown for `item` (its file didn't turn).
    fn unturn(&self, item: &FileItem, turns: u8) {
        if turns == 0 {
            return;
        }
        let back = 4 - turns;
        if let Some(shown) = self.cache.borrow_mut().get_mut(&item.id()) {
            shown.turn = (shown.turn + back) % 4;
        }
        if self.current.borrow().as_ref() == Some(item) {
            self.canvas.set_turn(self.canvas.turn() + back);
            if item.entry().kind == Kind::Video {
                self.playback.rotate_by(back);
            }
            self.update_overlays();
        }
    }

    /// `job`'s item was written and the grid now has `new` in its place.
    fn replaced(&self, job: &Job, new: &FileItem) {
        let old = &job.item;
        for queued in self.edits.borrow_mut().iter_mut() {
            if &queued.item == old {
                queued.item = new.clone();
            }
        }
        self.slideshow_replace(old, new);
        let cropped = matches!(job.edit, Edit::Crop(_));
        // A turned picture still shows right (its old pixels, turned); a
        // cropped one is decoded again.
        let shown = self.cache.borrow_mut().remove(&old.id());
        if !cropped {
            if let Some(shown) = shown {
                self.cache.borrow_mut().insert(new.id(), shown);
            }
            if self.showing.get() == old.id() {
                self.showing.set(new.id());
            }
        }
        if self.camera_for.get() == old.id() {
            self.camera_for.set(0);
        }
        if self.current.borrow().as_ref() == Some(old) {
            self.current.replace(Some(new.clone()));
            self.update_overlays();
            self.request();
        }
    }

    pub(super) fn start_crop(&self) {
        let Some(item) = self.current.borrow().clone() else {
            return;
        };
        if self.cropping() {
            return;
        }
        if !edit::can_crop(item.entry().kind, item.format()) {
            let what = if item.entry().kind == Kind::Video {
                "Videos".to_owned()
            } else {
                format!("{} files", item.format())
            };
            self.message(&format!("{what} can't be cropped"));
            return;
        }
        // The frame needs the picture itself (its real size), not the stand-in.
        if self.failed.get() || self.showing.get() != item.id() {
            return;
        }
        self.stop_slideshow();
        self.stop_animation();
        self.canvas.fit();
        let Some(frame) = self.canvas.image_rect() else {
            return;
        };
        self.cropping.set(Cropping::On);
        self.hide_tools();
        if self.camera_up.replace(false) {
            self.camera.set_visible(false);
        }
        if let Some(timer) = self.cursor_timer.take() {
            timer.remove();
        }
        self.widget.set_cursor(None);
        if let Some(action) = self.tools.actions.lookup_action("aspect") {
            action.change_state(&"free".to_variant());
        }
        self.tools.aspect.set_label("Free");
        self.tools.crop_view.start(frame, self.canvas.shown_size());
        self.tools.crop_view.set_visible(true);
        self.tools.crop_bar.set_visible(true);
        self.update_crop_bar();
        self.update_edit_actions();
    }

    fn cancel_crop(&self) {
        if self.cropping.get() == Cropping::On {
            self.end_crop();
        }
    }

    pub(super) fn end_crop(&self) {
        if self.cropping.replace(Cropping::Off) == Cropping::Off {
            return;
        }
        self.tools.crop_view.set_frozen(false);
        self.tools.crop_view.set_visible(false);
        self.tools.crop_bar.set_visible(false);
        self.update_edit_actions();
        if self.is_open() {
            self.pointer_moved();
        }
    }

    fn set_aspect(&self, target: &str) {
        let label = ASPECTS
            .iter()
            .find(|(t, _)| *t == target)
            .map_or("Free", |(_, label)| label);
        self.tools.aspect.set_label(label);
        let (width, height) = self.canvas.shown_size();
        let ratio = match target {
            "free" => None,
            "original" => Some(width / height),
            _ => target
                .split_once(':')
                .and_then(|(w, h)| Some(w.parse::<f64>().ok()? / h.parse::<f64>().ok()?)),
        };
        self.tools.crop_view.set_aspect(ratio);
    }

    fn update_crop_bar(&self) {
        let saving = self.cropping.get() == Cropping::Saving;
        if saving {
            self.tools.crop_size.set_text("Saving…");
        } else {
            let (_, _, width, height) = self.tools.crop_view.rect();
            self.tools
                .crop_size
                .set_text(&format!("{width} × {height}"));
        }
        self.tools
            .apply
            .set_sensitive(!saving && !self.tools.crop_view.is_whole());
        self.tools.cancel.set_sensitive(!saving);
        self.tools.aspect.set_sensitive(!saving);
    }

    /// Enter or the Crop button: asks first, since the original is replaced.
    fn confirm_crop(&self) {
        let Some(item) = self.current.borrow().clone() else {
            return;
        };
        if self.cropping.get() != Cropping::On || self.tools.crop_view.is_whole() {
            return;
        }
        let (x, y, width, height) = self.tools.crop_view.rect();
        let rect = Rect {
            x,
            y,
            width,
            height,
        };
        let lossless = if item.format() == "JPEG" {
            " The crop is lossless: the picture isn't recompressed."
        } else {
            ""
        };
        let dialog = adw::AlertDialog::builder()
            .heading(format!("Crop “{}”?", item.entry().name))
            .body(format!(
                "The cropped picture ({width} × {height}) replaces the original, which can't be restored.{lossless}"
            ))
            .default_response("crop")
            .close_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("crop", "Crop")]);
        dialog.set_response_appearance("crop", adw::ResponseAppearance::Destructive);
        let me = self.me.clone();
        dialog.connect_response(None, move |_, response| {
            let Some(viewer) = me.upgrade() else {
                return;
            };
            viewer.widget.grab_focus();
            if response != "crop"
                || viewer.cropping.get() != Cropping::On
                || viewer.current.borrow().as_ref() != Some(&item)
            {
                return;
            }
            viewer.cropping.set(Cropping::Saving);
            viewer.tools.crop_view.set_frozen(true);
            viewer.update_crop_bar();
            viewer.queue_edit(item.clone(), Edit::Crop(rect));
        });
        dialog.present(Some(&self.widget));
    }

    /// Handles a key while cropping: Esc cancels, Enter crops; the rest
    /// (navigation, zoom) waits until the crop is done.
    pub(super) fn crop_key(&self, key: gdk::Key) {
        match key {
            gdk::Key::Escape => self.cancel_crop(),
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => self.confirm_crop(),
            _ => {}
        }
    }
}

/// Edits waiting to run, in order.
pub(super) type Queue = VecDeque<Job>;
