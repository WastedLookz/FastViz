//! Decoding for the fullscreen viewer on its own threads, so it never waits
//! behind thumbnail work: the current item first, then its neighbors.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use image::RgbaImage;

use crate::decode::{self, Codec, Pixels};
use crate::media::Kind;
use crate::video;

pub struct Request {
    pub id: u64,
    pub path: PathBuf,
    pub kind: Kind,
    pub format: &'static str,
    /// A preview covering this screen size (device pixels), or `None` for full resolution.
    pub screen: Option<(u32, u32)>,
    /// Lower runs first.
    pub priority: u8,
}

impl Request {
    fn key(&self) -> (u64, bool) {
        (self.id, self.screen.is_none())
    }
}

pub enum Loaded {
    Still {
        id: u64,
        pixels: Pixels,
        /// The whole image's size; `pixels` may be smaller.
        width: u32,
        height: u32,
        full: bool,
    },
    Animation {
        id: u64,
        frames: Vec<(RgbaImage, u32)>,
    },
    Failed {
        id: u64,
        full: bool,
    },
}

#[derive(Default)]
struct Queue {
    waiting: Vec<Request>,
    /// (id, full) being decoded now; not requested again meanwhile.
    busy: HashSet<(u64, bool)>,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

pub struct Pool {
    shared: Arc<Shared>,
    pub results: async_channel::Receiver<Loaded>,
}

impl Pool {
    pub fn new(threads: usize) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::default(),
            ready: Condvar::new(),
        });
        let (tx, results) = async_channel::unbounded();
        for _ in 0..threads {
            let (shared, tx) = (shared.clone(), tx.clone());
            std::thread::Builder::new()
                .name("viewer-decode".into())
                .spawn(move || worker(&shared, &tx))
                .expect("failed to spawn viewer thread");
        }
        Self { shared, results }
    }

    /// Replaces whatever is still waiting; work already running finishes.
    pub fn request(&self, requests: Vec<Request>) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.waiting = requests
            .into_iter()
            .filter(|r| !queue.busy.contains(&r.key()))
            .collect();
        drop(queue);
        self.shared.ready.notify_all();
    }
}

fn worker(shared: &Shared, tx: &async_channel::Sender<Loaded>) {
    let mut codec = Codec::new();
    loop {
        let request = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let next = (0..queue.waiting.len()).min_by_key(|&i| queue.waiting[i].priority);
                if let Some(index) = next {
                    let request = queue.waiting.swap_remove(index);
                    queue.busy.insert(request.key());
                    break request;
                }
                queue = shared.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        let failed = Loaded::Failed {
            id: request.id,
            full: request.screen.is_none(),
        };
        let loaded = match catch_unwind(AssertUnwindSafe(|| load(&request, &mut codec))) {
            Ok(loaded) => loaded.unwrap_or(failed),
            Err(_) => {
                // A decoder panicked; its state may be broken, so start fresh.
                codec = Codec::new();
                failed
            }
        };
        shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .busy
            .remove(&request.key());
        if tx.send_blocking(loaded).is_err() {
            return;
        }
    }
}

fn load(request: &Request, codec: &mut Codec) -> Option<Loaded> {
    if request.kind == Kind::Video {
        // A still frame until playback exists; it never needs refining.
        let edge = request.screen.map_or(1920, |(w, h)| w.max(h));
        let thumb = video::thumbnail(&request.path, edge, request.format).ok()?;
        return Some(Loaded::Still {
            id: request.id,
            width: thumb.meta.width,
            height: thumb.meta.height,
            pixels: thumb.pixels,
            full: true,
        });
    }
    let data = std::fs::read(&request.path).ok()?;
    if let Some(frames) = decode::animation(&data) {
        return Some(Loaded::Animation {
            id: request.id,
            frames,
        });
    }
    let shown = decode::display(&data, request.screen, codec).ok()?;
    Some(Loaded::Still {
        id: request.id,
        pixels: shown.pixels,
        width: shown.width,
        height: shown.height,
        full: shown.full,
    })
}
