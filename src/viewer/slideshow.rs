//! Slideshow: the viewer moves on by itself every few seconds, and after a
//! video plays to its end. Space pauses, a click stops it (staying on the
//! item), Esc stops it and leaves fullscreen. While it runs, "next" and
//! "previous" follow its order, so preloading works when shuffled too.

use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;

use super::Viewer;
use crate::model::FileItem;
use crate::trace;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlideshowOptions {
    /// Seconds per photo.
    pub interval: f64,
    /// Start over after the last item.
    pub repeat: bool,
    pub shuffle: bool,
}

pub(super) struct Slideshow {
    options: SlideshowOptions,
    /// The shuffled order (empty when not shuffling: grid order).
    order: Vec<FileItem>,
    paused: bool,
    timer: Option<glib::SourceId>,
    /// Keeps the screen awake.
    inhibit: Option<(gtk::Application, u32)>,
}

impl Viewer {
    pub fn set_slideshow_options(&self, options: SlideshowOptions) {
        self.slideshow_options.set(options);
    }

    /// Starts a slideshow from the current item (if one isn't running).
    pub fn start_slideshow(&self) {
        let Some(current) = self.current.borrow().clone() else {
            return;
        };
        if self.slideshow.borrow().is_some() {
            return;
        }
        let options = self.slideshow_options.get();
        let order = if options.shuffle {
            self.shuffled(&current)
        } else {
            Vec::new()
        };
        let inhibit = self
            .widget
            .root()
            .and_downcast::<gtk::Window>()
            .and_then(|window| {
                let app = window.application()?;
                let flags = gtk::ApplicationInhibitFlags::IDLE;
                let cookie = app.inhibit(Some(&window), flags, Some("Slideshow"));
                Some((app, cookie))
            });
        trace!(
            "slideshow: start, {} s per photo, loop {}, shuffle {}",
            options.interval, options.repeat, options.shuffle
        );
        self.slideshow.replace(Some(Slideshow {
            options,
            order,
            paused: false,
            timer: None,
            inhibit,
        }));
        self.playback.set_pause(false);
        self.request();
        self.slideshow_arm();
        self.update_overlays();
    }

    pub(super) fn stop_slideshow(&self) {
        let Some(mut slideshow) = self.slideshow.take() else {
            return;
        };
        if let Some(timer) = slideshow.timer.take() {
            timer.remove();
        }
        if let Some((app, cookie)) = slideshow.inhibit.take() {
            app.uninhibit(cookie);
        }
        trace!("slideshow: stop");
        self.request();
        self.update_overlays();
    }

    pub(super) fn slideshow_paused(&self) -> bool {
        self.slideshow.borrow().as_ref().is_some_and(|s| s.paused)
    }

    pub(super) fn toggle_slideshow_pause(&self) {
        let paused = {
            let mut slideshow = self.slideshow.borrow_mut();
            let Some(slideshow) = slideshow.as_mut() else {
                return;
            };
            slideshow.paused = !slideshow.paused;
            slideshow.paused
        };
        if self.video_current() {
            if !paused && self.playback.ended() {
                // It ended while paused.
                self.slideshow_advance();
                return;
            }
            self.playback.set_pause(paused);
        }
        self.slideshow_arm();
        self.update_overlays();
    }

    /// (Re)starts the wait for the current item: the interval for a photo,
    /// the end for a video that plays.
    pub(super) fn slideshow_arm(&self) {
        let mut slideshow = self.slideshow.borrow_mut();
        let Some(slideshow) = slideshow.as_mut() else {
            return;
        };
        if let Some(timer) = slideshow.timer.take() {
            timer.remove();
        }
        let playing = self.video_current()
            && self.playback.is_active()
            && !self.playback.failed()
            && !self.playback.ended();
        if slideshow.paused || playing {
            return;
        }
        let me = self.me.clone();
        let interval = Duration::from_secs_f64(slideshow.options.interval.max(0.5));
        slideshow.timer = Some(glib::timeout_add_local_once(interval, move || {
            if let Some(viewer) = me.upgrade() {
                if let Some(slideshow) = viewer.slideshow.borrow_mut().as_mut() {
                    slideshow.timer = None;
                }
                viewer.slideshow_advance();
            }
        }));
    }

    pub(super) fn slideshow_video_ended(&self) {
        if self.slideshow.borrow().as_ref().is_some_and(|s| !s.paused) {
            self.slideshow_advance();
        }
    }

    fn slideshow_advance(&self) {
        match self.neighbor(1) {
            Some(next) => {
                trace!("slideshow: next, {}", next.1.entry().name);
                self.go(Some(next));
            }
            None => {
                trace!("slideshow: reached the end");
                self.stop_slideshow();
            }
        }
    }

    /// The item `step` away in slideshow order, wrapping when looping.
    pub(super) fn slideshow_neighbor(&self, step: i64) -> Option<(u32, FileItem)> {
        let slideshow = self.slideshow.borrow();
        let slideshow = slideshow.as_ref()?;
        let repeat = slideshow.options.repeat;
        let order = &slideshow.order;
        if order.is_empty() {
            let start = self.locate()?;
            let next = self.media_from(i64::from(start) + step, step);
            if next.is_some() || !repeat {
                return next;
            }
            let count = self.model.borrow().as_ref().map_or(0, |m| m.n_items());
            return if step > 0 {
                self.media_from(0, 1)
            } else {
                self.media_from(i64::from(count) - 1, -1)
            };
        }
        let current = self.current.borrow().clone()?;
        let count = order.len() as i64;
        let mut index = order.iter().position(|item| *item == current)? as i64;
        // Each other item at most once, skipping any deleted meanwhile.
        for _ in 1..count {
            index += step;
            if !(0..count).contains(&index) {
                if !repeat {
                    return None;
                }
                index = index.rem_euclid(count);
            }
            let item = &order[index as usize];
            if let Some(position) = self.position_of(item) {
                return Some((position, item.clone()));
            }
        }
        // Only the current item is left.
        repeat
            .then(|| self.locate().map(|position| (position, current)))
            .flatten()
    }

    /// All photos and videos in random order, `first` first.
    fn shuffled(&self, first: &FileItem) -> Vec<FileItem> {
        let Some(model) = self.model.borrow().clone() else {
            return Vec::new();
        };
        let mut items: Vec<FileItem> = (0..model.n_items())
            .filter_map(|p| model.item(p).and_downcast::<FileItem>())
            .filter(|item| item.entry().kind.is_media() && item != first)
            .collect();
        // Fisher-Yates with GLib's generator.
        for i in (1..items.len()).rev() {
            let j = glib::random_int_range(0, i as i32 + 1) as usize;
            items.swap(i, j);
        }
        items.insert(0, first.clone());
        items
    }
}
