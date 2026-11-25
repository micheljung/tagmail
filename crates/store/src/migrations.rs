use rusqlite::Connection;

use crate::error::{Error, Result};

pub(crate) const CONFIG: &[&str] = &[
    include_str!("../migrations/config/V1__init.sql"),
    include_str!("../migrations/config/V2__image_sender.sql"),
    include_str!("../migrations/config/V3__compose.sql"),
    include_str!("../migrations/config/V4__outbox_file.sql"),
    include_str!("../migrations/config/V5__multi_account.sql"),
    include_str!("../migrations/config/V6__default_identity.sql"),
    include_str!("../migrations/config/V7__draft_client_id.sql"),
    include_str!("../migrations/config/V8__push_subscription.sql"),
];
pub(crate) const CACHE: &[&str] = &[
    include_str!("../migrations/cache/V1__init.sql"),
    include_str!("../migrations/cache/V2__pending_text_index.sql"),
    include_str!("../migrations/cache/V3__op_overlay.sql"),
    include_str!("../migrations/cache/V4__hide_has_cal.sql"),
    include_str!("../migrations/cache/V5__has_calendar.sql"),
];

/// `user_version` of an up-to-date `config.db`.
pub const CONFIG_SCHEMA: i64 = CONFIG.len() as i64;
/// `user_version` of an up-to-date `cache.db`.
pub const CACHE_SCHEMA: i64 = CACHE.len() as i64;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// `user_version` is the list position, so each file's `V<n>` must be
    /// its position plus one.
    fn assert_numbered(dir: &str, migrations: &[&str]) {
        let path = format!("{}/migrations/{dir}", env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<(u32, String)> = std::fs::read_dir(&path)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .map(|name| {
                let n = name
                    .strip_prefix('V')
                    .and_then(|rest| rest.split_once("__"))
                    .and_then(|(n, _)| n.parse().ok())
                    .unwrap_or_else(|| panic!("{dir}/{name} is not V<n>__<name>.sql"));
                (n, name)
            })
            .collect();
        files.sort();
        let numbers: Vec<u32> = files.iter().map(|(n, _)| *n).collect();
        let expected: Vec<u32> = (1..).take(migrations.len()).collect();
        assert_eq!(numbers, expected, "{dir}: {files:?}");
        for ((_, name), sql) in files.iter().zip(migrations) {
            let file = std::fs::read_to_string(format!("{path}/{name}")).unwrap();
            assert_eq!(&file, sql, "{dir}/{name} is not at its position");
        }
    }

    #[test]
    fn migration_numbers_match_their_positions() {
        assert_numbered("config", CONFIG);
        assert_numbered("cache", CACHE);
    }
}
