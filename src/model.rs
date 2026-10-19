//! Grid items and their sort order.

use std::cell::Cell;
use std::cmp::Ordering;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};
use serde::{Deserialize, Serialize};

use crate::cache::Key;
use crate::media::{Kind, Meta};
use crate::natsort;
use crate::scan::Entry;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    Name,
    #[default]
    Modified,
    Taken,
    Format,
    Dimensions,
    Size,
}

impl SortKey {
    pub const ALL: [SortKey; 6] = [
        Self::Name,
        Self::Modified,
        Self::Taken,
        Self::Format,
        Self::Dimensions,
        Self::Size,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Modified => "modified",
            Self::Taken => "taken",
            Self::Format => "format",
            Self::Dimensions => "dimensions",
            Self::Size => "size",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Modified => "Date Modified",
            Self::Taken => "Date Taken",
            Self::Format => "Format",
            Self::Dimensions => "Dimensions",
            Self::Size => "Size",
        }
    }

    /// Whether sorting needs metadata read from file contents.
    pub fn needs_meta(self) -> bool {
        matches!(self, Self::Taken | Self::Dimensions)
    }
}

/// Orders by `key`, then by natural name order so equal keys stay stable.
pub fn compare(a: &FileItem, b: &FileItem, key: SortKey) -> Ordering {
    let (ea, eb) = (a.entry(), b.entry());
    let primary = match key {
        SortKey::Name => Ordering::Equal,
        SortKey::Modified => ea.mtime.cmp(&eb.mtime),
        SortKey::Taken => a.date_taken().cmp(&b.date_taken()),
        SortKey::Format => a.format().cmp(b.format()),
        SortKey::Dimensions => a.pixel_count().cmp(&b.pixel_count()),
        SortKey::Size => ea.size.cmp(&eb.size),
    };
    primary
        .then_with(|| natsort::compare(&ea.key, &eb.key))
        .then_with(|| ea.name.cmp(&eb.name))
}

/// A sorter reading the shared (key, ascending) state and the shuffle seed
/// (`Some` while shuffled); call `changed` after updating either.
/// Ascending puts the largest first (newest, Z to A, biggest): the user's
/// reading of it (2026-10-19), and the default.
pub fn sorter(
    state: Rc<Cell<(SortKey, bool)>>,
    shuffle: Rc<Cell<Option<u64>>>,
) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| {
        let (key, ascending) = state.get();
        let (Some(a), Some(b)) = (a.downcast_ref::<FileItem>(), b.downcast_ref::<FileItem>())
        else {
            return gtk::Ordering::Equal;
        };
        // Folders come first in either direction, like a file manager.
        let folder = |item: &FileItem| item.entry().kind == Kind::Folder;
        let folders_first = folder(b).cmp(&folder(a));
        if folders_first != Ordering::Equal {
            return folders_first.into();
        }
        if let Some(seed) = shuffle.get() {
            return shuffled(a.id(), b.id(), seed).into();
        }
        let ord = compare(a, b, key);
        if ascending { ord.reverse() } else { ord }.into()
    })
}

/// Which files the grid shows; folders always show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Show {
    #[default]
    All,
    Media,
    Pictures,
    Videos,
}

impl Show {
    pub const ALL: [Show; 4] = [Self::All, Self::Media, Self::Pictures, Self::Videos];

    pub fn id(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Media => "media",
            Self::Pictures => "pictures",
            Self::Videos => "videos",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All Files",
            Self::Media => "Pictures and Videos",
            Self::Pictures => "Pictures",
            Self::Videos => "Videos",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::All => "view-grid-symbolic",
            Self::Media => "camera-photo-symbolic",
            Self::Pictures => "image-x-generic-symbolic",
            Self::Videos => "video-x-generic-symbolic",
        }
    }

    pub fn includes(self, kind: Kind) -> bool {
        match (self, kind) {
            (Self::All, _) | (_, Kind::Folder) => true,
            (Self::Media, kind) => kind.is_media(),
            (Self::Pictures, kind) => kind == Kind::Image,
            (Self::Videos, kind) => kind == Kind::Video,
        }
    }
}

/// A filter reading the shared `Show` state; call `changed` after updating it.
pub fn filter(show: Rc<Cell<Show>>) -> gtk::CustomFilter {
    gtk::CustomFilter::new(move |item| {
        item.downcast_ref::<FileItem>()
            .is_some_and(|item| show.get().includes(item.entry().kind))
    })
}

/// A random but fixed order for `seed`, from each item's id, so items added
/// while shuffled land in random places too.
fn shuffled(a: u64, b: u64, seed: u64) -> Ordering {
    mix(a ^ seed).cmp(&mix(b ^ seed)).then(a.cmp(&b))
}

/// SplitMix64's finalizer: nearby ids get unrelated ranks.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

mod imp {
    use std::cell::{OnceCell, RefCell};

    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::FileItem)]
    pub struct FileItem {
        /// The thumbnail, once loaded (and while it stays in the texture budget).
        #[property(get, nullable)]
        texture: RefCell<Option<gdk::Texture>>,
        /// The texture is the whole image at 100%, so it must not be upscaled.
        #[property(get)]
        native: Cell<bool>,
        /// The file couldn't be decoded.
        #[property(get, set)]
        pub failed: Cell<bool>,
        pub texture_tier: Cell<u32>,
        pub meta: Cell<Option<Meta>>,
        pub entry: OnceCell<Entry>,
        pub id: Cell<u64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FileItem {
        const NAME: &'static str = "FvFileItem";
        type Type = super::FileItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for FileItem {}

    impl FileItem {
        pub fn set_thumbnail(&self, texture: Option<gdk::Texture>, tier: u32, native: bool) {
            self.texture_tier.set(tier);
            self.native.set(native);
            self.texture.replace(texture);
            let obj = self.obj();
            obj.notify_native();
            obj.notify_texture();
        }
    }
}

glib::wrapper! {
    pub struct FileItem(ObjectSubclass<imp::FileItem>);
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl FileItem {
    pub fn new(mut entry: Entry) -> Self {
        let item: Self = glib::Object::new();
        let imp = item.imp();
        imp.id.set(NEXT_ID.fetch_add(1, AtomicOrdering::Relaxed));
        imp.meta.set(entry.meta.take());
        imp.failed.set(entry.failed);
        let _ = imp.entry.set(entry);
        item
    }

    /// Unique for the life of the process.
    pub fn id(&self) -> u64 {
        self.imp().id.get()
    }

    pub fn entry(&self) -> &Entry {
        self.imp()
            .entry
            .get()
            .expect("FileItem created without an entry")
    }

    pub fn key(&self) -> Key {
        let entry = self.entry();
        Key {
            path: entry.path.clone(),
            mtime: entry.mtime,
            size: entry.size,
        }
    }

    pub fn meta(&self) -> Option<Meta> {
        self.imp().meta.get()
    }

    pub fn set_meta(&self, meta: Meta) {
        self.imp().meta.set(Some(meta));
    }

    pub fn texture_tier(&self) -> u32 {
        self.imp().texture_tier.get()
    }

    pub fn set_thumbnail(&self, texture: Option<gdk::Texture>, tier: u32, native: bool) {
        self.imp().set_thumbnail(texture, tier, native);
    }

    /// The format from the file's contents once known, else from its extension.
    pub fn format(&self) -> &'static str {
        self.meta().map_or(self.entry().format, |m| m.format)
    }

    /// EXIF date taken, falling back to the modification time.
    pub fn date_taken(&self) -> i64 {
        self.meta()
            .and_then(|m| m.taken)
            .unwrap_or(self.entry().mtime)
    }

    /// Width x height, or 0 while unknown.
    pub fn pixel_count(&self) -> u64 {
        self.meta()
            .map_or(0, |m| u64::from(m.width) * u64::from(m.height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffle_orders_randomly() {
        let order = |seed| {
            let mut ids: Vec<u64> = (1..=200).collect();
            ids.sort_by(|&a, &b| shuffled(a, b, seed));
            ids
        };
        let (one, two) = (order(1), order(2));
        assert_ne!(one, (1..=200).collect::<Vec<_>>());
        assert_ne!(one, two);
        assert_eq!(one, order(1));
    }

    #[test]
    fn show_keeps_folders() {
        for show in Show::ALL {
            assert!(show.includes(Kind::Folder));
            assert_eq!(Show::from_id(show.id()), Some(show));
        }
        assert!(Show::All.includes(Kind::Other));
        assert!(!Show::Media.includes(Kind::Other));
        assert!(Show::Media.includes(Kind::Video));
        assert!(!Show::Pictures.includes(Kind::Video));
        assert!(!Show::Videos.includes(Kind::Image));
    }
}
