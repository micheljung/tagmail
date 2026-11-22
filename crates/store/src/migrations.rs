use rusqlite::Connection;

use crate::error::{Error, Result};

pub(crate) const CONFIG: &[&str] = &[
    include_str!("../migrations/config/V1__init.sql"),
    include_str!("../migrations/config/V2__image_sender.sql"),
];
pub(crate) const CACHE: &[&str] = &[
    include_str!("../migrations/cache/V1__init.sql"),
    include_str!("../migrations/cache/V2__pending_text_index.sql"),
];

/// Applies the migrations after `PRAGMA user_version`, each in its own
/// transaction together with the version bump.
pub(crate) fn apply(conn: &mut Connection, migrations: &[&str]) -> Result<()> {
    let supported = migrations.len() as i64;
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > supported {
        return Err(Error::SchemaTooNew {
            found: current,
            supported,
        });
    }
    for (version, sql) in (1..).zip(migrations).skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}
