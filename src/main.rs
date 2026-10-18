//! FastViz: a fast, minimal photo and video browser.

mod cache;
mod decode;
mod fileops;
mod grid;
mod loader;
mod media;
mod model;
mod natsort;
mod prefs;
mod scan;
mod settings;
mod storage;
mod trace;
mod tree;
mod turbojpeg;
mod video;
mod window;

use adw::prelude::*;
use gtk::{gdk, glib};

const APP_ID: &str = "io.github.fastviz.FastViz";

fn main() -> glib::ExitCode {
    trace::init();
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|app| {
        trace!("gtk startup");
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        load_css();
        app.set_accels_for_action("win.show-pane", &["F9"]);
        app.set_accels_for_action("win.show-hidden", &["<Control>h"]);
        app.set_accels_for_action("win.preferences", &["<Control>comma"]);
    });
    app.connect_activate(|app| {
        // A second launch just raises the existing window.
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        window::Window::new(app, settings::Settings::load()).present();
        trace!("window presented");
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
