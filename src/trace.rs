//! Optional timing trace on stderr, enabled with `PHOTOVIEWER_TRACE=1`.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Option<Instant>> = OnceLock::new();

pub fn init() {
    START.get_or_init(|| {
        std::env::var_os("PHOTOVIEWER_TRACE")
            .is_some_and(|v| !v.is_empty() && v != "0")
            .then(Instant::now)
    });
}

pub fn enabled() -> bool {
    matches!(START.get(), Some(Some(_)))
}

pub fn emit(args: std::fmt::Arguments) {
    if let Some(Some(start)) = START.get() {
        eprintln!("[{:9.1} ms] {args}", start.elapsed().as_secs_f64() * 1e3);
    }
}

/// Logs a timestamped line when tracing is enabled; costs one branch otherwise.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::trace::enabled() {
            $crate::trace::emit(format_args!($($arg)*))
        }
    };
}

/// Developer aid: with `PHOTOVIEWER_SNAPSHOT=/path.png`, renders `window` to a
/// PNG after `delay_ms` and quits, so the layout can be checked without a screenshot tool.
pub fn snapshot_on_request(window: &gtk::Window, delay_ms: u64) {
    use gtk::prelude::*;

    let Some(path) = std::env::var_os("PHOTOVIEWER_SNAPSHOT") else {
        return;
    };
    let delay_ms = std::env::var("PHOTOVIEWER_SNAPSHOT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(delay_ms);
    let window = window.clone();
    gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(delay_ms), move || {
        let paintable = gtk::WidgetPaintable::new(Some(&window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, window.width().into(), window.height().into());
        let texture = snapshot
            .to_node()
            .zip(window.renderer())
            .map(|(node, renderer)| renderer.render_texture(node, None));
        match texture.map(|t| t.save_to_png(&path)) {
            Some(Ok(())) => eprintln!("snapshot saved to {}", path.to_string_lossy()),
            Some(Err(e)) => eprintln!("snapshot failed: {e}"),
            None => eprintln!("snapshot failed: nothing rendered"),
        }
        if let Some(app) = window.application() {
            app.quit();
        }
    });
}
