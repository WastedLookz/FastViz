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

/// Developer aid: with `PHOTOVIEWER_AUTOSCROLL=<px per frame>`, scrolls the
/// grid continuously (bouncing at the ends) and traces frame times and memory
/// once a second, to check scrolling stays smooth and memory bounded.
pub fn autoscroll_on_request(scroller: &gtk::ScrolledWindow) {
    use gtk::prelude::*;
    use std::cell::{Cell, RefCell};

    let Some(step) = std::env::var("PHOTOVIEWER_AUTOSCROLL")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
    else {
        return;
    };
    let direction = Cell::new(1.0);
    let last_frame = Cell::new(None::<i64>);
    let intervals = RefCell::new(Vec::<f64>::new());
    let report_at = Cell::new(0i64);
    scroller.add_tick_callback(move |scroller, clock| {
        let adj = scroller.vadjustment();
        let max = adj.upper() - adj.page_size();
        let next = adj.value() + step * direction.get();
        if next >= max || next <= 0.0 {
            direction.set(-direction.get());
        }
        adj.set_value(next.clamp(0.0, max.max(0.0)));

        let now = clock.frame_time();
        if let Some(previous) = last_frame.replace(Some(now)) {
            intervals.borrow_mut().push((now - previous) as f64 / 1e3);
        }
        if now >= report_at.get() {
            report_at.set(now + 1_000_000);
            let mut frames = intervals.take();
            if !frames.is_empty() {
                frames.sort_by(f64::total_cmp);
                let slow = frames.iter().filter(|&&ms| ms > 20.0).count();
                emit(format_args!(
                    "scroll: {} frames, median {:.1} ms, worst {:.1} ms, {} over 20 ms | RSS {} MiB",
                    frames.len(),
                    frames[frames.len() / 2],
                    frames[frames.len() - 1],
                    slow,
                    rss_mib()
                ));
            }
        }
        gtk::glib::ControlFlow::Continue
    });
}

fn rss_mib() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| (pages * 4096) >> 20)
}
