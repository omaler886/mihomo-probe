//! The `$persistentStore` backend.
//!
//! Faithful port of subs-check-pro's `loon_store.go`, which itself mirrors how
//! Sub-Store behaves on Loon/iOS: the bundle stores its ENTIRE configuration
//! under the single key `"sub-store"` (read from disk on every access, written
//! pretty-printed through tmp+rename), while every other key is an ephemeral
//! cache that lives in memory and is flushed to a side file after 2 quiet
//! seconds. Keeping the two-file split matters in practice: the cache keys are
//! written constantly (download caches), and syncing them to disk on every
//! write shows up as real I/O stalls on VPS disks.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// Key holding Sub-Store's own configuration object. Everything else is cache.
const MAIN_KEY: &str = "sub-store";
const CACHE_FLUSH_DELAY: Duration = Duration::from_secs(2);

pub trait KvStore: Send + Sync + 'static {
    fn read(&self, key: &str) -> Option<String>;
    fn write(&self, value: Option<String>, key: &str);
}

struct CacheState {
    data: HashMap<String, String>,
    flush_task: Option<tokio::task::JoinHandle<()>>,
}

pub struct JsonKvStore {
    main_path: PathBuf,
    cache_path: PathBuf,
    state: Mutex<CacheState>,
    // Weak self-reference so the debounced flush task can re-lock the state
    // after the quiet period; once the caller drops the store the upgrade
    // fails and the pending flush quietly does nothing.
    me: Weak<JsonKvStore>,
}

impl JsonKvStore {
    pub fn new_arc(dir: &Path) -> io::Result<Arc<Self>> {
        std::fs::create_dir_all(dir)?;
        let cache_path = dir.join("sub-store-cache.json");
        let mut data = HashMap::new();
        if let Ok(bytes) = std::fs::read(&cache_path) {
            if let Ok(parsed) = serde_json::from_slice::<HashMap<String, String>>(&bytes) {
                data = parsed;
            }
        }
        Ok(Arc::new_cyclic(|me| Self {
            main_path: dir.join("sub-store.json"),
            cache_path,
            state: Mutex::new(CacheState {
                data,
                flush_task: None,
            }),
            me: me.clone(),
        }))
    }

    fn write_cache(&self, value: Option<String>, key: &str) {
        let task = {
            let mut state = self.state.lock().unwrap();
            match value {
                None => {
                    state.data.remove(key);
                }
                Some(v) => {
                    state.data.insert(key.to_string(), v);
                }
            }
            // Cancel-and-replace debounce, same semantics as Go's time.AfterFunc.
            if let Some(task) = state.flush_task.take() {
                task.abort();
            }
            let me = self.me.clone();
            let cache_path = self.cache_path.clone();
            Some(tokio::spawn(async move {
                tokio::time::sleep(CACHE_FLUSH_DELAY).await;
                if let Some(store) = me.upgrade() {
                    let mut state = store.state.lock().unwrap();
                    state.flush_task = None;
                    if let Ok(body) = serde_json::to_vec(&state.data) {
                        let tmp = cache_path.with_extension("json.tmp");
                        if std::fs::write(&tmp, body).is_ok() {
                            let _ = std::fs::rename(&tmp, &cache_path);
                        }
                    }
                }
            }))
        };
        self.state.lock().unwrap().flush_task = task;
    }
}

impl KvStore for JsonKvStore {
    fn read(&self, key: &str) -> Option<String> {
        // The main key is always read fresh from disk: Sub-Store (and our own
        // server-side env rewriting) treat it as the source of truth.
        if key == MAIN_KEY {
            return std::fs::read(&self.main_path)
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned());
        }
        self.state.lock().unwrap().data.get(key).cloned()
    }

    fn write(&self, value: Option<String>, key: &str) {
        if key == MAIN_KEY {
            match value {
                None => {
                    let _ = std::fs::remove_file(&self.main_path);
                }
                Some(raw) => {
                    // Pretty-print like Go's MarshalIndent so humans can diff
                    // the config; preserve_order keeps the bundle's own key
                    // order, which is what round-trips cleanly.
                    let pretty = serde_json::from_str::<serde_json::Value>(&raw)
                        .ok()
                        .and_then(|v| serde_json::to_string_pretty(&v).ok())
                        .unwrap_or(raw);
                    let tmp = self.main_path.with_extension("json.tmp");
                    if std::fs::write(&tmp, pretty).is_ok() {
                        let _ = std::fs::rename(&tmp, &self.main_path);
                    }
                }
            }
            return;
        }
        self.write_cache(value, key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DirGuard(PathBuf);
    impl DirGuard {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("probe-substore-kv-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn main_key_roundtrips_through_disk() {
        let dir = DirGuard::new("main");
        let store = JsonKvStore::new_arc(&dir.0).unwrap();
        assert_eq!(store.read("sub-store"), None);
        store.write(Some(r#"{"subs":{}}"#.into()), "sub-store");
        // Main key must be readable without any async runtime.
        assert_eq!(store.read("sub-store").unwrap(), "{\n  \"subs\": {}\n}");
        store.write(None, "sub-store");
        assert_eq!(store.read("sub-store"), None);
    }

    #[tokio::test]
    async fn cache_key_roundtrips_after_debounce() {
        let dir = DirGuard::new("cache");
        let store = JsonKvStore::new_arc(&dir.0).unwrap();
        store.write(Some("v1".into()), "cache-key");
        assert_eq!(store.read("cache-key").as_deref(), Some("v1"));
        // The flush task needs a moment past the 2s debounce.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let on_disk = std::fs::read_to_string(dir.0.join("sub-store-cache.json")).unwrap();
        assert!(
            on_disk.contains("cache-key"),
            "cache was not flushed: {on_disk}"
        );
    }
}
