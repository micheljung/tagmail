//! The local stores the shell reads through: `<data>/replica/{cache,config}.db`
//! plus message blobs, opened as the same [`SqliteStore`] the server uses.

use std::path::Path;
use std::sync::Arc;

use tagmail_api_core::events::ChangeFeed;
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_store::credentials::KEY_LEN;
use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey};

pub const REPLICA_DIR: &str = "replica";

#[derive(Clone)]
pub struct Local {
    pub store: Arc<SqliteStore>,
    pub feed: Arc<ChangeFeed>,
}

/// The replica's config.db holds no account secrets, so its data key needs
/// no protection and a constant master key keeps it openable across runs.
fn replica_master_key() -> MasterKey {
    MasterKey::from_bytes(zeroize::Zeroizing::new([0u8; KEY_LEN]))
}

pub async fn open(data_dir: &Path) -> tagmail_store::Result<Local> {
    let dir = data_dir.join(REPLICA_DIR);
    let (config, cache, blobs) = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir)?;
        let config = ConfigDb::open(&dir.join("config.db"))?;
        let cache = CacheDb::open(&dir.join("cache.db"))?;
        let blobs = BlobStore::open(&dir.join("blobs"))?;
        Ok::<_, tagmail_store::Error>((config, cache, blobs))
    })
    .await
    .expect("opening the replica does not panic")?;
    let store = SqliteStore::open(config, cache, blobs, &replica_master_key()).await?;
    let feed = Arc::new(ChangeFeed::new(store.cache().head().await?));
    store.set_feed(feed.clone());
    Ok(Local {
        store: Arc::new(store),
        feed,
    })
}
