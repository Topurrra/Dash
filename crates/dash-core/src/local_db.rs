use redb::{Database, TableDefinition};
use serde::{de::DeserializeOwned, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
#[cfg(windows)]
use crate::dpapi;

pub const LOCAL_DB_FILE: &str = "store.redb";

const JSON_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("json");

/// Process-wide cache of opened redb handles, keyed by database path.
///
/// redb forbids two `Database` instances open on the same file at once (it
/// locks the file), so the old code re-opened the database on EVERY call and
/// serialized those opens behind a single global mutex. Worse, `open_db` ran
/// a fresh `begin_write` + `commit` — an fsync — on every call (even reads)
/// just to ensure the table existed. Under a busy disk / AV scan that fsync
/// could stall for hundreds of ms while the global lock was held, parking
/// every window's pending IPC together (the intermittent both-windows hang).
///
/// We now open each database ONCE, cache the handle, and let redb's own MVCC
/// do the concurrency: many concurrent readers + a single serialized writer,
/// with readers seeing a consistent snapshot even mid-write. The mutex below
/// guards only the cache map (a fast HashMap lookup, plus the one-time open
/// per path); it is never held across a database read/write transaction, so a
/// reader no longer blocks behind an unrelated write's fsync.
static DB_CACHE: LazyLock<Mutex<HashMap<PathBuf, Arc<Database>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Set to `true` by `quarantine_corrupt_db` when it has to move the
/// corrupt database aside and create a fresh one. The flag is
/// intentionally a `swap`-based latch — once the frontend has read it
/// via `take_db_corruption_recovered()` it resets to `false` so
/// subsequent calls return `false` (i.e., the banner shows once).
static DB_CORRUPTION_RECOVERED: AtomicBool = AtomicBool::new(false);

/// Consume and return the "DB was corrupted and auto-recovered" flag.
///
/// Returns `true` once (the first call after a corruption event) then
/// `false` for all subsequent calls until the next corruption event.
/// Called by the `take_db_corruption_notice` Tauri command in `lib.rs`
/// so the frontend can show a one-time dismissable warning banner.
pub fn take_db_corruption_recovered() -> bool {
    DB_CORRUPTION_RECOVERED.swap(false, Ordering::Relaxed)
}

/// Wrap a JSON payload with DPAPI on Windows. On non-Windows builds we
/// fall through to plaintext — the cfg-gating mirrors the dpapi module's
/// own `#![cfg(windows)]`. Centralizing the gate here means every call
/// site reads as if encryption always happens, which is correct for the
/// only platform we ship on today.
fn encrypt_for_db(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        dpapi::protect(plaintext)
    }
    #[cfg(not(windows))]
    {
        Ok(plaintext.to_vec())
    }
}

/// Reverse of `encrypt_for_db`. On Windows the dpapi module already
/// transparently handles legacy plaintext (returns input unchanged if it
/// lacks the version byte), so reads of a pre-encryption database keep
/// working — the next write will re-protect the value automatically.
fn decrypt_from_db(stored: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        dpapi::unprotect(stored)
    }
    #[cfg(not(windows))]
    {
        Ok(stored.to_vec())
    }
}

pub fn database_path_for_dir(dir: &Path) -> PathBuf {
    dir.join(LOCAL_DB_FILE)
}

fn open_db(path: &Path) -> Result<Database, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create database directory: {error}"))?;
    }

    // Graceful corruption recovery: if `Database::open` fails on an
    // existing file (truncated by a system crash, partially overwritten
    // by AV/backup software, intentionally corrupted, etc.), we move the
    // bad file aside as `store.corrupt-<timestamp>.redb` and start a
    // fresh DB. The user loses any data that lived in the corrupt file,
    // but they don't lose the app — without this, the daemon refuses to
    // launch and there's no obvious recovery path for a non-technical
    // user. We always prefer "service degraded" over "service down".
    let db = if path.exists() {
        match Database::open(path) {
            Ok(db) => db,
            // Held by another app (apps on these engines share the folder):
            // busy, not corrupt — leave it alone.
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                return Err("The local database is in use by another app".to_string());
            }
            // Only real corruption is moved aside; I/O trouble or a newer
            // file format leaves the file alone.
            Err(error) if !is_corruption(&error) => {
                return Err(format!("Cannot open local database: {error}"));
            }
            Err(error) => {
                eprintln!(
                    "local_db: cannot open existing database ({error}); quarantining and starting fresh."
                );
                quarantine_corrupt_db(path)?;
                Database::create(path).map_err(|e| {
                    format!("Cannot create fresh database after quarantine: {e}")
                })?
            }
        }
    } else {
        Database::create(path).map_err(|error| format!("Cannot create local database: {error}"))?
    };

    let write_txn = db
        .begin_write()
        .map_err(|error| format!("Cannot initialize local database: {error}"))?;
    {
        let _table = write_txn
            .open_table(JSON_TABLE)
            .map_err(|error| format!("Cannot initialize local database table: {error}"))?;
    }
    write_txn
        .commit()
        .map_err(|error| format!("Cannot commit local database initialization: {error}"))?;

    Ok(db)
}

/// Whether an open error means the file itself is damaged.
fn is_corruption(error: &redb::DatabaseError) -> bool {
    matches!(error, redb::DatabaseError::Storage(redb::StorageError::Corrupted(_)))
}

/// Return the process-wide cached handle for `path`, opening and
/// one-time-initializing the database on first access. Subsequent calls are a
/// fast cache hit — no file re-open and, crucially, no fsync. The cache mutex
/// is held only for the lookup (and the rare first open per path); the redb
/// read/write transactions in the callers below run on the returned handle
/// WITHOUT any app-level lock, so a reader never blocks behind an unrelated
/// writer's fsync.
fn get_db(path: &Path) -> Result<Arc<Database>, String> {
    let mut cache = DB_CACHE
        .lock()
        .map_err(|_| "Local database cache lock was poisoned".to_string())?;
    if let Some(db) = cache.get(path) {
        return Ok(db.clone());
    }
    let db = Arc::new(open_db(path)?);
    cache.insert(path.to_path_buf(), db.clone());
    Ok(db)
}

/// Move a corrupt redb file aside so a fresh DB can be created in its
/// place. The quarantined file gets a `.corrupt-<timestamp-ms>.redb`
/// suffix so users can recover the data manually if they really need
/// to (open it from a backup, etc.) and so we keep evidence for support
/// if the corruption was due to a bug rather than disk damage.
///
/// Sets `DB_CORRUPTION_RECOVERED` so the frontend can show a one-time
/// warning banner informing the user that their preferences were reset.
fn quarantine_corrupt_db(path: &Path) -> Result<(), String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let quarantine = path.with_extension(format!("corrupt-{timestamp}.redb"));
    fs::rename(path, quarantine)
        .map_err(|error| format!("Cannot quarantine corrupt database: {error}"))?;
    // Signal the frontend that a corruption recovery happened. The flag
    // is consumed by `take_db_corruption_notice` (lib.rs) once the main
    // window's layout polls it on mount — the banner then shows once.
    DB_CORRUPTION_RECOVERED.store(true, Ordering::Relaxed);
    Ok(())
}

pub fn read_json_or_default<T>(db_path: &Path, key: &str, fallback: T) -> Result<T, String>
where
    T: Clone + DeserializeOwned + Serialize,
{
    let db = get_db(db_path)?;
    let read_txn = db
        .begin_read()
        .map_err(|error| format!("Cannot read local database: {error}"))?;
    // The redb value may be DPAPI-protected (post-encryption installs) or
    // raw JSON (legacy installs from before encryption shipped). We always
    // route through `decrypt_from_db` so the DPAPI module's version-byte
    // check transparently handles both cases — protected blobs come back
    // as plaintext, legacy blobs pass through unchanged.
    {
        let table = read_txn
            .open_table(JSON_TABLE)
            .map_err(|error| format!("Cannot read local database table: {error}"))?;
        if let Some(value) = table
            .get(key)
            .map_err(|error| format!("Cannot read local database value: {error}"))?
        {
            // A value we can't read (e.g. written by a newer version) is
            // kept as-is: use the default without saving over it.
            let parsed = decrypt_from_db(value.value())
                .and_then(|plaintext| serde_json::from_slice::<T>(&plaintext).map_err(|e| e.to_string()));
            return match parsed {
                Ok(parsed) => Ok(parsed),
                Err(error) => {
                    eprintln!("local_db: cannot read value {key:?} ({error}); using the default without saving.");
                    Ok(fallback)
                }
            };
        }
    }
    drop(read_txn);

    // No value in the DB yet — seed it with the default. (Legacy loose-JSON
    // files are no longer read/migrated: all config has lived in this encrypted
    // redb database for a while, so there's nothing left to import.)
    let data = serde_json::to_vec_pretty(&fallback)
        .map_err(|error| format!("Cannot serialize local database value: {error}"))?;
    write_json_bytes_to_db(&db, key, &data)?;
    Ok(fallback)
}

pub fn read_json<T>(db_path: &Path, key: &str) -> Result<Option<T>, String>
where
    T: DeserializeOwned,
{
    if !db_path.exists() {
        return Ok(None);
    }

    let db = get_db(db_path)?;
    let read_txn = db
        .begin_read()
        .map_err(|error| format!("Cannot read local database: {error}"))?;
    let table = read_txn
        .open_table(JSON_TABLE)
        .map_err(|error| format!("Cannot read local database table: {error}"))?;
    let Some(value) = table
        .get(key)
        .map_err(|error| format!("Cannot read local database value: {error}"))?
    else {
        return Ok(None);
    };
    // Same DPAPI-transparent path as `read_json_or_default` — the
    // decrypt routine handles both protected blobs and legacy plaintext.
    let plaintext = decrypt_from_db(value.value())?;
    let parsed = serde_json::from_slice::<T>(&plaintext)
        .map_err(|error| format!("Cannot parse local database value: {error}"))?;
    Ok(Some(parsed))
}

pub fn write_json<T>(db_path: &Path, key: &str, value: &T) -> Result<(), String>
where
    T: Serialize,
{
    let data = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("Cannot serialize local database value: {error}"))?;
    write_json_bytes(db_path, key, &data)
}

pub fn write_json_bytes(db_path: &Path, key: &str, data: &[u8]) -> Result<(), String> {
    let db = get_db(db_path)?;
    write_json_bytes_to_db(&db, key, data)
}

/// Writes the value through DPAPI on Windows so all values land in redb
/// encrypted. Callers don't see the encryption — they pass plain JSON
/// bytes and get back a Result, identical to before.
fn write_json_bytes_to_db(db: &Database, key: &str, data: &[u8]) -> Result<(), String> {
    let encrypted = encrypt_for_db(data)?;
    let write_txn = db
        .begin_write()
        .map_err(|error| format!("Cannot write local database: {error}"))?;
    {
        let mut table = write_txn
            .open_table(JSON_TABLE)
            .map_err(|error| format!("Cannot write local database table: {error}"))?;
        table
            .insert(key, encrypted.as_slice())
            .map_err(|error| format!("Cannot write local database value: {error}"))?;
    }
    write_txn
        .commit()
        .map_err(|error| format!("Cannot commit local database value: {error}"))
}

/// Remove a single key from the local DB. Used by features that legitimately
/// drop individual values (e.g. Time Tracker retention pruning + wipe). No-op
/// when the DB file or the key is absent.
pub fn remove_json(db_path: &Path, key: &str) -> Result<(), String> {
    if !db_path.exists() {
        return Ok(());
    }
    let db = get_db(db_path)?;
    let write_txn = db
        .begin_write()
        .map_err(|error| format!("Cannot write local database: {error}"))?;
    {
        let mut table = write_txn
            .open_table(JSON_TABLE)
            .map_err(|error| format!("Cannot open local database table: {error}"))?;
        table
            .remove(key)
            .map_err(|error| format!("Cannot remove local database value: {error}"))?;
    }
    write_txn
        .commit()
        .map_err(|error| format!("Cannot commit local database removal: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_open_elsewhere_is_not_quarantined() {
        let dir = std::env::temp_dir().join(format!("dash-db-busy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = database_path_for_dir(&dir);
        let held = Database::create(&path).unwrap();

        let error = open_db(&path).err().expect("busy database must not open");
        assert!(error.contains("in use"), "{error}");
        let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "nothing moved aside: {names:?}");

        drop(held);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_corruption_is_quarantined() {
        let corrupt = redb::DatabaseError::Storage(redb::StorageError::Corrupted("bad".into()));
        assert!(is_corruption(&corrupt));
        assert!(!is_corruption(&redb::DatabaseError::UpgradeRequired(1)));
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked");
        assert!(!is_corruption(&redb::DatabaseError::Storage(redb::StorageError::Io(io))));
    }

    #[test]
    fn unreadable_value_is_not_overwritten_by_the_default() {
        let dir = std::env::temp_dir().join(format!("dash-db-default-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = database_path_for_dir(&dir);

        // Absent key: the default is seeded.
        assert_eq!(read_json_or_default(&path, "absent", vec![1u32]).unwrap(), vec![1]);
        assert_eq!(read_json::<Vec<u32>>(&path, "absent").unwrap(), Some(vec![1]));

        // Unparsable value: the default is returned, the stored bytes stay.
        write_json_bytes(&path, "newer", b"{not json for this version").unwrap();
        assert_eq!(read_json_or_default(&path, "newer", vec![2u32]).unwrap(), vec![2]);
        let db = get_db(&path).unwrap();
        let txn = db.begin_read().unwrap();
        let table = txn.open_table(JSON_TABLE).unwrap();
        let stored = decrypt_from_db(table.get("newer").unwrap().unwrap().value()).unwrap();
        assert_eq!(stored, b"{not json for this version");
    }
}
