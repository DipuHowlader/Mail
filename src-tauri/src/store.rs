//! JSON config store for the launcher and disposable app instances.
//!
//! The launcher keeps license and active-instance metadata in the shared legacy
//! store. Each app process uses its own store below `instances/<id>`, which is
//! removed when that process closes. App instances cannot overwrite each
//! other's campaigns, accounts or session data.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The config directory, mirroring desktop runtime's `app.getPath('appData')` folder name.
///
/// Windows: `%APPDATA%\Novamail Command` (kept even though the product is called
/// "Novamail" so existing licenses and campaigns stay visible).
pub fn store_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NOVAMAIL_STORE_DIR") {
        return PathBuf::from(dir);
    }
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(if cfg!(windows) {
        "Novamail Command"
    } else {
        "Novamail"
    })
}

pub struct Store {
    path: PathBuf,
    data: Map<String, Value>,
}

impl Store {
    /// Open the shared store at the default location.
    pub fn open() -> Self {
        Self::open_at(store_dir())
    }

    /// Open (or create) a store rooted at a specific directory. Used by tests.
    pub fn open_at(dir: impl AsRef<Path>) -> Self {
        let path = dir.as_ref().join("novamail-command.json");
        let data = read_data(&path).unwrap_or_default();
        Self { path, data }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    /// Refresh after another app process has committed a store update. The
    /// writer uses an atomic rename, so this read always sees a complete file.
    pub fn refresh(&mut self) -> Result<(), String> {
        self.data = read_data(&self.path)?;
        Ok(())
    }

    /// Read-modify-write transaction shared by every app process. The OS file
    /// lock is released automatically if the process exits or crashes.
    pub fn transact<T>(
        &mut self,
        update: impl FnOnce(&mut Map<String, Value>) -> Result<T, String>,
    ) -> Result<T, String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let lock_path = self.path.with_extension("json.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|error| format!("Could not open the config lock: {error}"))?;
        lock_file
            .lock()
            .map_err(|error| format!("Could not lock the config store: {error}"))?;

        let mut latest = read_data(&self.path)?;
        let result = update(&mut latest)?;
        write_data(&self.path, &latest)?;
        self.data = latest;
        Ok(result)
    }

    /// Set a top-level key and persist immediately.
    pub fn set(&mut self, key: &str, value: Value) -> Result<(), String> {
        self.transact(|data| {
            data.insert(key.to_string(), value);
            Ok(())
        })
    }

    /// Remove a top-level key and persist immediately.
    #[allow(dead_code)] // Part of the store API; used by later phases.
    pub fn remove(&mut self, key: &str) -> Result<(), String> {
        self.transact(|data| {
            data.remove(key);
            Ok(())
        })
    }

    /// Erase a disposable app-instance store during shutdown.
    pub fn clear(&mut self) -> Result<(), String> {
        self.data.clear();
        for path in [self.path.clone(), self.path.with_extension("json.lock")] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(())
    }

    /// Remove app content left by the pre-isolation shared store while keeping
    /// launcher-owned data such as the accepted license.
    pub fn clear_legacy_app_data(&mut self) -> Result<(), String> {
        let initial = read_data(&self.path)?;
        if !has_legacy_app_data(&initial) {
            self.data = initial;
            return Ok(());
        }

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let lock_path = self.path.with_extension("json.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|error| format!("Could not open the config lock: {error}"))?;
        lock_file
            .lock()
            .map_err(|error| format!("Could not lock the config store: {error}"))?;

        let mut latest = read_data(&self.path)?;
        let mut changed = latest.remove("campaigns").is_some();
        changed |= latest.remove("accounts").is_some();
        let account_keys: Vec<String> = latest
            .keys()
            .filter(|key| key.starts_with("accounts:"))
            .cloned()
            .collect();
        for key in account_keys {
            changed |= latest.remove(&key).is_some();
        }
        if changed {
            write_data(&self.path, &latest)?;
        }
        self.data = latest;
        Ok(())
    }
}

fn has_legacy_app_data(data: &Map<String, Value>) -> bool {
    data.contains_key("campaigns")
        || data.contains_key("accounts")
        || data.keys().any(|key| key.starts_with("accounts:"))
}

fn read_data(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(format!("Could not read the config store: {error}")),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("The config store contains invalid JSON: {error}"))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "The config store must contain a JSON object.".to_string())
}

/// Replace the complete object only after it is flushed to a unique sibling
/// file. A unique name prevents independent app processes from sharing a temp
/// path even if an external process ignores the lock file.
fn write_data(path: &Path, data: &Map<String, Value>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&Value::Object(data.clone()))
        .map_err(|error| error.to_string())?;
    let temp = path.with_file_name(format!(
        "novamail-command-{}.json.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let write_result = (|| {
        let mut file = File::create(&temp).map_err(|error| error.to_string())?;
        file.write_all(text.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temp, path).map_err(|error| error.to_string())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(temp);
    }
    write_result
}

/// Milliseconds since the Unix epoch, matching JavaScript's `Date.now()`.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
