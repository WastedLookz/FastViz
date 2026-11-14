//! Entering and leaving the fullscreen viewer from the grid.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use super::Window;
use crate::model::FileItem;
use crate::scan;

impl Window {
    pub(super) fn setup_viewer(self: &Rc<Self>) {
        self.viewer.connect_exit(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |item| this.close_viewer(item)
        ));
        self.viewer.connect_delete(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |item| this.trash(vec![item.entry().path.clone()])
        ));
        self.viewer.connect_background(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |black| this.viewer_black.set(black)
        ));
        self.viewer.connect_volume(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |volume| this.volume.set(volume)
        ));
        self.viewer.connect_edited(glib::clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            None,
            move |old, entry| this.replace_edited(old, entry)
        ));
        self.viewer.connect_message(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |text| this.toast(adw::Toast::new(text))
        ));
    }

    /// A file rotated or cropped in the viewer: its grid item is replaced by
    /// a fresh one (new size and thumbnail; the date is kept, so it stays in
    /// place). `None` if it isn't in the open folder any more.
    fn replace_edited(&self, old: &FileItem, entry: scan::Entry) -> Option<FileItem> {
        let index = (0..self.store.n_items())
            .find(|&i| self.store.item(i).as_ref() == Some(old.upcast_ref()))?;
        self.thumbnailer.file_changed(&entry.path);
        self.tally([old.entry()], true);
        self.tally([&entry], false);
        let new = FileItem::new(entry);
        self.thumbnailer.register(std::slice::from_ref(&new));
        self.thumbnailer.want_metadata([&new]);
        self.store.splice(index, 1, std::slice::from_ref(&new));
        self.update_count_status();
        Some(new)
    }

    /// Opens the photo or video at `position` (in grid order) fullscreen.
    ///
    /// The viewer is a window of its own, opened already fullscreen: GNOME
    /// Shell animates a window that goes fullscreen by stretching it from its
    /// old frame (down over the taskbar, and back up when leaving), while a
    /// window that opens gets the shell's opening animation instead. With
    /// the main window as its parent that is the dialog one, unfolding from
    /// the middle of the screen while it fades in (100 ms). The grid's window
    /// never changes size.
    pub(super) fn open_viewer(self: &Rc<Self>, position: u32) {
        crate::trace!("fullscreen: open");
        // Leave the drive to the viewer while it is open.
        self.thumbnailer.pause_background(true);
        let screen = self.screen_size();
        if !self.viewer_window.is_visible() {
            self.viewer_window.set_transient_for(Some(&self.window));
            match self.monitor() {
                Some(monitor) => self.viewer_window.fullscreen_on_monitor(&monitor),
                None => self.viewer_window.fullscreen(),
            }
            self.viewer_window.present();
        }
        self.viewer
            .open(self.selection.upcast_ref(), position, screen);
    }

    /// Back to the grid, with the last item viewed selected and in view.
    fn close_viewer(&self, item: Option<FileItem>) {
        // Without a parent the shell closes it like any window: shrinking a
        // little towards the middle while it fades out (150 ms), over the
        // grid as it was (the dialog closing would fold it to a line).
        self.viewer_window.set_transient_for(None::<&gtk::Window>);
        self.viewer_window.set_visible(false);
        self.thumbnailer.pause_background(false);
        let position = item.and_then(|item| {
            (0..self.selection.n_items())
                .find(|&p| self.selection.item(p).as_ref() == Some(item.upcast_ref()))
        });
        if let Some(position) = position {
            self.grid.view.scroll_to(
                position,
                gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
                None,
            );
        }
        self.grid.view.grab_focus();
    }

    /// The main window's monitor, which the viewer fills.
    fn monitor(&self) -> Option<gdk::Monitor> {
        let display = WidgetExt::display(&self.window);
        // A file opened from outside can reach the viewer before the window
        // is on screen, with no surface (and no size) yet: then take the
        // first monitor, not a 1 x 1 "screen" (a blurry 1/8-scale preview).
        self.window
            .surface()
            .and_then(|surface| display.monitor_at_surface(&surface))
            .or_else(|| display.monitors().item(0).and_downcast::<gdk::Monitor>())
    }

    /// The monitor's size in device pixels, which previews are decoded for.
    fn screen_size(&self) -> (u32, u32) {
        let (width, height, scale) = match self.monitor() {
            Some(monitor) => {
                let geometry = monitor.geometry();
                (geometry.width(), geometry.height(), monitor.scale_factor())
            }
            None => (
                self.window.width(),
                self.window.height(),
                self.window.scale_factor(),
            ),
        };
        (
            (width * scale).max(1) as u32,
            (height * scale).max(1) as u32,
        )
    }

    /// F5: a slideshow from the item on screen, or from the selected (else
    /// first) photo or video in the grid.
    pub(super) fn start_slideshow(self: &Rc<Self>) {
        if !self.viewer.is_open() {
            let selected = self.selection.selection();
            let from = if selected.is_empty() {
                0
            } else {
                selected.minimum()
            };
            let media = |p: &u32| {
                self.selection
                    .item(*p)
                    .and_downcast::<FileItem>()
                    .is_some_and(|item| item.entry().kind.is_media())
            };
            let count = self.selection.n_items();
            let Some(position) = (from..count).chain(0..from).find(media) else {
                return;
            };
            self.open_viewer(position);
        }
        self.viewer.start_slideshow();
    }

    pub(super) fn set_viewer_black(&self, black: bool) {
        self.viewer_black.set(black);
        self.viewer.set_black(black);
    }
}
