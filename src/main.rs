//! FastViz: a fast, minimal photo and video browser.

mod cache;
mod decode;
mod fileops;
mod grid;
mod loader;
mod media;
mod model;
mod mpv;
mod natsort;
mod prefs;
mod scan;
mod settings;
mod storage;
mod trace;
mod tree;
mod turbojpeg;
mod video;
mod viewer;
mod window;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

const APP_ID: &str = "io.github.fastviz.FastViz";

fn main() -> glib::ExitCode {
    trace::init();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(|app| {
        trace!("gtk startup");
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        load_css();
        app.set_accels_for_action("win.show-pane", &["F9"]);
        app.set_accels_for_action("win.show-hidden", &["<Control>h"]);
        app.set_accels_for_action("win.preferences", &["<Control>comma"]);
        app.set_accels_for_action("win.slideshow", &["F5"]);
    });
    // One window; a second launch (or opening files) reuses it.
    let window: Rc<RefCell<Option<Rc<window::Window>>>> = Rc::default();
    let show = move |app: &adw::Application, path: Option<PathBuf>| {
        let existing = window.borrow().clone();
        match existing {
            Some(window) => {
                if let Some(path) = path {
                    window.open_path(path);
                }
                window.present();
            }
            None => {
                let created = window::Window::new(app, settings::Settings::load(), path);
                created.present();
                trace!("window presented");
                window.replace(Some(created));
            }
        }
    };
    let show = Rc::new(show);
    app.connect_activate(glib::clone!(
        #[strong]
        show,
        move |app| show(app, None)
    ));
    // Files from the file manager or the command line: the first one counts.
    app.connect_open(move |app, files, _| {
        show(app, files.iter().find_map(|file| file.path()));
    });
    app.run()
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
