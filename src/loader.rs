//! Thumbnail loading pipeline.
//!
//! The grid registers the items its tiles show as "wants", prioritized by
//! distance to the viewport. Worker threads always take the most urgent want:
//!
//! - cache readers (SSD): a hit is decoded right away; a miss moves on;
//! - source readers (the photo drive; one read at a time on spinning disks,
//!   which only seek with more): read the file, just its first 128 KB if the
//!   EXIF thumbnail suffices; videos go through FFmpeg here;
//! - decoders: scale, orient and encode for the cache;
//! - one writer thread stores results in SQLite.
//!
//! Items scrolled away are dropped from the wants, so nothing piles up. When
//! nothing on screen is waiting, the rest of the folder is cached in the
//! background, nearest to the view first, so scrolling finds it ready.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::cache::{self, Key};
use crate::decode::{self, Codec, PREFIX_LEN, Pixels};
use crate::grid::Tile;
use crate::media::{Kind, Meta};
use crate::model::FileItem;
use crate::trace;
use crate::video;

/// Thumbnail sizes stored in the cache (longer edge, device pixels).
pub const TIERS: [u32; 3] = [128, 256, 512];
const CACHE_READERS: usize = 3;
/// Reader threads for the photo drive; how many may read at once depends on
/// the drive (see `Thumbnailer::reset`).
const SOURCE_READERS: usize = 4;
/// Items further than this many viewport heights away aren't loaded yet.
const PREFETCH_SCREENS: f32 = 2.0;
/// Longest wait for a folder scan before reading thumbnails from the drive
/// anyway. While a scan runs, new items keep sorting in, so what's on screen
/// changes; reading early wastes disk time and slows the scan.
const SCAN_HEAD_START: Duration = Duration::from_secs(3);
/// Background items queued for the drive at once; keeps them from crowding
/// out new on-screen work.
const BACKGROUND_IN_FLIGHT: usize = 2;
const BACKGROUND_PRIORITY: u32 = u32::MAX - 1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Cache,
    Source,
    Busy,
}

/// An item to thumbnail, as the workers see it.
#[derive(Clone)]
struct Job {
    id: u64,
    serial: u64,
    key: Key,
    tier: u32,
    kind: Kind,
    format: &'static str,
    /// Filling the cache for the folder, not for something on screen.
    background: bool,
}

struct Want {
    job: Job,
    priority: u32,
    stage: Stage,
}

/// A folder item that may need work later: (id, key, kind, format).
type Pending = (u64, Key, Kind, &'static str);

#[derive(Default)]
struct Queue {
    wants: HashMap<u64, Want>,
    next_serial: u64,
    /// Header-only reads for sorting, done when nothing else is wanted.
    meta: VecDeque<Pending>,
    /// The folder's items to make sure are cached, nearest to the view first.
    background: VecDeque<Pending>,
    background_tier: u32,
    /// Holds back drive reads while a folder scan runs: they would compete for
    /// the same disk, and later batches can re-sort what's on screen.
    source_paused: bool,
    /// Parallel reads the current folder's drive tolerates, and reads running.
    source_slots: usize,
    source_busy: usize,
}

impl Queue {
    fn serial(&mut self) -> u64 {
        self.next_serial += 1;
        self.next_serial
    }

    /// The most urgent want in `stage`, marked busy.
    fn take(&mut self, stage: Stage) -> Option<Job> {
        let want = self
            .wants
            .values_mut()
            .filter(|w| w.stage == stage)
            .min_by_key(|w| w.priority)?;
        want.stage = Stage::Busy;
        Some(want.job.clone())
    }

    /// Removes a want once served, unless it was replaced meanwhile.
    fn finish(&mut self, job: &Job) {
        if self
            .wants
            .get(&job.id)
            .is_some_and(|w| w.job.serial == job.serial)
        {
            self.wants.remove(&job.id);
        }
    }

    fn advance(&mut self, job: &Job) -> bool {
        match self.wants.get_mut(&job.id) {
            Some(want) if want.job.serial == job.serial => {
                want.stage = Stage::Source;
                true
            }
            _ => false,
        }
    }

    /// Whether the drive can take another background item.
    fn background_ready(&self) -> bool {
        !self.source_paused
            && !self.background.is_empty()
            && self.wants.values().filter(|w| w.job.background).count() < BACKGROUND_IN_FLIGHT
    }
}

#[derive(Default)]
struct Stats {
    cache_hits: AtomicU64,
    generated: AtomicU64,
    embedded: AtomicU64,
    videos: AtomicU64,
    failed: AtomicU64,
    meta: AtomicU64,
    read_us: AtomicU64,
    decode_us: AtomicU64,
}

struct Shared {
    queue: Mutex<Queue>,
    cache_ready: Condvar,
    source_ready: Condvar,
    stats: Stats,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn finish(&self, job: &Job) {
        self.lock().finish(job);
        if job.background {
            // Room for the next background item.
            self.cache_ready.notify_one();
        }
    }
}

/// Results sent back to the UI thread.
enum Event {
    Thumb {
        id: u64,
        tier: u32,
        pixels: Pixels,
        native: bool,
        meta: Option<Meta>,
        background: bool,
    },
    Failed {
        id: u64,
        meta: Option<Meta>,
    },
    Meta {
        id: u64,
        meta: Meta,
    },
}

struct DecodeJob {
    job: Job,
    data: Vec<u8>,
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .expect("failed to spawn thumbnail thread");
}

fn start_workers(
    shared: &Arc<Shared>,
    events: async_channel::Sender<Event>,
) -> mpsc::Sender<cache::Write> {
    let writer = cache::spawn_writer();
    let decoders = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 8));
    let (decode_tx, decode_rx) = async_channel::bounded::<DecodeJob>(decoders * 2);

    for _ in 0..CACHE_READERS {
        let (shared, events) = (shared.clone(), events.clone());
        spawn("thumb-cache", move || cache_reader(&shared, &events));
    }
    for _ in 0..SOURCE_READERS {
        let (shared, events, writer, decode_tx) = (
            shared.clone(),
            events.clone(),
            writer.clone(),
            decode_tx.clone(),
        );
        spawn("thumb-read", move || {
            source_reader(&shared, &events, &writer, &decode_tx)
        });
    }
    for _ in 0..decoders {
        let (shared, events, writer, decode_rx) = (
            shared.clone(),
            events.clone(),
            writer.clone(),
            decode_rx.clone(),
        );
        spawn("thumb-decode", move || {
            decoder(&shared, &events, &writer, &decode_rx)
        });
    }
    writer
}

enum CacheWork {
    Want(Job),
    Background(Pending, u32),
}

fn cache_reader(shared: &Shared, events: &async_channel::Sender<Event>) {
    let conn = cache::open().ok();
    let mut codec = Codec::new();
    loop {
        let work = {
            let mut queue = shared.lock();
            loop {
                if let Some(job) = queue.take(Stage::Cache) {
                    break CacheWork::Want(job);
                }
                if queue.background_ready() {
                    let tier = queue.background_tier;
                    if let Some(pending) = queue.background.pop_front() {
                        break CacheWork::Background(pending, tier);
                    }
                }
                queue = shared
                    .cache_ready
                    .wait(queue)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        let job = match work {
            CacheWork::Want(job) => job,
            CacheWork::Background((id, key, kind, format), tier) => {
                // Already cached: nothing to do (not even decoding it).
                let cached = conn
                    .as_ref()
                    .is_some_and(|conn| cache::has_thumb(conn, &key, tier));
                let mut queue = shared.lock();
                if !cached && !queue.wants.contains_key(&id) {
                    let job = Job {
                        id,
                        serial: queue.serial(),
                        key,
                        tier,
                        kind,
                        format,
                        background: true,
                    };
                    let want = Want {
                        job,
                        priority: BACKGROUND_PRIORITY,
                        stage: Stage::Source,
                    };
                    queue.wants.insert(id, want);
                    drop(queue);
                    shared.source_ready.notify_one();
                }
                continue;
            }
        };

        let cached = conn
            .as_ref()
            .and_then(|conn| cache::thumb(conn, &job.key, job.tier).ok().flatten());
        let pixels = cached.as_ref().and_then(|c| {
            catch_unwind(AssertUnwindSafe(|| {
                decode::decode_cached(c.encoding, &c.data, &mut codec)
            }))
            .ok()
            .and_then(Result::ok)
        });
        match (cached, pixels) {
            (Some(cached), Some(pixels)) if cached.tier >= job.tier || cached.native => {
                shared.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
                let _ = events.send_blocking(Event::Thumb {
                    id: job.id,
                    tier: cached.tier,
                    pixels,
                    native: cached.native,
                    meta: None,
                    background: false,
                });
                shared.finish(&job);
            }
            (cached, pixels) => {
                // Show a smaller cached tier while the real one is made.
                if let (Some(cached), Some(pixels)) = (cached, pixels) {
                    let _ = events.send_blocking(Event::Thumb {
                        id: job.id,
                        tier: cached.tier,
                        pixels,
                        native: false,
                        meta: None,
                        background: false,
                    });
                }
                if shared.lock().advance(&job) {
                    shared.source_ready.notify_one();
                }
            }
        }
    }
}

enum SourceJob {
    Thumb(Job),
    Meta(Pending),
}

fn source_reader(
    shared: &Shared,
    events: &async_channel::Sender<Event>,
    writer: &mpsc::Sender<cache::Write>,
    decode_tx: &async_channel::Sender<DecodeJob>,
) {
    let mut codec = Codec::new();
    loop {
        let work = {
            let mut queue = shared.lock();
            loop {
                if !queue.source_paused && queue.source_busy < queue.source_slots.max(1) {
                    let work = match queue.take(Stage::Source) {
                        Some(job) => Some(SourceJob::Thumb(job)),
                        None => queue.meta.pop_front().map(SourceJob::Meta),
                    };
                    if let Some(work) = work {
                        queue.source_busy += 1;
                        break work;
                    }
                }
                queue = shared
                    .source_ready
                    .wait(queue)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        // The slot is released as soon as this thread stops reading the drive.
        let release = || {
            shared.lock().source_busy -= 1;
            shared.source_ready.notify_one();
        };
        match work {
            SourceJob::Thumb(job) if job.kind == Kind::Video => {
                // FFmpeg reads only what it needs (index, one keyframe), so it
                // runs here, holding the drive slot.
                let started = Instant::now();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    video::thumbnail(&job.key.path, job.tier, job.format)
                }));
                release();
                record(&shared.stats.read_us, started);
                match result {
                    Ok(Ok(thumb)) => {
                        shared.stats.videos.fetch_add(1, Ordering::Relaxed);
                        deliver(shared, events, writer, &job, thumb, &mut codec);
                    }
                    _ => fail(shared, events, writer, &job, None),
                }
            }
            SourceJob::Thumb(job) => {
                let started = Instant::now();
                let Ok(mut file) = File::open(&job.key.path) else {
                    release();
                    fail(shared, events, writer, &job, None);
                    continue;
                };
                let mut data = Vec::with_capacity(PREFIX_LEN.min(job.key.size as usize));
                let complete = match (&mut file).take(PREFIX_LEN as u64).read_to_end(&mut data) {
                    Ok(_) => data.len() < PREFIX_LEN,
                    Err(_) => {
                        release();
                        fail(shared, events, writer, &job, None);
                        continue;
                    }
                };
                // A big-enough EXIF thumbnail saves reading the rest of a JPEG.
                let embedded = catch_unwind(AssertUnwindSafe(|| {
                    decode::embedded_jpeg_thumbnail(&data, job.tier, &mut codec)
                }))
                .ok()
                .flatten();
                if let Some(thumb) = embedded {
                    release();
                    record(&shared.stats.read_us, started);
                    shared.stats.embedded.fetch_add(1, Ordering::Relaxed);
                    deliver(shared, events, writer, &job, thumb, &mut codec);
                    continue;
                }
                let read_ok = complete || file.read_to_end(&mut data).is_ok();
                release();
                if !read_ok {
                    fail(shared, events, writer, &job, None);
                    continue;
                }
                record(&shared.stats.read_us, started);
                if decode_tx.send_blocking(DecodeJob { job, data }).is_err() {
                    return;
                }
            }
            SourceJob::Meta((id, key, kind, format)) => {
                let meta = if kind == Kind::Video {
                    catch_unwind(|| video::probe(&key.path, format))
                        .ok()
                        .flatten()
                } else {
                    read_meta(&key)
                };
                release();
                shared.stats.meta.fetch_add(1, Ordering::Relaxed);
                let Some(meta) = meta else {
                    continue;
                };
                let _ = writer.send(cache::Write::Meta {
                    key,
                    meta: Some(meta),
                    failed: false,
                });
                let _ = events.send_blocking(Event::Meta { id, meta });
            }
        }
    }
}

fn read_meta(key: &Key) -> Option<Meta> {
    let mut data = Vec::with_capacity(PREFIX_LEN);
    File::open(&key.path)
        .ok()?
        .take(PREFIX_LEN as u64)
        .read_to_end(&mut data)
        .ok()?;
    catch_unwind(|| decode::metadata(&data)).ok().flatten()
}

fn decoder(
    shared: &Shared,
    events: &async_channel::Sender<Event>,
    writer: &mpsc::Sender<cache::Write>,
    jobs: &async_channel::Receiver<DecodeJob>,
) {
    let mut codec = Codec::new();
    while let Ok(DecodeJob { job, data }) = jobs.recv_blocking() {
        let started = Instant::now();
        let result = catch_unwind(AssertUnwindSafe(|| {
            decode::thumbnail(&data, job.tier, &mut codec)
        }));
        record(&shared.stats.decode_us, started);
        match result {
            Ok(Ok(thumb)) => {
                shared.stats.generated.fetch_add(1, Ordering::Relaxed);
                deliver(shared, events, writer, &job, thumb, &mut codec);
            }
            Ok(Err(_)) => {
                let meta = decode::metadata(&data);
                fail(shared, events, writer, &job, meta);
            }
            Err(_) => {
                // A decoder panicked; its state may be broken, so start fresh.
                codec = Codec::new();
                fail(shared, events, writer, &job, None);
            }
        }
    }
}

fn deliver(
    shared: &Shared,
    events: &async_channel::Sender<Event>,
    writer: &mpsc::Sender<cache::Write>,
    job: &Job,
    thumb: decode::Thumbnail,
    codec: &mut Codec,
) {
    if let Ok((encoding, data)) = decode::encode(&thumb.pixels, codec) {
        let _ = writer.send(cache::Write::Thumb {
            key: job.key.clone(),
            tier: job.tier,
            encoding,
            native: thumb.native,
            data,
        });
    }
    let _ = writer.send(cache::Write::Meta {
        key: job.key.clone(),
        meta: Some(thumb.meta),
        failed: false,
    });
    let _ = events.send_blocking(Event::Thumb {
        id: job.id,
        tier: job.tier,
        pixels: thumb.pixels,
        native: thumb.native,
        meta: Some(thumb.meta),
        background: job.background,
    });
    shared.finish(job);
}

fn fail(
    shared: &Shared,
    events: &async_channel::Sender<Event>,
    writer: &mpsc::Sender<cache::Write>,
    job: &Job,
    meta: Option<Meta>,
) {
    shared.stats.failed.fetch_add(1, Ordering::Relaxed);
    let _ = writer.send(cache::Write::Meta {
        key: job.key.clone(),
        meta,
        failed: true,
    });
    let _ = events.send_blocking(Event::Failed { id: job.id, meta });
    shared.finish(job);
}

fn record(total: &AtomicU64, since: Instant) {
    total.fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// Least-recently-used accounting for thumbnail textures.
struct Lru {
    clock: u64,
    entries: HashMap<u64, (u64, usize)>,
    total: usize,
    budget: usize,
}

impl Lru {
    fn touch(&mut self, id: u64) {
        self.clock += 1;
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.0 = self.clock;
        }
    }

    fn insert(&mut self, id: u64, bytes: usize) {
        self.clock += 1;
        if let Some((_, old)) = self.entries.insert(id, (self.clock, bytes)) {
            self.total -= old;
        }
        self.total += bytes;
    }

    /// Least recently used ids to drop to get back under budget, skipping `keep`.
    fn evict(&mut self, keep: &HashSet<u64>) -> Vec<u64> {
        if self.total <= self.budget {
            return Vec::new();
        }
        let mut order: Vec<(u64, u64)> = self
            .entries
            .iter()
            .filter(|(id, _)| !keep.contains(id))
            .map(|(&id, &(stamp, _))| (stamp, id))
            .collect();
        order.sort_unstable();
        let target = self.budget / 10 * 9;
        let mut evicted = Vec::new();
        for (_, id) in order {
            if self.total <= target {
                break;
            }
            if let Some((_, bytes)) = self.entries.remove(&id) {
                self.total -= bytes;
                evicted.push(id);
            }
        }
        evicted
    }
}

/// Texture memory budget: 1/16 of available RAM, between 128 MiB and 1 GiB.
fn texture_budget() -> usize {
    let available = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find_map(|line| line.strip_prefix("MemAvailable:"))
                .and_then(|rest| {
                    rest.trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
        });
    (available.unwrap_or(4 << 20) * 1024 / 16).clamp(128 << 20, 1 << 30)
}

type MetaHandler = Box<dyn Fn(&FileItem)>;
type BoundTile = (glib::WeakRef<Tile>, glib::WeakRef<gtk::ListItem>);

/// UI-thread side: tracks which items the grid shows and applies results.
pub struct Thumbnailer {
    me: std::rc::Weak<Self>,
    shared: Arc<Shared>,
    writer: mpsc::Sender<cache::Write>,
    items: RefCell<HashMap<u64, glib::WeakRef<FileItem>>>,
    /// Tiles showing each item, with their list item (for its position).
    bound: RefCell<HashMap<u64, BoundTile>>,
    viewport: RefCell<Option<glib::WeakRef<gtk::ScrolledWindow>>>,
    tier: Cell<u32>,
    sync_queued: Cell<bool>,
    lru: RefCell<Lru>,
    on_meta: RefCell<Option<MetaHandler>>,
    /// When the current folder was opened, until its first screen is filled.
    opened: Cell<Option<Instant>>,
    /// Bumped per folder, so a stale scan timeout doesn't resume the next one.
    folder: Cell<u64>,
    /// The folder scan is still running: load only what's on screen, no prefetch.
    scanning: Cell<bool>,
    /// Last scroll offset, direction (+1 down, -1 up) and when it moved.
    scroll: Cell<(f64, f32, Option<Instant>)>,
    /// A sync is scheduled for when scrolling comes to rest.
    rest_check: Cell<bool>,
    /// The background list needs rebuilding (new folder order, view or size).
    background_dirty: Cell<bool>,
    /// List position of the first row on screen, as of the last sync.
    first_visible: Cell<u32>,
}

impl Thumbnailer {
    pub fn new() -> Rc<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::default(),
            cache_ready: Condvar::new(),
            source_ready: Condvar::new(),
            stats: Stats::default(),
        });
        let (events_tx, events) = async_channel::unbounded();
        let writer = start_workers(&shared, events_tx);

        let budget = texture_budget();
        trace!("thumbnail texture budget: {} MiB", budget >> 20);
        let this = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            shared,
            writer,
            items: RefCell::default(),
            bound: RefCell::default(),
            viewport: RefCell::default(),
            tier: Cell::new(TIERS[1]),
            sync_queued: Cell::new(false),
            lru: RefCell::new(Lru {
                clock: 0,
                entries: HashMap::new(),
                total: 0,
                budget,
            }),
            on_meta: RefCell::default(),
            opened: Cell::new(None),
            folder: Cell::new(0),
            scanning: Cell::new(false),
            scroll: Cell::new((0.0, 0.0, None)),
            rest_check: Cell::new(false),
            background_dirty: Cell::new(false),
            first_visible: Cell::new(0),
        });

        let weak = Rc::downgrade(&this);
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.apply(event);
            }
        });
        if trace::enabled() {
            this.trace_throughput();
        }
        this
    }

    /// A file or folder was moved or renamed; its cached thumbnails follow it.
    pub fn file_moved(&self, from: &std::path::Path, to: &std::path::Path) {
        let _ = self.writer.send(cache::Write::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }

    pub fn set_viewport(&self, scroller: &gtk::ScrolledWindow) {
        self.viewport.replace(Some(scroller.downgrade()));
    }

    /// Called with each item whose metadata arrives.
    pub fn connect_meta(&self, f: impl Fn(&FileItem) + 'static) {
        self.on_meta.replace(Some(Box::new(f)));
    }

    /// Forgets the previous folder's items and pending work. `rotational`
    /// says whether the new folder is on a spinning disk: one read at a time
    /// is fastest there (more just seek), while SSDs like several.
    pub fn reset(&self, rotational: Option<bool>) {
        self.items.borrow_mut().clear();
        {
            let mut queue = self.shared.lock();
            queue.wants.clear();
            queue.meta.clear();
            queue.background.clear();
            queue.source_paused = true;
            queue.source_slots = match rotational {
                Some(true) => 1,
                Some(false) => SOURCE_READERS,
                None => 2,
            };
        }
        let mut lru = self.lru.borrow_mut();
        lru.entries.clear();
        lru.total = 0;
        self.opened.set(Some(Instant::now()));
        self.scanning.set(true);
        self.background_dirty.set(false);

        let folder = self.folder.get() + 1;
        self.folder.set(folder);
        let me = self.me.clone();
        glib::timeout_add_local_once(SCAN_HEAD_START, move || {
            if let Some(this) = me.upgrade()
                && this.folder.get() == folder
            {
                this.resume_source();
            }
        });
    }

    /// The folder scan is complete: read from the drive, with prefetch.
    pub fn scan_finished(&self) {
        self.scanning.set(false);
        self.background_dirty.set(true);
        self.resume_source();
        self.queue_sync();
    }

    fn resume_source(&self) {
        let mut queue = self.shared.lock();
        if queue.source_paused {
            queue.source_paused = false;
            drop(queue);
            self.shared.source_ready.notify_all();
        }
    }

    pub fn register(&self, items: &[FileItem]) {
        let mut map = self.items.borrow_mut();
        for item in items {
            map.insert(item.id(), item.downgrade());
        }
    }

    /// The grid's order changed (sorting), so the background order is stale.
    pub fn order_changed(&self) {
        if !self.scanning.get() {
            self.background_dirty.set(true);
            self.queue_sync();
        }
    }

    /// Queues header reads for media without metadata (for sorting).
    pub fn want_metadata<'a>(&self, items: impl IntoIterator<Item = &'a FileItem>) {
        let mut queue = self.shared.lock();
        let before = queue.meta.len();
        for item in items {
            let entry = item.entry();
            if entry.kind.is_media() && item.meta().is_none() && !item.failed() {
                queue
                    .meta
                    .push_back((item.id(), item.key(), entry.kind, entry.format));
            }
        }
        if queue.meta.len() > before {
            self.shared.source_ready.notify_all();
        }
    }

    /// Drops pending header reads (the sort no longer needs them).
    pub fn cancel_metadata(&self) {
        self.shared.lock().meta.clear();
    }

    /// Thumbnail size in logical pixels changed.
    pub fn set_size(&self, size: i32) {
        let scale = self
            .viewport
            .borrow()
            .as_ref()
            .and_then(|v| v.upgrade())
            .map_or(1, |v| v.scale_factor());
        let pixels = (size * scale).max(1) as u32;
        let tier = TIERS
            .into_iter()
            .find(|&t| t >= pixels)
            .unwrap_or(TIERS[TIERS.len() - 1]);
        if self.tier.replace(tier) != tier {
            self.background_dirty.set(!self.scanning.get());
            self.queue_sync();
        }
    }

    pub fn bind(&self, tile: &Tile, list_item: &gtk::ListItem, item: &FileItem) {
        self.bound
            .borrow_mut()
            .insert(item.id(), (tile.downgrade(), list_item.downgrade()));
        self.lru.borrow_mut().touch(item.id());
        self.queue_sync();
    }

    /// The grid scrolled to `offset`.
    pub fn scrolled(&self, offset: f64) {
        let (last, direction, _) = self.scroll.get();
        let direction = if offset > last {
            1.0
        } else if offset < last {
            -1.0
        } else {
            direction
        };
        self.scroll.set((offset, direction, Some(Instant::now())));
        self.background_dirty.set(!self.scanning.get());
        self.queue_sync();
        self.check_rest();
    }

    /// Syncs again once scrolling stops, to re-center the background work.
    fn check_rest(&self) {
        if self.rest_check.replace(true) {
            return;
        }
        let me = self.me.clone();
        glib::timeout_add_local_once(Duration::from_millis(450), move || {
            let Some(this) = me.upgrade() else {
                return;
            };
            this.rest_check.set(false);
            if this.scrolling() {
                this.check_rest();
            } else {
                this.queue_sync();
            }
        });
    }

    fn scrolling(&self) -> bool {
        matches!(self.scroll.get().2, Some(at) if at.elapsed() < Duration::from_millis(400))
    }

    pub fn unbind(&self, item: &FileItem) {
        self.bound.borrow_mut().remove(&item.id());
        self.queue_sync();
    }

    /// Recomputes wants after scrolling, binding or resizing; once per idle.
    pub fn queue_sync(&self) {
        if self.sync_queued.replace(true) {
            return;
        }
        // Idle callbacks run after the frame's layout, so tile positions are current.
        let me = self.me.clone();
        glib::idle_add_local_once(move || {
            if let Some(this) = me.upgrade() {
                this.sync();
            }
        });
    }

    fn sync(&self) {
        self.sync_queued.set(false);
        let Some(viewport) = self.viewport.borrow().as_ref().and_then(|v| v.upgrade()) else {
            return;
        };
        let height = viewport.height() as f32;
        let limit = height * PREFETCH_SCREENS;
        let tier = self.tier.get();
        let columns = viewport
            .child()
            .and_downcast::<gtk::GridView>()
            .map_or(1, |grid| grid.max_columns().max(1));

        // Live tiles with their list positions. GTK maps only the tiles on
        // screen; the rows it keeps bound around them are unmapped, with stale
        // positions, so estimate those from the mapped rows' geometry.
        let mut tiles = Vec::new();
        for (&id, (tile, list_item)) in self.bound.borrow().iter() {
            if let (Some(tile), Some(list_item)) = (tile.upgrade(), list_item.upgrade()) {
                tiles.push((id, tile, list_item.position() / columns));
            }
        }
        let mapped: Vec<(u32, f32, f32)> = tiles
            .iter()
            .filter(|(_, tile, _)| tile.is_mapped())
            .filter_map(|(_, tile, row)| {
                let bounds = tile.compute_bounds(&viewport)?;
                Some((*row, bounds.y(), bounds.height()))
            })
            .collect();
        let anchor = mapped.iter().min_by_key(|(row, _, _)| *row).copied();
        if let Some((row, _, _)) = anchor {
            self.first_visible.set(row * columns);
        }
        let pitch = match (anchor, mapped.iter().max_by_key(|(row, _, _)| *row)) {
            (Some((r0, y0, _)), Some(&(r1, y1, _))) if r1 > r0 => (y1 - y0) / (r1 - r0) as f32,
            (Some((_, _, h)), _) => h + 12.0,
            _ => 0.0,
        };

        // While scrolling, fill the edge coming into view first; at rest
        // (and when a folder opens), fill in reading order.
        let leading = if self.scrolling() {
            self.scroll.get().1
        } else {
            0.0
        };

        let mut visible_missing = 0;
        let mut visible = 0;
        let mut wanted: Vec<(Job, u32)> = Vec::new();
        {
            let items = self.items.borrow();
            for (id, tile, row) in &tiles {
                let Some(item) = items.get(id).and_then(|i| i.upgrade()) else {
                    continue;
                };
                let (top, bottom) = if tile.is_mapped() {
                    match tile.compute_bounds(&viewport) {
                        Some(b) => (b.y(), b.y() + b.height()),
                        None => continue,
                    }
                } else if let Some((anchor_row, anchor_top, tile_height)) = anchor {
                    let top = anchor_top + (*row as f32 - anchor_row as f32) * pitch;
                    (top, top + tile_height)
                } else {
                    continue;
                };
                let on_screen = tile.is_mapped() && bottom > 0.0 && top < height;
                if on_screen {
                    visible += 1;
                }
                let entry = item.entry();
                if !entry.kind.is_media() || item.failed() {
                    continue;
                }
                if item.native() || item.texture_tier() >= tier {
                    continue;
                }
                let priority = if on_screen {
                    if leading > 0.0 {
                        (height - top).max(0.0) as u32
                    } else {
                        top.max(0.0) as u32
                    }
                } else {
                    let distance = if top >= height {
                        top - height
                    } else {
                        (-bottom).max(0.0)
                    };
                    if distance > limit || self.scanning.get() {
                        continue;
                    }
                    1_000_000 + distance as u32
                };
                if on_screen {
                    visible_missing += 1;
                }
                let job = Job {
                    id: *id,
                    serial: 0,
                    key: item.key(),
                    tier,
                    kind: entry.kind,
                    format: entry.format,
                    background: false,
                };
                wanted.push((job, priority));
            }
        }

        let mut queue = self.shared.lock();
        let wanted_ids: HashSet<u64> = wanted.iter().map(|(job, _)| job.id).collect();
        queue.wants.retain(|id, want| {
            want.stage == Stage::Busy || want.job.background || wanted_ids.contains(id)
        });
        for (mut job, priority) in wanted {
            match queue.wants.get_mut(&job.id) {
                Some(want) if want.job.tier >= tier => {
                    // Now on screen or nearby: no longer background work.
                    want.priority = priority;
                    want.job.background = false;
                }
                _ => {
                    job.serial = queue.serial();
                    let want = Want {
                        job,
                        priority,
                        stage: Stage::Cache,
                    };
                    queue.wants.insert(want.job.id, want);
                }
            }
        }
        let has_work = queue.wants.values().any(|w| w.stage != Stage::Busy);
        drop(queue);
        if self.background_dirty.get() && !self.scanning.get() && !self.scrolling() {
            self.refill_background(&viewport, tier);
        }
        if has_work {
            self.shared.cache_ready.notify_all();
            self.shared.source_ready.notify_all();
        }

        if visible > 0
            && visible_missing == 0
            && let Some(opened) = self.opened.take()
        {
            trace!(
                "viewport filled: {visible} thumbnails after {:.1} ms",
                opened.elapsed().as_secs_f64() * 1e3
            );
        }
    }

    /// Lists the folder's media that may still need caching, nearest to the
    /// view first, for the workers to go through when nothing else waits.
    fn refill_background(&self, viewport: &gtk::ScrolledWindow, tier: u32) {
        self.background_dirty.set(false);
        let Some(model) = viewport
            .child()
            .and_downcast::<gtk::GridView>()
            .and_then(|grid| grid.model())
        else {
            return;
        };
        let center = self.first_visible.get();
        let mut pending: Vec<(u32, Pending)> = Vec::new();
        for position in 0..model.n_items() {
            let Some(item) = model.item(position).and_downcast::<FileItem>() else {
                continue;
            };
            let entry = item.entry();
            if !entry.kind.is_media()
                || item.failed()
                || item.native()
                || item.texture_tier() >= tier
            {
                continue;
            }
            // Onward from the view first, then back up: one direction at a
            // time keeps a spinning disk from seeking between two regions.
            let order = if position >= center {
                position - center
            } else {
                u32::MAX / 2 + (center - position)
            };
            pending.push((order, (item.id(), item.key(), entry.kind, entry.format)));
        }
        pending.sort_unstable_by_key(|(order, _)| *order);
        let mut queue = self.shared.lock();
        queue.background = pending.into_iter().map(|(_, p)| p).collect();
        queue.background_tier = tier;
        drop(queue);
        self.shared.cache_ready.notify_all();
    }

    fn apply(&self, event: Event) {
        match event {
            Event::Thumb {
                id,
                tier,
                pixels,
                native,
                meta,
                background,
            } => {
                let Some(item) = self.item(id) else {
                    return;
                };
                if let Some(meta) = meta {
                    self.set_meta(&item, meta);
                }
                // Background results are cached on disk; only keep the
                // texture if the item is on screen now.
                if tier < item.texture_tier()
                    || (background && !self.bound.borrow().contains_key(&id))
                {
                    return;
                }
                let (width, height) = pixels.dimensions();
                let (format, channels) = if pixels.has_alpha() {
                    (gdk::MemoryFormat::R8g8b8a8, 4)
                } else {
                    (gdk::MemoryFormat::R8g8b8, 3)
                };
                let bytes = glib::Bytes::from_owned(pixels.into_raw());
                let size = bytes.len();
                let texture = gdk::MemoryTexture::new(
                    width as i32,
                    height as i32,
                    format,
                    &bytes,
                    width as usize * channels,
                );
                item.set_thumbnail(Some(texture.upcast()), tier, native);
                self.track(id, size);
            }
            Event::Failed { id, meta } => {
                if let Some(item) = self.item(id) {
                    if let Some(meta) = meta {
                        self.set_meta(&item, meta);
                    }
                    item.set_failed(true);
                }
            }
            Event::Meta { id, meta } => {
                if let Some(item) = self.item(id) {
                    self.set_meta(&item, meta);
                }
            }
        }
        self.queue_sync();
    }

    fn item(&self, id: u64) -> Option<FileItem> {
        self.items.borrow().get(&id).and_then(|i| i.upgrade())
    }

    fn set_meta(&self, item: &FileItem, meta: Meta) {
        if item.meta() == Some(meta) {
            return;
        }
        item.set_meta(meta);
        if let Some(f) = self.on_meta.borrow().as_ref() {
            f(item);
        }
    }

    fn track(&self, id: u64, bytes: usize) {
        let mut lru = self.lru.borrow_mut();
        lru.insert(id, bytes);
        let keep: HashSet<u64> = self.bound.borrow().keys().copied().collect();
        for evicted in lru.evict(&keep) {
            if let Some(item) = self.item(evicted) {
                item.set_thumbnail(None, 0, false);
            }
        }
    }

    fn trace_throughput(&self) {
        let shared = self.shared.clone();
        let mut last = [0u64; 6];
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let s = &shared.stats;
            let now = [
                s.cache_hits.load(Ordering::Relaxed),
                s.generated.load(Ordering::Relaxed),
                s.embedded.load(Ordering::Relaxed),
                s.failed.load(Ordering::Relaxed),
                s.meta.load(Ordering::Relaxed),
                s.videos.load(Ordering::Relaxed),
            ];
            if now != last {
                let made = now[1] + now[2] + now[5];
                trace!(
                    "thumbs/s: {} cached, {} decoded, {} from EXIF, {} videos, {} failed, {} headers | avg read {:.1} ms, decode {:.1} ms",
                    now[0] - last[0],
                    now[1] - last[1],
                    now[2] - last[2],
                    now[5] - last[5],
                    now[3] - last[3],
                    now[4] - last[4],
                    s.read_us.load(Ordering::Relaxed) as f64 / made.max(1) as f64 / 1e3,
                    s.decode_us.load(Ordering::Relaxed) as f64 / now[1].max(1) as f64 / 1e3,
                );
                last = now;
            }
            glib::ControlFlow::Continue
        });
    }
}
