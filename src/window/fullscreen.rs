//! Entering and leaving the fullscreen viewer from the grid.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::Window;
use crate::model::FileItem;

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
    }

    /// Opens the photo or video at `position` (in grid order) fullscreen.
    pub(super) fn open_viewer(self: &Rc<Self>, position: u32) {
        crate::trace!("fullscreen: open");
        // Leave the drive to the viewer while it is open.
        self.thumbnailer.pause_background(true);
        self.pages.set_visible_child_name("viewer");
        self.window.fullscreen();
        let screen = self.screen_size();
        self.viewer
            .open(self.selection.upcast_ref(), position, screen);
    }

    /// Back to the grid, with the last item viewed selected and in view.
    fn close_viewer(&self, item: Option<FileItem>) {
        self.window.unfullscreen();
        self.pages.set_visible_child_name("browser");
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

    /// The monitor's size in device pixels, which previews are decoded for.
    fn screen_size(&self) -> (u32, u32) {
        let display = WidgetExt::display(&self.window);
        let monitor = self
            .window
            .surface()
            .and_then(|surface| display.monitor_at_surface(&surface));
        let (width, height, scale) = match monitor {
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

    pub(super) fn set_viewer_black(&self, black: bool) {
        self.viewer_black.set(black);
        self.viewer.set_black(black);
    }
}
