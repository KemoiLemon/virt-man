use crate::api::uploads::Uploads;
use crate::store::Store;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<RwLock<Store>>,
    pub data_dir: PathBuf,
    /// True when running as the desktop app's loopback sidecar: no auth,
    /// every request is a synthetic local admin.
    pub auth_disabled: bool,
    /// In-flight staged uploads, keyed by id. See `api::uploads`.
    pub uploads: Uploads,
}

impl AppState {
    pub fn new(store: Store, data_dir: PathBuf, auth_disabled: bool) -> Self {
        Self {
            store: Arc::new(RwLock::new(store)),
            data_dir,
            auth_disabled,
            uploads: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Read lock over the store.
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, Store> {
        self.store.read().expect("store lock poisoned")
    }

    /// Write lock over the store.
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, Store> {
        self.store.write().expect("store lock poisoned")
    }
}
