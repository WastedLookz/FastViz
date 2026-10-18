//! Persistent thumbnail and metadata cache: `~/.cache/photoviewer/cache.sqlite`.
//!
//! Rows are keyed by absolute path and are only valid while the file's mtime
//! and size still match. Each thread opens its own connection (WAL mode);
//! a single writer thread batches inserts into transactions.

use std::collections::HashMap;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gtk::glib;
use rusqlite::{Connection, OptionalExtension, params};

use crate::decode::{DECODER_VERSION, Encoding};
use crate::media::{self, Meta};

/// Bump to discard all cached data after a format change.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS thumb (
        path BLOB NOT NULL,
        tier INTEGER NOT NULL,
        mtime INTEGER NOT NULL,
        size INTEGER NOT NULL,
        encoding INTEGER NOT NULL,
        native INTEGER NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (path, tier)
    );
    CREATE TABLE IF NOT EXISTS meta (
        path BLOB PRIMARY KEY,
        mtime INTEGER NOT NULL,
        size INTEGER NOT NULL,
        width INTEGER,
        height INTEGER,
        taken INTEGER,
        format TEXT,
        -- DECODER_VERSION that failed to decode this file, or 0.
        failed INTEGER NOT NULL DEFAULT 0
    );
";

/// Identifies one version of a file.
#[derive(Clone, Debug)]
pub struct Key {
    pub path: PathBuf,
    pub mtime: i64,
    pub size: u64,
}

impl Key {
    fn path_bytes(&self) -> &[u8] {
        self.path.as_os_str().as_bytes()
    }
}

pub fn path() -> PathBuf {
    glib::user_cache_dir()
        .join("photoviewer")
        .join("cache.sqlite")
}

pub fn open() -> rusqlite::Result<Connection> {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut conn = Connection::open(&path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // Several threads open at startup; settle the schema under a write lock.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        tx.execute_batch("DROP TABLE IF EXISTS thumb; DROP TABLE IF EXISTS meta;")?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(conn)
}

pub struct CachedThumb {
    pub tier: u32,
    pub encoding: Encoding,
    pub native: bool,
    pub data: Vec<u8>,
}

/// The smallest cached tier at or above `tier`, else the largest one below it.
pub fn thumb(conn: &Connection, key: &Key, tier: u32) -> rusqlite::Result<Option<CachedThumb>> {
    let mut stmt = conn.prepare_cached(
        "SELECT tier, encoding, native, data FROM thumb
         WHERE path = ?1 AND mtime = ?2 AND size = ?3
         ORDER BY tier < ?4, CASE WHEN tier >= ?4 THEN tier ELSE -tier END
         LIMIT 1",
    )?;
    stmt.query_row(
        params![key.path_bytes(), key.mtime, key.size as i64, tier],
        |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        },
    )
    .optional()
    .map(|row| {
        row.and_then(|(tier, encoding, native, data)| {
            Some(CachedThumb {
                tier,
                encoding: Encoding::from_code(encoding)?,
                native,
                data,
            })
        })
    })
}

/// What the cache knows about a file without opening it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Known {
    pub meta: Option<Meta>,
    /// The current decoder already failed on this version of the file.
    pub failed: bool,
}

/// Cached facts for the direct children of `dir`, by file name, with the
/// (mtime, size) they are valid for.
pub fn folder(
    conn: &Connection,
    dir: &Path,
) -> rusqlite::Result<HashMap<PathBuf, (i64, u64, Known)>> {
    let mut low = dir.as_os_str().as_bytes().to_vec();
    if !low.ends_with(b"/") {
        low.push(b'/');
    }
    // '0' sorts right after '/', so [dir/, dir0) is exactly the paths under dir.
    let mut high = low.clone();
    *high.last_mut().expect("non-empty prefix") = b'0';

    let mut stmt = conn.prepare_cached(
        "SELECT path, mtime, size, width, height, taken, format, failed FROM meta
         WHERE path >= ?1 AND path < ?2",
    )?;
    let rows = stmt.query_map(params![low, high], |row| {
        let path: Vec<u8> = row.get(0)?;
        let width: Option<u32> = row.get(3)?;
        let height: Option<u32> = row.get(4)?;
        let format: Option<String> = row.get(6)?;
        let meta = match (width, height, format) {
            (Some(width), Some(height), Some(format)) => Some(Meta {
                width,
                height,
                taken: row.get(5)?,
                format: media::format_label(&format),
            }),
            _ => None,
        };
        let failed = row.get::<_, u32>(7)? == DECODER_VERSION;
        Ok((
            path,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            Known { meta, failed },
        ))
    })?;

    let mut out = HashMap::new();
    for row in rows {
        let (path, mtime, size, known) = row?;
        if path[low.len()..].contains(&b'/') {
            continue;
        }
        out.insert(
            PathBuf::from(std::ffi::OsString::from_vec(path)),
            (mtime, size as u64, known),
        );
    }
    Ok(out)
}

pub enum Write {
    Thumb {
        key: Key,
        tier: u32,
        encoding: Encoding,
        native: bool,
        data: Vec<u8>,
    },
    Meta {
        key: Key,
        meta: Option<Meta>,
        failed: bool,
    },
}

/// Starts the single writer thread; send it rows to store.
pub fn spawn_writer() -> mpsc::Sender<Write> {
    let (tx, rx) = mpsc::channel::<Write>();
    std::thread::Builder::new()
        .name("cache-writer".into())
        .spawn(move || {
            let mut conn = match open() {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("Thumbnail cache disabled: {e}");
                    // Keep draining so senders never notice.
                    for _ in rx {}
                    return;
                }
            };
            while let Ok(first) = rx.recv() {
                // Batch whatever arrives within a short window into one transaction.
                let mut batch = vec![first];
                let deadline = Instant::now() + Duration::from_millis(50);
                while batch.len() < 256 {
                    let left = deadline.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(left) {
                        Ok(write) => batch.push(write),
                        Err(_) => break,
                    }
                }
                if let Err(e) = store(&mut conn, batch) {
                    eprintln!("Thumbnail cache write failed: {e}");
                }
            }
        })
        .expect("failed to spawn cache writer");
    tx
}

fn store(conn: &mut Connection, batch: Vec<Write>) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for write in batch {
        match write {
            Write::Thumb {
                key,
                tier,
                encoding,
                native,
                data,
            } => {
                // Tiers of an older version of the file are now garbage.
                tx.prepare_cached(
                    "DELETE FROM thumb WHERE path = ?1 AND (mtime != ?2 OR size != ?3)",
                )?
                .execute(params![key.path_bytes(), key.mtime, key.size as i64])?;
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO thumb (path, tier, mtime, size, encoding, native, data)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?
                .execute(params![
                    key.path_bytes(),
                    tier,
                    key.mtime,
                    key.size as i64,
                    encoding as i64,
                    native,
                    data
                ])?;
            }
            Write::Meta { key, meta, failed } => {
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO meta (path, mtime, size, width, height, taken, format, failed)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?
                .execute(params![
                    key.path_bytes(),
                    key.mtime,
                    key.size as i64,
                    meta.map(|m| m.width),
                    meta.map(|m| m.height),
                    meta.and_then(|m| m.taken),
                    meta.map(|m| m.format),
                    if failed { DECODER_VERSION } else { 0 },
                ])?;
            }
        }
    }
    tx.commit()
}
