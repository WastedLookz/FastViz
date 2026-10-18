//! FastViz: a fast, minimal photo and video browser.

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "io.github.fastviz.FastViz";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    });
    app.connect_activate(build_window);
    app.run()
}

fn build_window(app: &adw::Application) {
    let status = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .title("FastViz")
        .build();

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&status));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("FastViz")
        .default_width(1200)
        .default_height(800)
        .content(&view)
        .build();

    // Stage 0 check: confirm we run as a native Wayland client with a GPU renderer.
    window.connect_realize(move |w| {
        let backend = WidgetExt::display(w).type_().name();
        let renderer = w.renderer().map_or("none", |r| r.type_().name());
        let info = format!("GDK backend: {backend}\nGSK renderer: {renderer}");
        eprintln!("{info}");
        status.set_description(Some(&info));
    });

    window.present();
}
