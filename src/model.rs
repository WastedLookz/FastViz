//! Grid items and their sort order.

use std::cell::Cell;
use std::cmp::Ordering;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use serde::{Deserialize, Serialize};

use crate::natsort;
use crate::scan::Entry;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    Name,
    #[default]
    Modified,
    Format,
    Size,
}

impl SortKey {
    pub const ALL: [SortKey; 4] = [Self::Name, Self::Modified, Self::Format, Self::Size];

    pub fn id(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Modified => "modified",
            Self::Format => "format",
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
            Self::Format => "Format",
            Self::Size => "Size",
        }
    }
}

/// Orders by `key`, then by natural name order so equal keys stay stable.
pub fn compare(a: &Entry, b: &Entry, key: SortKey) -> Ordering {
    let primary = match key {
        SortKey::Name => Ordering::Equal,
        SortKey::Modified => a.mtime.cmp(&b.mtime),
        SortKey::Format => a.format.cmp(b.format),
        SortKey::Size => a.size.cmp(&b.size),
    };
    primary
        .then_with(|| natsort::compare(&a.key, &b.key))
        .then_with(|| a.name.cmp(&b.name))
}

/// A sorter reading the shared (key, descending) state; call `changed` after updating it.
pub fn sorter(state: Rc<Cell<(SortKey, bool)>>) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| {
        let (key, descending) = state.get();
        let (Some(a), Some(b)) = (a.downcast_ref::<FileItem>(), b.downcast_ref::<FileItem>())
        else {
            return gtk::Ordering::Equal;
        };
        let ord = compare(a.entry(), b.entry(), key);
        if descending { ord.reverse() } else { ord }.into()
    })
}

mod imp {
    use std::cell::OnceCell;

    use super::*;

    #[derive(Default)]
    pub struct FileItem {
        pub entry: OnceCell<Entry>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FileItem {
        const NAME: &'static str = "FvFileItem";
        type Type = super::FileItem;
    }

    impl ObjectImpl for FileItem {}
}

glib::wrapper! {
    pub struct FileItem(ObjectSubclass<imp::FileItem>);
}

impl FileItem {
    pub fn new(entry: Entry) -> Self {
        let item: Self = glib::Object::new();
        let _ = item.imp().entry.set(entry);
        item
    }

    pub fn entry(&self) -> &Entry {
        self.imp()
            .entry
            .get()
            .expect("FileItem created without an entry")
    }
}
