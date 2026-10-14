//! Persistent storage for the app's files and record databases.
//!
//! The Java shell exports/imports saves by reading and writing two text files in
//! `getFilesDir()`: `wie_fs_v1` and `wie_db_v1`. Their content is opaque to Java,
//! so the only requirement is that this module can read back what it wrote.
//!
//! Format (one entry per line, all fields hex encoded, tab separated):
//!   wie_fs_v1:  `F<TAB>hex(aid)<TAB>hex(path)<TAB>hex(data)`
//!   wie_db_v1:  `D<TAB>hex(app_id)<TAB>hex(name)`                 (database exists)
//!               `R<TAB>hex(app_id)<TAB>hex(name)<TAB>id<TAB>hex(data)`  (record)

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use wie_backend::RecordId;

pub const FS_FILE: &str = "wie_fs_v1";
pub const DB_FILE: &str = "wie_db_v1";
const HEADER: &str = "WIEJNI1";
const FLUSH_DELAY: Duration = Duration::from_millis(300);

type FileKey = (String, String);
type DbKey = (String, String);

#[derive(Default)]
struct Inner {
    files: BTreeMap<FileKey, Vec<u8>>,
    dbs: BTreeMap<DbKey, BTreeMap<RecordId, Vec<u8>>>,
    fs_dirty_since: Option<Instant>,
    db_dirty_since: Option<Instant>,
}

#[derive(Clone)]
pub struct Store {
    dir: Arc<PathBuf>,
    inner: Arc<Mutex<Inner>>,
}

fn hex(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn unhex_str(text: &str) -> Option<String> {
    String::from_utf8(unhex(text)?).ok()
}

impl Store {
    pub fn open(dir: &Path) -> Self {
        let mut inner = Inner::default();

        if let Ok(text) = fs::read_to_string(dir.join(FS_FILE)) {
            for line in text.lines().skip_while(|line| *line == HEADER) {
                let parts: Vec<&str> = line.split('\t').collect();
                if let ["F", aid, path, data] = parts.as_slice()
                    && let (Some(aid), Some(path), Some(data)) = (unhex_str(aid), unhex_str(path), unhex(data))
                {
                    inner.files.insert((aid, path), data);
                }
            }
        }

        if let Ok(text) = fs::read_to_string(dir.join(DB_FILE)) {
            for line in text.lines().skip_while(|line| *line == HEADER) {
                let parts: Vec<&str> = line.split('\t').collect();
                match parts.as_slice() {
                    ["D", app, name] => {
                        if let (Some(app), Some(name)) = (unhex_str(app), unhex_str(name)) {
                            inner.dbs.entry((app, name)).or_default();
                        }
                    }
                    ["R", app, name, id, data] => {
                        if let (Some(app), Some(name), Ok(id), Some(data)) = (unhex_str(app), unhex_str(name), id.parse(), unhex(data)) {
                            inner.dbs.entry((app, name)).or_default().insert(id, data);
                        }
                    }
                    _ => {}
                }
            }
        }

        tracing::info!("[store] loaded {} files, {} databases", inner.files.len(), inner.dbs.len());

        Self {
            dir: Arc::new(dir.to_owned()),
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn write_atomic(&self, name: &str, text: &str) {
        let target = self.dir.join(name);
        let temp = self.dir.join(format!("{name}.tmp"));
        let result = fs::write(&temp, text).and_then(|_| fs::rename(&temp, &target));
        if let Err(error) = result {
            tracing::error!("[store] failed to write {name}: {error}");
        }
    }

    /// Writes pending changes once they have settled for a moment, or immediately when `force`.
    pub fn flush(&self, force: bool) {
        let (fs_text, db_text) = {
            let mut inner = self.lock();
            let due = |since: Option<Instant>| since.is_some_and(|since| force || since.elapsed() >= FLUSH_DELAY);

            let fs_text = due(inner.fs_dirty_since).then(|| {
                let mut text = String::from(HEADER);
                for ((aid, path), data) in &inner.files {
                    text.push_str(&format!("\nF\t{}\t{}\t{}", hex(aid.as_bytes()), hex(path.as_bytes()), hex(data)));
                }
                text.push('\n');
                text
            });
            let db_text = due(inner.db_dirty_since).then(|| {
                let mut text = String::from(HEADER);
                for ((app, name), records) in &inner.dbs {
                    let (app, name) = (hex(app.as_bytes()), hex(name.as_bytes()));
                    text.push_str(&format!("\nD\t{app}\t{name}"));
                    for (id, data) in records {
                        text.push_str(&format!("\nR\t{app}\t{name}\t{id}\t{}", hex(data)));
                    }
                }
                text.push('\n');
                text
            });

            if fs_text.is_some() {
                inner.fs_dirty_since = None;
            }
            if db_text.is_some() {
                inner.db_dirty_since = None;
            }
            (fs_text, db_text)
        };

        if let Some(text) = fs_text {
            self.write_atomic(FS_FILE, &text);
        }
        if let Some(text) = db_text {
            self.write_atomic(DB_FILE, &text);
        }
    }

    fn mark_fs(inner: &mut Inner) {
        inner.fs_dirty_since.get_or_insert_with(Instant::now);
    }

    fn mark_db(inner: &mut Inner) {
        inner.db_dirty_since.get_or_insert_with(Instant::now);
    }
}

fn normalize(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|part| !part.is_empty() && *part != ".").collect();
    if parts.is_empty() || parts.contains(&"..") {
        return None;
    }
    Some(parts.join("/"))
}

#[async_trait::async_trait]
impl wie_backend::Filesystem for Store {
    async fn exists(&self, aid: &str, path: &str) -> bool {
        let Some(path) = normalize(path) else { return false };
        self.lock().files.contains_key(&(aid.to_owned(), path))
    }

    async fn size(&self, aid: &str, path: &str) -> Option<usize> {
        let path = normalize(path)?;
        self.lock().files.get(&(aid.to_owned(), path)).map(Vec::len)
    }

    async fn read(&self, aid: &str, path: &str, offset: usize, count: usize, buf: &mut [u8]) -> Option<usize> {
        let path = normalize(path)?;
        let inner = self.lock();
        let data = inner.files.get(&(aid.to_owned(), path))?;
        if offset >= data.len() {
            return Some(0);
        }
        let n = count.min(data.len() - offset).min(buf.len());
        buf[..n].copy_from_slice(&data[offset..offset + n]);
        Some(n)
    }

    async fn write(&self, aid: &str, path: &str, offset: usize, data: &[u8]) -> usize {
        let Some(path) = normalize(path) else {
            tracing::warn!(aid, path, "[store] rejected write path");
            return 0;
        };
        let mut inner = self.lock();
        let file = inner.files.entry((aid.to_owned(), path)).or_default();
        if file.len() < offset + data.len() {
            file.resize(offset + data.len(), 0);
        }
        file[offset..offset + data.len()].copy_from_slice(data);
        Self::mark_fs(&mut inner);
        data.len()
    }

    async fn truncate(&self, aid: &str, path: &str, len: usize) {
        let Some(path) = normalize(path) else {
            tracing::warn!(aid, path, "[store] rejected truncate path");
            return;
        };
        let mut inner = self.lock();
        inner.files.entry((aid.to_owned(), path)).or_default().resize(len, 0);
        Self::mark_fs(&mut inner);
    }
}

#[async_trait::async_trait]
impl wie_backend::DatabaseRepository for Store {
    async fn open(&self, name: &str, app_id: &str) -> Box<dyn wie_backend::Database> {
        let key = (app_id.to_owned(), name.to_owned());
        {
            let mut inner = self.lock();
            if !inner.dbs.contains_key(&key) {
                inner.dbs.insert(key.clone(), BTreeMap::new());
                Self::mark_db(&mut inner);
            }
        }
        Box::new(Db { store: self.clone(), key })
    }

    async fn exists(&self, name: &str, app_id: &str) -> bool {
        self.lock().dbs.contains_key(&(app_id.to_owned(), name.to_owned()))
    }

    async fn delete(&self, name: &str, app_id: &str) -> bool {
        let mut inner = self.lock();
        let removed = inner.dbs.remove(&(app_id.to_owned(), name.to_owned())).is_some();
        if removed {
            Self::mark_db(&mut inner);
        }
        removed
    }

    async fn usage(&self, app_id: &str) -> u64 {
        self.lock()
            .dbs
            .iter()
            .filter(|((app, _), _)| app == app_id)
            .flat_map(|(_, records)| records.values())
            .map(|data| data.len() as u64)
            .sum()
    }
}

struct Db {
    store: Store,
    key: DbKey,
}

impl Db {
    fn with<R>(&self, f: impl FnOnce(&mut BTreeMap<RecordId, Vec<u8>>, &mut bool) -> R) -> R {
        let mut inner = self.store.lock();
        let mut changed = false;
        let result = f(inner.dbs.entry(self.key.clone()).or_default(), &mut changed);
        if changed {
            Store::mark_db(&mut inner);
        }
        result
    }
}

#[async_trait::async_trait]
impl wie_backend::Database for Db {
    async fn next_id(&self) -> RecordId {
        // MIDP requires the first record id to be 1.
        self.with(|records, _| records.keys().next_back().map_or(1, |id| id + 1))
    }

    async fn add(&mut self, data: &[u8]) -> RecordId {
        self.with(|records, changed| {
            let id = records.keys().next_back().map_or(1, |id| id + 1);
            records.insert(id, data.to_vec());
            *changed = true;
            id
        })
    }

    async fn get(&self, id: RecordId) -> Option<Vec<u8>> {
        self.with(|records, _| records.get(&id).cloned())
    }

    async fn set(&mut self, id: RecordId, data: &[u8]) -> bool {
        self.with(|records, changed| {
            records.insert(id, data.to_vec());
            *changed = true;
            true
        })
    }

    async fn delete(&mut self, id: RecordId) -> bool {
        self.with(|records, changed| {
            let removed = records.remove(&id).is_some();
            *changed |= removed;
            removed
        })
    }

    async fn get_record_ids(&self) -> Vec<RecordId> {
        self.with(|records, _| records.keys().copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wie_backend::{Database as _, DatabaseRepository as _, Filesystem as _};

    fn block<F: std::future::Future>(future: F) -> F::Output {
        // The store never awaits anything, so a single poll completes every call.
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("store future did not complete"),
        }
    }

    #[test]
    fn round_trips_files_and_records() {
        let dir = std::env::temp_dir().join(format!("wie_jni_store_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let store = Store::open(&dir);
        assert_eq!(block(store.write("app", "/save/a.dat", 2, b"xy")), 2);
        let mut db = block(store.open("rec", "app"));
        assert_eq!(block(db.add(b"one")), 1);
        assert_eq!(block(db.add(b"two")), 2);
        store.flush(true);

        let reopened = Store::open(&dir);
        let mut buf = [0u8; 8];
        assert_eq!(block(reopened.read("app", "save/a.dat", 0, 8, &mut buf)), Some(4));
        assert_eq!(&buf[..4], b"\0\0xy");
        let db = block(reopened.open("rec", "app"));
        assert_eq!(block(db.get_record_ids()), vec![1, 2]);
        assert_eq!(block(db.get(2)).as_deref(), Some(&b"two"[..]));
        assert!(block(reopened.exists("rec", "app")));

        let _ = fs::remove_dir_all(&dir);
    }
}
