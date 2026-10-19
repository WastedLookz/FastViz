//! Video playback in the viewer: mpv draws through its OpenGL render API into
//! a GtkGLArea over the canvas (window embedding doesn't exist on Wayland),
//! with a small overlay of controls. Until the new file's first frame is up
//! the area stays transparent, so the poster frame underneath shows instead
//! of a black flash.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_float, c_int, c_uint, c_void};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::mpv::{Event, Player, Renderer, Value};
use crate::trace;

/// Observed mpv properties, by reply id.
const PAUSE: u64 = 1;
const TIME: u64 = 2;
const DURATION: u64 = 3;
const VOLUME: u64 = 4;
const MUTE: u64 = 5;
const EOF: u64 = 6;

/// Volume change per wheel notch, in percent.
const VOLUME_STEP: f64 = 5.0;
/// After the user drags the seek bar, position reports wait this long so
/// the knob doesn't jump back under the pointer.
const SCRUB_HOLD: Duration = Duration::from_millis(400);

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
}

#[link(name = "gtk-4")]
unsafe extern "C" {
    fn gdk_wayland_display_get_wl_display(display: *mut gdk::ffi::GdkDisplay) -> *mut c_void;
}

/// GTK draws with EGL on Wayland, so mpv resolves GL functions through it.
unsafe extern "C" fn get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    // SAFETY: `name` is a C string from mpv.
    unsafe { eglGetProcAddress(name) }
}

const GL_DRAW_FRAMEBUFFER_BINDING: c_uint = 0x8CA6;
const GL_FRAMEBUFFER: c_uint = 0x8D40;
const GL_SCISSOR_TEST: c_uint = 0x0C11;
const GL_COLOR_BUFFER_BIT: c_uint = 0x4000;

/// The few GL calls we make ourselves (mpv loads its own).
#[derive(Clone, Copy)]
struct Gl {
    get_integerv: unsafe extern "C" fn(c_uint, *mut c_int),
    bind_framebuffer: unsafe extern "C" fn(c_uint, c_uint),
    disable: unsafe extern "C" fn(c_uint),
    color_mask: unsafe extern "C" fn(u8, u8, u8, u8),
    clear_color: unsafe extern "C" fn(c_float, c_float, c_float, c_float),
    clear: unsafe extern "C" fn(c_uint),
}

impl Gl {
    /// Needs a current GL context.
    fn load() -> Option<Self> {
        let get = |name: &CStr| {
            // SAFETY: a valid C string; null if the function doesn't exist.
            let pointer = unsafe { eglGetProcAddress(name.as_ptr()) };
            (!pointer.is_null()).then_some(pointer)
        };
        let pointers = (
            get(c"glGetIntegerv")?,
            get(c"glBindFramebuffer")?,
            get(c"glDisable")?,
            get(c"glColorMask")?,
            get(c"glClearColor")?,
            get(c"glClear")?,
        );
        // SAFETY: these are the standard GL entry points with these signatures.
        unsafe {
            Some(Self {
                get_integerv: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(c_uint, *mut c_int),
                >(pointers.0),
                bind_framebuffer: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(c_uint, c_uint),
                >(pointers.1),
                disable: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_uint)>(
                    pointers.2,
                ),
                color_mask: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u8, u8, u8, u8)>(
                    pointers.3,
                ),
                clear_color: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(c_float, c_float, c_float, c_float),
                >(pointers.4),
                clear: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_uint)>(pointers.5),
            })
        }
    }

    fn framebuffer(&self) -> c_int {
        let mut fbo = 0;
        // SAFETY: the binding is a single integer.
        unsafe { (self.get_integerv)(GL_DRAW_FRAMEBUFFER_BINDING, &mut fbo) };
        fbo
    }

    /// Makes framebuffer `fbo` fully transparent, whatever state mpv left.
    fn clear(&self, fbo: c_int) {
        // SAFETY: plain GL calls with a context current.
        unsafe {
            (self.bind_framebuffer)(GL_FRAMEBUFFER, fbo as c_uint);
            (self.disable)(GL_SCISSOR_TEST);
            (self.color_mask)(1, 1, 1, 1);
            (self.clear_color)(0.0, 0.0, 0.0, 0.0);
            (self.clear)(GL_COLOR_BUFFER_BIT);
        }
    }
}

/// mpv's letterbox color: black or the canvas's dark gray.
fn background(black: bool) -> &'static str {
    if black { "#000000" } else { "#1e1e1e" }
}

/// "1:05" or "1:02:03".
fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

type Handler = Box<dyn Fn()>;
type ValueHandler = Box<dyn Fn(f64)>;

pub struct Playback {
    me: Weak<Self>,
    pub area: gtk::GLArea,
    pub controls: gtk::Box,
    play_button: gtk::Button,
    time_label: gtk::Label,
    seek: gtk::Adjustment,
    mute_button: gtk::Button,
    volume_scale: gtk::Adjustment,
    player: RefCell<Option<Rc<Player>>>,
    renderer: RefCell<Option<Renderer>>,
    gl: Cell<Option<Gl>>,
    /// The area's framebuffer size in device pixels.
    size: Cell<(i32, i32)>,
    /// mpv or OpenGL couldn't start; the viewer shows still frames instead.
    unavailable: Cell<bool>,
    /// Files loaded so far; mpv numbers playlist entries the same way.
    loads: Cell<i64>,
    /// Playlist entry of the file wanted now, 0 for none.
    entry: Cell<i64>,
    /// mpv started the wanted file (so its next playback restart is ours).
    started: Cell<bool>,
    /// mpv has a frame of the wanted file; until then the area is transparent.
    has_frame: Cell<bool>,
    failed: Cell<bool>,
    paused: Cell<bool>,
    eof: Cell<bool>,
    duration: Cell<f64>,
    volume: Cell<f64>,
    muted: Cell<bool>,
    black: Cell<bool>,
    controls_hovered: Cell<bool>,
    scrubbed: Cell<Option<Instant>>,
    loaded_at: Cell<Option<Instant>>,
    /// Idle inhibition while a video plays, so the screen doesn't blank.
    inhibit: RefCell<Option<(gtk::Application, u32)>>,
    on_ready: RefCell<Option<Handler>>,
    on_ended: RefCell<Option<Handler>>,
    on_volume: RefCell<Option<ValueHandler>>,
    on_wheel: RefCell<Option<ValueHandler>>,
}

impl Playback {
    pub fn new(volume: f64, black: bool) -> Rc<Self> {
        let area = gtk::GLArea::builder()
            .has_depth_buffer(false)
            .has_stencil_buffer(false)
            .auto_render(false)
            .can_target(false)
            .visible(false)
            .build();

        let play_button = control_button("media-playback-pause-symbolic", "Play/Pause (Space)");
        let mute_button = control_button("audio-volume-high-symbolic", "Mute (M)");
        let time_label = gtk::Label::builder()
            .label("0:00 / 0:00")
            .css_classes(["fv-time"])
            .build();
        let seek = gtk::Adjustment::new(0.0, 0.0, 1.0, 1.0, 5.0, 0.0);
        let seek_scale = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .adjustment(&seek)
            .draw_value(false)
            .hexpand(true)
            .focusable(false)
            .focus_on_click(false)
            .build();
        let volume_scale =
            gtk::Adjustment::new(volume.clamp(0.0, 100.0), 0.0, 100.0, 1.0, 10.0, 0.0);
        let volume_slider = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .adjustment(&volume_scale)
            .draw_value(false)
            .width_request(100)
            .focusable(false)
            .focus_on_click(false)
            .tooltip_text("Volume (wheel)")
            .build();
        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .margin_bottom(24)
            .width_request(720)
            .css_classes(["fv-controls"])
            .visible(false)
            .build();
        controls.append(&play_button);
        controls.append(&time_label);
        controls.append(&seek_scale);
        controls.append(&mute_button);
        controls.append(&volume_slider);

        let playback = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            area,
            controls,
            play_button,
            time_label,
            seek,
            mute_button,
            volume_scale,
            player: RefCell::default(),
            renderer: RefCell::default(),
            gl: Cell::new(None),
            size: Cell::new((0, 0)),
            unavailable: Cell::new(false),
            loads: Cell::new(0),
            entry: Cell::new(0),
            started: Cell::new(false),
            has_frame: Cell::new(false),
            failed: Cell::new(false),
            paused: Cell::new(false),
            eof: Cell::new(false),
            duration: Cell::new(0.0),
            volume: Cell::new(volume.clamp(0.0, 100.0)),
            muted: Cell::new(false),
            black: Cell::new(black),
            controls_hovered: Cell::new(false),
            scrubbed: Cell::new(None),
            loaded_at: Cell::new(None),
            inhibit: RefCell::default(),
            on_ready: RefCell::default(),
            on_ended: RefCell::default(),
            on_volume: RefCell::default(),
            on_wheel: RefCell::default(),
        });
        playback.connect_signals(&seek_scale, &volume_slider);
        playback.update_controls();
        playback
    }

    /// Called when the current video shows its first frame or fails.
    pub fn connect_ready(&self, f: impl Fn() + 'static) {
        self.on_ready.replace(Some(Box::new(f)));
    }

    /// Called when the current video plays to its end.
    pub fn connect_ended(&self, f: impl Fn() + 'static) {
        self.on_ended.replace(Some(Box::new(f)));
    }

    /// Called when the volume changes (to remember it).
    pub fn connect_volume(&self, f: impl Fn(f64) + 'static) {
        self.on_volume.replace(Some(Box::new(f)));
    }

    /// Called with wheel notches over the seek bar, which leaves the wheel
    /// to the viewer (only the volume slider takes it).
    pub fn connect_wheel(&self, f: impl Fn(f64) + 'static) {
        self.on_wheel.replace(Some(Box::new(f)));
    }

    /// Starts playing `path`; false if playback can't work here at all.
    pub fn play(&self, path: &Path) -> bool {
        let Some(player) = self.ensure_player() else {
            return false;
        };
        self.area.set_visible(true);
        if !self.ensure_renderer(&player) {
            self.unavailable.set(true);
            self.area.set_visible(false);
            return false;
        }
        if self.is_active() && self.has_frame.get() {
            trace_frames(&player);
        }
        self.loads.set(self.loads.get() + 1);
        self.entry.set(self.loads.get());
        self.started.set(false);
        self.has_frame.set(false);
        self.failed.set(false);
        self.eof.set(false);
        self.duration.set(0.0);
        self.seek.set_upper(1.0);
        self.seek.set_value(0.0);
        self.loaded_at.set(Some(Instant::now()));
        self.set_paused(&player, false);
        if let Err(e) = player.load(path) {
            trace!("video: can't load {}: {e}", path.display());
            self.failed.set(true);
        }
        self.area.queue_render();
        self.update_controls();
        true
    }

    /// Unloads the video (leaving it, or the viewer).
    pub fn stop(&self) {
        if self.entry.replace(0) != 0
            && let Some(player) = self.player.borrow().as_ref()
        {
            if self.has_frame.get() {
                trace_frames(player);
            }
            let _ = player.command(&["stop"]);
        }
        self.started.set(false);
        self.has_frame.set(false);
        self.failed.set(false);
        self.area.set_visible(false);
        self.controls.set_visible(false);
        self.update_inhibit();
    }

    /// Playing a file now (mpv draws it, not the canvas).
    pub fn is_active(&self) -> bool {
        self.entry.get() != 0
    }

    /// The current video shows a frame, or won't.
    pub fn is_ready(&self) -> bool {
        self.has_frame.get() || self.failed.get()
    }

    /// The current video played to its end.
    pub fn ended(&self) -> bool {
        self.eof.get()
    }

    pub fn failed(&self) -> bool {
        self.failed.get()
    }

    pub fn unavailable(&self) -> bool {
        self.unavailable.get()
    }

    pub fn toggle_pause(&self) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        if !self.is_active() {
            return;
        }
        if self.eof.get() {
            // Ended: play again from the start.
            let _ = player.command(&["seek", "0", "absolute"]);
            self.set_paused(&player, false);
        } else {
            self.set_paused(&player, !self.paused.get());
        }
    }

    pub fn set_pause(&self, paused: bool) {
        if let Some(player) = self.player.borrow().clone()
            && self.is_active()
        {
            self.set_paused(&player, paused);
        }
    }

    /// Pauses or resumes, updating our copy now: mpv reports the change
    /// later, and a quick second toggle must see this one.
    fn set_paused(&self, player: &Player, paused: bool) {
        player.set_flag("pause", paused);
        self.paused.set(paused);
        self.update_controls();
    }

    pub fn toggle_mute(&self) {
        self.set_muted(!self.muted.get());
    }

    /// Like pausing, our copy updates now so quick repeats add up.
    fn set_muted(&self, muted: bool) {
        if let Some(player) = self.player.borrow().as_ref() {
            player.set_flag("mute", muted);
        }
        self.muted.set(muted);
        self.update_controls();
    }

    /// Wheel: `notches` up (positive) raises the volume.
    pub fn step_volume(&self, notches: f64) {
        let volume = (self.volume.get() + notches * VOLUME_STEP).clamp(0.0, 100.0);
        self.set_volume(volume);
    }

    fn set_volume(&self, volume: f64) {
        if let Some(player) = self.player.borrow().as_ref() {
            player.set_double("volume", volume);
        }
        // Turning it up unmutes.
        if self.muted.get() && volume > self.volume.get() {
            self.set_muted(false);
        }
        self.volume_changed(volume);
        self.update_controls();
    }

    pub fn set_black(&self, black: bool) {
        self.black.set(black);
        if let Some(player) = self.player.borrow().as_ref() {
            player.set_string("background", background(black));
        }
    }

    /// Shows or hides the controls overlay (only for a video that plays).
    pub fn show_controls(&self, show: bool) {
        self.controls
            .set_visible(show && self.is_active() && !self.failed.get());
    }

    pub fn controls_hovered(&self) -> bool {
        self.controls.is_visible() && self.controls_hovered.get()
    }

    /// Whether (x, y) in `ancestor`'s coordinates is on the controls.
    pub fn controls_at(&self, ancestor: &impl IsA<gtk::Widget>, x: f64, y: f64) -> bool {
        self.controls.is_visible()
            && ancestor
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .is_some_and(|w| w == self.controls || w.is_ancestor(&self.controls))
    }

    fn ensure_player(&self) -> Option<Rc<Player>> {
        if let Some(player) = self.player.borrow().as_ref() {
            return Some(player.clone());
        }
        if self.unavailable.get() {
            return None;
        }
        let started = Instant::now();
        let volume = self.volume.get().to_string();
        let options = [
            ("vo", "libmpv"),
            // VA-API when it works, else software decoding.
            ("hwdec", "auto-safe"),
            ("gpu-hwdec-interop", "vaapi"),
            // Stop on the last frame instead of unloading.
            ("keep-open", "yes"),
            // Never block our render call waiting for a frame's display time.
            ("video-timing-offset", "0"),
            // mpv 0.37's default Lanczos chroma scaling garbles the color
            // plane of NV12 frames (all VA-API output) at some sizes here,
            // e.g. 1440x1440 and 4K phone clips: diagonal blue/orange streaks.
            ("cscale", "bilinear"),
            ("osd-level", "0"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            // Don't look for subtitle, audio or cover files next to each clip
            // (extra metadata reads on slow drives).
            ("sub-auto", "no"),
            ("audio-file-auto", "no"),
            ("cover-art-auto", "no"),
            ("audio-display", "no"),
            ("volume-max", "100"),
            ("volume", volume.as_str()),
            ("background", background(self.black.get())),
        ];
        let mut player = match Player::new(&options) {
            Ok(player) => player,
            Err(e) => {
                eprintln!("Video playback unavailable: mpv failed to start: {e}");
                self.unavailable.set(true);
                return None;
            }
        };
        // mpv wakes us from its threads; events are handled on the main loop.
        let (tx, rx) = async_channel::bounded::<()>(1);
        player.set_wakeup(move || {
            let _ = tx.try_send(());
        });
        for (id, name) in [(PAUSE, "pause"), (MUTE, "mute"), (EOF, "eof-reached")] {
            player.observe_flag(id, name);
        }
        for (id, name) in [
            (TIME, "time-pos"),
            (DURATION, "duration"),
            (VOLUME, "volume"),
        ] {
            player.observe_double(id, name);
        }
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            while rx.recv().await.is_ok() {
                let Some(playback) = me.upgrade() else {
                    return;
                };
                playback.handle_events();
            }
        });
        trace!(
            "video: mpv started in {:.1} ms",
            started.elapsed().as_secs_f64() * 1e3
        );
        self.player.replace(Some(player.clone()));
        Some(player)
    }

    fn ensure_renderer(&self, player: &Rc<Player>) -> bool {
        if self.renderer.borrow().is_some() {
            return true;
        }
        let started = Instant::now();
        if !self.area.is_realized() {
            self.area.realize();
        }
        self.area.make_current();
        if let Some(e) = self.area.error() {
            eprintln!("Video playback unavailable: no OpenGL context: {e}");
            return false;
        }
        let Some(gl) = Gl::load() else {
            eprintln!("Video playback unavailable: GL functions not found through EGL");
            return false;
        };
        let display = WidgetExt::display(&self.area);
        let wl_display = if display.type_().name() == "GdkWaylandDisplay" {
            // SAFETY: `display` is a live GdkWaylandDisplay.
            unsafe { gdk_wayland_display_get_wl_display(display.as_ptr()) }
        } else {
            std::ptr::null_mut()
        };
        let mut renderer = match Renderer::new(player, get_proc_address, wl_display) {
            Ok(renderer) => renderer,
            Err(e) => {
                eprintln!("Video playback unavailable: mpv can't render with OpenGL: {e}");
                return false;
            }
        };
        // New frames arrive on mpv's threads; draw them on the main loop.
        let (tx, rx) = async_channel::bounded::<()>(1);
        renderer.set_update(move || {
            let _ = tx.try_send(());
        });
        let area = self.area.downgrade();
        glib::spawn_future_local(async move {
            while rx.recv().await.is_ok() {
                match area.upgrade() {
                    Some(area) => area.queue_render(),
                    None => return,
                }
            }
        });
        self.gl.set(Some(gl));
        self.renderer.replace(Some(renderer));
        trace!(
            "video: OpenGL renderer ready in {:.1} ms (Wayland display: {})",
            started.elapsed().as_secs_f64() * 1e3,
            !wl_display.is_null()
        );
        true
    }

    fn render(&self) {
        let Some(gl) = self.gl.get() else {
            return;
        };
        let renderer = self.renderer.borrow();
        let Some(renderer) = renderer.as_ref() else {
            return;
        };
        let fbo = gl.framebuffer();
        let (width, height) = self.size.get();
        // mpv always draws (keeping its frame queue moving); before the new
        // file's first frame the result is cleared so the poster shows through.
        renderer.render(fbo, width, height);
        if !self.has_frame.get() {
            gl.clear(fbo);
        }
    }

    fn handle_events(&self) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        while let Some(event) = player.next_event() {
            match event {
                Event::StartFile { entry } => {
                    if entry == self.entry.get() {
                        self.started.set(true);
                    }
                }
                Event::PlaybackRestart => {
                    if self.started.get() && !self.has_frame.get() {
                        self.has_frame.set(true);
                        self.area.queue_render();
                        if let Some(loaded) = self.loaded_at.take() {
                            trace!(
                                "video: playing {:.1} ms after loading (hwdec: {})",
                                loaded.elapsed().as_secs_f64() * 1e3,
                                player.get_string("hwdec-current").unwrap_or_default()
                            );
                        }
                        self.update_inhibit();
                        self.ready();
                    }
                }
                Event::EndFile {
                    entry,
                    error: Some(error),
                } if entry == self.entry.get() => {
                    trace!("video: can't play: {error}");
                    self.failed.set(true);
                    self.controls.set_visible(false);
                    self.ready();
                }
                Event::Property { id, value } => self.property_changed(id, value),
                _ => {}
            }
        }
    }

    fn ready(&self) {
        if let Some(f) = self.on_ready.borrow().as_ref() {
            f();
        }
    }

    fn property_changed(&self, id: u64, value: Value) {
        match (id, value) {
            (PAUSE, Value::Flag(paused)) => {
                self.paused.set(paused);
                self.update_inhibit();
            }
            (EOF, Value::Flag(eof)) => {
                self.eof.set(eof);
                self.update_inhibit();
                if eof
                    && self.has_frame.get()
                    && let Some(f) = self.on_ended.borrow().as_ref()
                {
                    f();
                }
            }
            (MUTE, Value::Flag(muted)) => self.muted.set(muted),
            (VOLUME, Value::Double(volume)) => self.volume_changed(volume),
            (DURATION, Value::Double(duration)) => {
                self.duration.set(duration);
                self.seek.set_upper(duration.max(1.0));
            }
            (DURATION, Value::None) => self.duration.set(0.0),
            (TIME, Value::Double(time)) => {
                let scrubbing = self
                    .scrubbed
                    .get()
                    .is_some_and(|at| at.elapsed() < SCRUB_HOLD);
                if !scrubbing {
                    self.seek.set_value(time);
                }
            }
            _ => return,
        }
        self.update_controls();
    }

    fn volume_changed(&self, volume: f64) {
        let changed = (self.volume.replace(volume) - volume).abs() > 0.01;
        if changed && let Some(f) = self.on_volume.borrow().as_ref() {
            f(volume);
        }
    }

    fn update_controls(&self) {
        let playing = !self.paused.get() && !self.eof.get();
        self.play_button.set_icon_name(if playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
        let volume = self.volume.get();
        self.mute_button.set_icon_name(match volume {
            _ if self.muted.get() || volume <= 0.0 => "audio-volume-muted-symbolic",
            v if v < 34.0 => "audio-volume-low-symbolic",
            v if v < 67.0 => "audio-volume-medium-symbolic",
            _ => "audio-volume-high-symbolic",
        });
        if (self.volume_scale.value() - volume).abs() > 0.01 {
            self.volume_scale.set_value(volume);
        }
        self.time_label.set_text(&format!(
            "{} / {}",
            clock(self.seek.value()),
            clock(self.duration.get())
        ));
    }

    /// Keeps the screen awake while a video is playing.
    fn update_inhibit(&self) {
        let playing =
            self.is_active() && self.has_frame.get() && !self.paused.get() && !self.eof.get();
        let mut inhibit = self.inhibit.borrow_mut();
        match (playing, inhibit.as_ref()) {
            (true, None) => {
                let window = self.area.root().and_downcast::<gtk::Window>();
                if let Some((window, app)) = window.and_then(|w| w.application().map(|a| (w, a))) {
                    let cookie = app.inhibit(
                        Some(&window),
                        gtk::ApplicationInhibitFlags::IDLE,
                        Some("Playing a video"),
                    );
                    *inhibit = Some((app, cookie));
                }
            }
            (false, Some(_)) => {
                if let Some((app, cookie)) = inhibit.take() {
                    app.uninhibit(cookie);
                }
            }
            _ => {}
        }
    }

    fn connect_signals(self: &Rc<Self>, seek_scale: &gtk::Scale, volume_slider: &gtk::Scale) {
        self.area.connect_render(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, _| {
                playback.render();
                glib::Propagation::Stop
            }
        ));
        // Desktop GL where possible: GTK would pick GLES, on which mpv 0.37
        // can't measure HDR peaks (PQ clips without metadata look washed out).
        self.area.connect_create_context(|area| {
            let surface = area.native()?.surface()?;
            let context = surface.create_gl_context().ok()?;
            context.set_allowed_apis(gdk::GLAPI::GL);
            if context.realize().is_err() {
                context.set_allowed_apis(gdk::GLAPI::GL | gdk::GLAPI::GLES);
                if let Err(e) = context.realize() {
                    eprintln!("No OpenGL context for video: {e}");
                    return None;
                }
            }
            Some(context)
        });
        // The area's framebuffer size in device pixels.
        self.area.connect_resize(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            move |_, width, height| playback.size.set((width, height))
        ));
        self.area.connect_unrealize(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            move |area| {
                // Unload first so mpv doesn't report the lost output as a
                // failure; it frees its GL objects with our context current.
                if playback.entry.replace(0) != 0
                    && let Some(player) = playback.player.borrow().as_ref()
                {
                    let _ = player.command(&["stop"]);
                }
                area.make_current();
                playback.renderer.take();
                playback.gl.set(None);
            }
        ));

        self.play_button.connect_clicked(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            move |_| playback.toggle_pause()
        ));
        self.mute_button.connect_clicked(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            move |_| playback.toggle_mute()
        ));
        seek_scale.connect_change_value(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, _, value| {
                playback.scrubbed.set(Some(Instant::now()));
                if let Some(player) = playback.player.borrow().as_ref() {
                    let _ = player.command(&["seek", &format!("{value:.3}"), "absolute"]);
                }
                glib::Propagation::Proceed
            }
        ));
        // The seek bar would seek on the wheel; the viewer gets it instead.
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
        wheel.connect_scroll(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |controller, _, dy| {
                if let Some(f) = playback.on_wheel.borrow().as_ref() {
                    f(notches(controller, dy));
                }
                glib::Propagation::Stop
            }
        ));
        seek_scale.add_controller(wheel);
        volume_slider.connect_change_value(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, _, value| {
                playback.set_volume(value.clamp(0.0, 100.0));
                glib::Propagation::Proceed
            }
        ));

        let hover = gtk::EventControllerMotion::new();
        hover.connect_enter(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            move |_, _, _| playback.controls_hovered.set(true)
        ));
        hover.connect_leave(glib::clone!(
            #[weak(rename_to = playback)]
            self,
            move |_| playback.controls_hovered.set(false)
        ));
        self.controls.add_controller(hover);
    }
}

/// How smoothly the video that is being left played, for the trace.
fn trace_frames(player: &Player) {
    if trace::enabled() {
        let get = |name| player.get_double(name).unwrap_or(0.0);
        trace!(
            "video: left at {:.1} s, {} frames dropped by the decoder, {} shown late",
            get("time-pos"),
            get("frame-drop-count"),
            get("vo-delayed-frame-count")
        );
    }
}

/// Wheel notches from a scroll event (touchpads report pixels).
pub fn notches(controller: &gtk::EventControllerScroll, dy: f64) -> f64 {
    match controller.unit() {
        gdk::ScrollUnit::Wheel => dy,
        _ => dy / 40.0,
    }
}

fn control_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .css_classes(["flat", "circular"])
        .focusable(false)
        .focus_on_click(false)
        .build()
}
