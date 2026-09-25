use crate::store::Store;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<RwLock<Store>>,
    pub data_dir: PathBuf,
}

impl AppState {
    pub fn new(store: Store, data_dir: PathBuf) -> Self {
        Self {
            store: Arc::new(RwLock::new(store)),
            data_dir,
        }
    }

    /// Read lock over the store.
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, Store> {
        self.store.read().unwrap()
    }

    /// Write lock over the store.
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, Store> {
        self.store.write().unwrap()
    }
}
