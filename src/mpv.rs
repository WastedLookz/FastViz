//! Minimal binding to libmpv's client and OpenGL render APIs.
//!
//! The client API (2.x, mpv 0.35+) is stable C, and playback needs about
//! twenty calls, so we bind them directly instead of using the `libmpv2`
//! crate: its render context borrows the player, which doesn't fit inside a
//! GTK widget without leaking it, and it is LGPL while this binary is MIT.

use std::ffi::{CStr, CString, c_char, c_double, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::rc::Rc;

#[repr(C)]
struct Handle {
    _private: [u8; 0],
}

#[repr(C)]
struct RenderContext {
    _private: [u8; 0],
}

#[repr(C)]
struct RawEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct RawProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct RawStartFile {
    playlist_entry_id: i64,
}

#[repr(C)]
struct RawEndFile {
    reason: c_int,
    error: c_int,
    playlist_entry_id: i64,
}

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct OpenGlInitParams {
    get_proc_address: GetProcAddress,
    ctx: *mut c_void,
}

#[repr(C)]
struct OpenGlFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

pub type GetProcAddress =
    unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void;
type Callback = unsafe extern "C" fn(data: *mut c_void);

const FORMAT_STRING: c_int = 1;
const FORMAT_FLAG: c_int = 3;
const FORMAT_DOUBLE: c_int = 5;

const EVENT_NONE: c_int = 0;
const EVENT_SHUTDOWN: c_int = 1;
const EVENT_START_FILE: c_int = 6;
const EVENT_END_FILE: c_int = 7;
const EVENT_FILE_LOADED: c_int = 8;
const EVENT_PLAYBACK_RESTART: c_int = 21;
const EVENT_PROPERTY_CHANGE: c_int = 22;

const END_FILE_REASON_ERROR: c_int = 4;

const RENDER_PARAM_API_TYPE: c_int = 1;
const RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const RENDER_PARAM_OPENGL_FBO: c_int = 3;
const RENDER_PARAM_FLIP_Y: c_int = 4;
const RENDER_PARAM_WL_DISPLAY: c_int = 9;

/// glibc's value; mpv refuses to start unless numbers format the C way.
const LC_NUMERIC: c_int = 1;

#[link(name = "mpv")]
unsafe extern "C" {
    fn mpv_client_api_version() -> std::ffi::c_ulong;
    fn mpv_create() -> *mut Handle;
    fn mpv_initialize(ctx: *mut Handle) -> c_int;
    fn mpv_terminate_destroy(ctx: *mut Handle);
    fn mpv_error_string(error: c_int) -> *const c_char;
    fn mpv_free(data: *mut c_void);
    fn mpv_set_option_string(ctx: *mut Handle, name: *const c_char, data: *const c_char) -> c_int;
    fn mpv_command(ctx: *mut Handle, args: *mut *const c_char) -> c_int;
    fn mpv_set_property(
        ctx: *mut Handle,
        name: *const c_char,
        format: c_int,
        data: *mut c_void,
    ) -> c_int;
    fn mpv_get_property(
        ctx: *mut Handle,
        name: *const c_char,
        format: c_int,
        data: *mut c_void,
    ) -> c_int;
    fn mpv_observe_property(
        ctx: *mut Handle,
        reply_userdata: u64,
        name: *const c_char,
        format: c_int,
    ) -> c_int;
    fn mpv_wait_event(ctx: *mut Handle, timeout: c_double) -> *mut RawEvent;
    fn mpv_set_wakeup_callback(ctx: *mut Handle, cb: Option<Callback>, data: *mut c_void);

    fn mpv_render_context_create(
        res: *mut *mut RenderContext,
        mpv: *mut Handle,
        params: *mut RenderParam,
    ) -> c_int;
    fn mpv_render_context_set_update_callback(
        ctx: *mut RenderContext,
        cb: Option<Callback>,
        data: *mut c_void,
    );
    fn mpv_render_context_render(ctx: *mut RenderContext, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_free(ctx: *mut RenderContext);
}

unsafe extern "C" {
    fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char;
}

fn error_string(error: c_int) -> String {
    // SAFETY: mpv returns a static string for any error code.
    unsafe { CStr::from_ptr(mpv_error_string(error)) }
        .to_string_lossy()
        .into_owned()
}

fn check(error: c_int) -> Result<(), String> {
    if error < 0 {
        Err(error_string(error))
    } else {
        Ok(())
    }
}

fn cstring(text: &str) -> CString {
    CString::new(text).expect("mpv names contain no NUL bytes")
}

/// A thread-safe notification mpv calls from its own threads.
type Notify = Box<dyn Fn() + Send + Sync>;

unsafe extern "C" fn call_notify(data: *mut c_void) {
    // SAFETY: `data` is the `Notify` box registered with this callback,
    // freed only after mpv can no longer call it.
    unsafe { (*data.cast::<Notify>())() }
}

#[derive(Debug)]
pub enum Value {
    Flag(bool),
    Double(f64),
    /// The property is unavailable (e.g. `duration` with nothing loaded).
    None,
}

#[derive(Debug)]
pub enum Event {
    StartFile {
        entry: i64,
    },
    FileLoaded,
    /// Playback (re)started after loading or seeking: a frame is up.
    PlaybackRestart,
    /// A file stopped; `error` is set if it couldn't be played.
    EndFile {
        entry: i64,
        error: Option<String>,
    },
    Property {
        id: u64,
        value: Value,
    },
    Shutdown,
}

/// The client API version of the libmpv in use, as (major, minor).
pub fn api_version() -> (u32, u32) {
    // SAFETY: no arguments, no state.
    let version = unsafe { mpv_client_api_version() };
    ((version >> 16) as u32, (version & 0xffff) as u32)
}

/// The playback core: one per process, created on first use.
pub struct Player {
    handle: *mut Handle,
    wakeup: *mut Notify,
}

impl Player {
    /// Creates and starts mpv with `options` (set before initialization).
    pub fn new(options: &[(&str, &str)]) -> Result<Rc<Self>, String> {
        // SAFETY: plain libc/libmpv calls on the main thread; GTK has set the
        // locale from the environment, and mpv needs C number formatting.
        unsafe {
            setlocale(LC_NUMERIC, c"C".as_ptr());
            let handle = mpv_create();
            if handle.is_null() {
                return Err("mpv_create failed".to_owned());
            }
            for (name, value) in options {
                let error =
                    mpv_set_option_string(handle, cstring(name).as_ptr(), cstring(value).as_ptr());
                if error < 0 {
                    eprintln!("mpv: can't set {name}={value}: {}", error_string(error));
                }
            }
            if let Err(e) = check(mpv_initialize(handle)) {
                mpv_terminate_destroy(handle);
                return Err(e);
            }
            Ok(Rc::new(Self {
                handle,
                wakeup: ptr::null_mut(),
            }))
        }
    }

    /// Calls `notify` (from any thread) when events are waiting; drain them
    /// with [`Player::next_event`] on the main thread, not from `notify`.
    pub fn set_wakeup(self: &mut Rc<Self>, notify: impl Fn() + Send + Sync + 'static) {
        let player = Rc::get_mut(self).expect("set_wakeup before sharing the player");
        let boxed: *mut Notify = Box::into_raw(Box::new(Box::new(notify)));
        // SAFETY: the box lives until after mpv is destroyed (see Drop).
        unsafe { mpv_set_wakeup_callback(player.handle, Some(call_notify), boxed.cast()) };
        player.wakeup = boxed;
    }

    fn command_raw(&self, args: &[CString]) -> Result<(), String> {
        let mut pointers: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        pointers.push(ptr::null());
        // SAFETY: a NULL-terminated array of valid C strings.
        check(unsafe { mpv_command(self.handle, pointers.as_mut_ptr()) })
    }

    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        let args: Vec<CString> = args.iter().map(|a| cstring(a)).collect();
        self.command_raw(&args)
    }

    /// Replaces whatever is playing with `path` (any bytes, not just UTF-8).
    pub fn load(&self, path: &Path) -> Result<(), String> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        self.command_raw(&[cstring("loadfile"), path, cstring("replace")])
    }

    pub fn set_flag(&self, name: &str, value: bool) {
        let mut flag = c_int::from(value);
        // SAFETY: FORMAT_FLAG takes a pointer to an int.
        let error = unsafe {
            mpv_set_property(
                self.handle,
                cstring(name).as_ptr(),
                FORMAT_FLAG,
                (&raw mut flag).cast(),
            )
        };
        let _ = check(error);
    }

    pub fn set_double(&self, name: &str, value: f64) {
        let mut value = value;
        // SAFETY: FORMAT_DOUBLE takes a pointer to a double.
        let error = unsafe {
            mpv_set_property(
                self.handle,
                cstring(name).as_ptr(),
                FORMAT_DOUBLE,
                (&raw mut value).cast(),
            )
        };
        let _ = check(error);
    }

    pub fn set_string(&self, name: &str, value: &str) {
        let value = cstring(value);
        let mut pointer = value.as_ptr();
        // SAFETY: FORMAT_STRING takes a pointer to a char pointer.
        let error = unsafe {
            mpv_set_property(
                self.handle,
                cstring(name).as_ptr(),
                FORMAT_STRING,
                (&raw mut pointer).cast(),
            )
        };
        let _ = check(error);
    }

    pub fn get_double(&self, name: &str) -> Option<f64> {
        let mut value = 0.0;
        // SAFETY: FORMAT_DOUBLE writes a double.
        let error = unsafe {
            mpv_get_property(
                self.handle,
                cstring(name).as_ptr(),
                FORMAT_DOUBLE,
                (&raw mut value).cast(),
            )
        };
        (error >= 0).then_some(value)
    }

    pub fn get_string(&self, name: &str) -> Option<String> {
        let mut pointer: *mut c_char = ptr::null_mut();
        // SAFETY: on success mpv stores a string we must free with mpv_free.
        unsafe {
            let error = mpv_get_property(
                self.handle,
                cstring(name).as_ptr(),
                FORMAT_STRING,
                (&raw mut pointer).cast(),
            );
            if error < 0 || pointer.is_null() {
                return None;
            }
            let text = CStr::from_ptr(pointer).to_string_lossy().into_owned();
            mpv_free(pointer.cast());
            Some(text)
        }
    }

    /// Reports changes to a flag property as `Event::Property { id, .. }`.
    pub fn observe_flag(&self, id: u64, name: &str) {
        // SAFETY: a plain call with a valid C string.
        unsafe { mpv_observe_property(self.handle, id, cstring(name).as_ptr(), FORMAT_FLAG) };
    }

    /// Reports changes to a numeric property as `Event::Property { id, .. }`.
    pub fn observe_double(&self, id: u64, name: &str) {
        // SAFETY: a plain call with a valid C string.
        unsafe { mpv_observe_property(self.handle, id, cstring(name).as_ptr(), FORMAT_DOUBLE) };
    }

    /// The next waiting event we care about, without blocking.
    pub fn next_event(&self) -> Option<Event> {
        loop {
            // SAFETY: mpv returns a valid event that stays valid until the
            // next mpv_wait_event call; we copy what we need out of it.
            let event = unsafe { &*mpv_wait_event(self.handle, 0.0) };
            let parsed = match event.event_id {
                EVENT_NONE => return None,
                EVENT_SHUTDOWN => Event::Shutdown,
                EVENT_START_FILE => Event::StartFile {
                    // SAFETY: START_FILE carries mpv_event_start_file.
                    entry: unsafe { (*event.data.cast::<RawStartFile>()).playlist_entry_id },
                },
                EVENT_FILE_LOADED => Event::FileLoaded,
                EVENT_PLAYBACK_RESTART => Event::PlaybackRestart,
                EVENT_END_FILE => {
                    // SAFETY: END_FILE carries mpv_event_end_file.
                    let end = unsafe { &*event.data.cast::<RawEndFile>() };
                    let error =
                        (end.reason == END_FILE_REASON_ERROR).then(|| error_string(end.error));
                    Event::EndFile {
                        entry: end.playlist_entry_id,
                        error,
                    }
                }
                EVENT_PROPERTY_CHANGE => {
                    // SAFETY: PROPERTY_CHANGE carries mpv_event_property whose
                    // data matches its format.
                    let property = unsafe { &*event.data.cast::<RawProperty>() };
                    let value = match property.format {
                        FORMAT_FLAG => Value::Flag(unsafe { *property.data.cast::<c_int>() } != 0),
                        FORMAT_DOUBLE => Value::Double(unsafe { *property.data.cast::<f64>() }),
                        _ => Value::None,
                    };
                    Event::Property {
                        id: event.reply_userdata,
                        value,
                    }
                }
                _ => continue,
            };
            return Some(parsed);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: after terminate_destroy mpv calls nothing of ours.
        unsafe {
            mpv_terminate_destroy(self.handle);
            if !self.wakeup.is_null() {
                drop(Box::from_raw(self.wakeup));
            }
        }
    }
}

/// mpv's OpenGL renderer, bound to one GL context.
pub struct Renderer {
    context: *mut RenderContext,
    update: *mut Notify,
    /// mpv must outlive its render context.
    _player: Rc<Player>,
}

impl Renderer {
    /// Must be called with the GL context current. `wl_display` (or null)
    /// lets mpv use VA-API hardware decoding on Wayland.
    pub fn new(
        player: &Rc<Player>,
        get_proc_address: GetProcAddress,
        wl_display: *mut c_void,
    ) -> Result<Self, String> {
        let mut init = OpenGlInitParams {
            get_proc_address,
            ctx: ptr::null_mut(),
        };
        let mut params = vec![
            RenderParam {
                kind: RENDER_PARAM_API_TYPE,
                data: c"opengl".as_ptr().cast_mut().cast(),
            },
            RenderParam {
                kind: RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&raw mut init).cast(),
            },
        ];
        if !wl_display.is_null() {
            params.push(RenderParam {
                kind: RENDER_PARAM_WL_DISPLAY,
                data: wl_display,
            });
        }
        params.push(RenderParam {
            kind: 0,
            data: ptr::null_mut(),
        });
        let mut context = ptr::null_mut();
        // SAFETY: the parameters are only read during the call.
        check(unsafe {
            mpv_render_context_create(&mut context, player.handle, params.as_mut_ptr())
        })?;
        Ok(Self {
            context,
            update: ptr::null_mut(),
            _player: player.clone(),
        })
    }

    /// Calls `notify` (from any thread) when there is a new frame to draw.
    pub fn set_update(&mut self, notify: impl Fn() + Send + Sync + 'static) {
        let boxed: *mut Notify = Box::into_raw(Box::new(Box::new(notify)));
        // SAFETY: the box lives until the render context is freed (see Drop).
        unsafe {
            mpv_render_context_set_update_callback(self.context, Some(call_notify), boxed.cast());
            if !self.update.is_null() {
                drop(Box::from_raw(self.update));
            }
        }
        self.update = boxed;
    }

    /// Draws the current frame into framebuffer `fbo` of `width` × `height`
    /// device pixels. The GL context must be current.
    pub fn render(&self, fbo: i32, width: i32, height: i32) {
        let mut target = OpenGlFbo {
            fbo,
            w: width,
            h: height,
            internal_format: 0,
        };
        let mut flip: c_int = 1;
        let mut params = [
            RenderParam {
                kind: RENDER_PARAM_OPENGL_FBO,
                data: (&raw mut target).cast(),
            },
            RenderParam {
                kind: RENDER_PARAM_FLIP_Y,
                data: (&raw mut flip).cast(),
            },
            RenderParam {
                kind: 0,
                data: ptr::null_mut(),
            },
        ];
        // SAFETY: the parameters are only read during the call.
        let _ = check(unsafe { mpv_render_context_render(self.context, params.as_mut_ptr()) });
    }
}

impl Drop for Renderer {
    /// The GL context must be current.
    fn drop(&mut self) {
        // SAFETY: freeing the context stops update callbacks, so the box can go.
        unsafe {
            mpv_render_context_free(self.context);
            if !self.update.is_null() {
                drop(Box::from_raw(self.update));
            }
        }
    }
}
