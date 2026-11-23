//! Identities in `config.db`: reads for compose, and the CRUD behind
//! `/api/identities` with its change events. Every account keeps at least
//! one identity and exactly one default.

use tagmail_api_core::compose::{ComposeError, ComposeResult};
use tagmail_api_core::compose_dto::IdentityDto;
use tagmail_api_core::identities::{IdentityPatch, NewIdentity};
use tagmail_store::rusqlite::{self, Connection, OptionalExtension, params};
use tagmail_store::{ChangeOp, Entity, NewChange, changes};

use crate::compose::{read, write};
use crate::supervisor::{ConnectorFactory, Engine};

const COLUMNS: &str = "id, account_id, address, display_name, signature_html, is_default";

fn sql(e: rusqlite::Error) -> ComposeError {
    ComposeError::Internal(Box::new(tagmail_store::Error::from(e)))
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<IdentityDto> {
    Ok(IdentityDto {
        id: r.get(0)?,
        account_id: r.get(1)?,
        address: r.get(2)?,
        name: r.get(3)?,
        signature: r.get(4)?,
        is_default: r.get(5)?,
    })
}

/// Every identity, by account, each account's default first.
pub(crate) fn identities(c: &Connection) -> ComposeResult<Vec<IdentityDto>> {
    let mut stmt = c
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM identity ORDER BY account_id, is_default DESC, id"
        ))
        .map_err(sql)?;
    let rows = stmt
        .query_map([], row)
        .map_err(sql)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sql)?;
    Ok(rows)
}

fn find(c: &Connection, id: i64) -> ComposeResult<Option<IdentityDto>> {
    c.query_row(
        &format!("SELECT {COLUMNS} FROM identity WHERE id = ?1"),
        [id],
        row,
    )
    .optional()
    .map_err(sql)
}

/// The identity a draft is sent from.
pub(crate) fn identity(c: &Connection, id: i64) -> ComposeResult<IdentityDto> {
    find(c, id)?.ok_or_else(|| ComposeError::Invalid {
        field: "identityId",
        message: "no such identity".to_owned(),
    })
}

fn address_taken(c: &Connection, account: i64, address: &str, except: i64) -> ComposeResult<()> {
    let taken: bool = c
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM identity WHERE account_id = ?1 \
             AND address = ?2 COLLATE NOCASE AND id != ?3)",
            params![account, address, except],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if taken {
        return Err(ComposeError::Conflict(format!(
            "the account already has the identity {address}"
        )));
    }
    Ok(())
}

fn make_default(c: &Connection, account: i64, id: i64) -> ComposeResult<()> {
    c.execute(
        "UPDATE identity SET is_default = 0 WHERE account_id = ?1 AND id != ?2",
        params![account, id],
    )
    .map_err(sql)?;
    c.execute("UPDATE identity SET is_default = 1 WHERE id = ?1", [id])
        .map_err(sql)?;
    Ok(())
}

fn create(c: &Connection, new: &NewIdentity) -> ComposeResult<IdentityDto> {
    let active: Option<String> = c
        .query_row(
            "SELECT status FROM account WHERE id = ?1",
            [new.account_id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    if active.is_none_or(|s| s == "deleting") {
        return Err(ComposeError::NotFound("account"));
    }
    address_taken(c, new.account_id.0, &new.address, 0)?;
    c.execute(
        "INSERT INTO identity (account_id, address, display_name, signature_html) \
         VALUES (?1, ?2, ?3, ?4)",
        params![new.account_id.0, new.address, new.name, new.signature],
    )
    .map_err(sql)?;
    let id = c.last_insert_rowid();
    let has_default: bool = c
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM identity WHERE account_id = ?1 AND is_default = 1)",
            [new.account_id.0],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if new.is_default || !has_default {
        make_default(c, new.account_id.0, id)?;
    }
    identity(c, id)
}

fn update(c: &Connection, id: i64, patch: &IdentityPatch) -> ComposeResult<IdentityDto> {
    let current = find(c, id)?.ok_or(ComposeError::NotFound("identity"))?;
    if let Some(address) = &patch.address {
        address_taken(c, current.account_id, address, id)?;
    }
    c.execute(
        "UPDATE identity SET address = coalesce(?2, address), \
         display_name = CASE WHEN ?3 THEN ?4 ELSE display_name END, \
         signature_html = CASE WHEN ?5 THEN ?6 ELSE signature_html END WHERE id = ?1",
        params![
            id,
            patch.address,
            patch.name.is_some(),
            patch.name.clone().flatten(),
            patch.signature.is_some(),
            patch.signature.clone().flatten(),
        ],
    )
    .map_err(sql)?;
    if patch.make_default {
        make_default(c, current.account_id, id)?;
    }
    identity(c, id)
}

/// Deletes the identity; returns the one promoted to default, if any.
fn delete(c: &Connection, id: i64) -> ComposeResult<Option<IdentityDto>> {
    let current = find(c, id)?.ok_or(ComposeError::NotFound("identity"))?;
    let count: i64 = c
        .query_row(
            "SELECT count(*) FROM identity WHERE account_id = ?1",
            [current.account_id],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if count <= 1 {
        return Err(ComposeError::Conflict(
            "an account needs at least one identity".to_owned(),
        ));
    }
    c.execute("DELETE FROM identity WHERE id = ?1", [id])
        .map_err(sql)?;
    if !current.is_default {
        return Ok(None);
    }
    let next: i64 = c
        .query_row(
            "SELECT min(id) FROM identity WHERE account_id = ?1",
            [current.account_id],
            |r| r.get(0),
        )
        .map_err(sql)?;
    make_default(c, current.account_id, next)?;
    identity(c, next).map(Some)
}

impl<F: ConnectorFactory> Engine<F> {
    /// Logs identity changes (`None`: deleted) and wakes the event streams.
    async fn emit_identities(&self, changed: Vec<(i64, Option<IdentityDto>)>) {
        let changes: Vec<NewChange> = changed
            .into_iter()
            .map(|(id, identity)| NewChange {
                entity: Entity::Identity,
                entity_id: id,
                op: if identity.is_some() {
                    ChangeOp::Upsert
                } else {
                    ChangeOp::Delete
                },
                payload: identity.and_then(|i| serde_json::to_string(&i).ok()),
            })
            .collect();
        let stores = self.stores();
        let epoch = stores.cache.epoch();
        let head = stores
            .cache
            .write(move |tx| {
                changes::append_all(tx, &changes)?;
                changes::head(tx, epoch)
            })
            .await;
        match head {
            Ok(head) => stores.feed.publish(head),
            Err(e) => tracing::warn!(error = %e, "cannot publish an identity change"),
        }
    }

    /// The account's other identities, after a default change demoted one.
    async fn account_identities(&self, account: i64) -> ComposeResult<Vec<IdentityDto>> {
        let all = read(self.stores(), identities).await?;
        Ok(all
            .into_iter()
            .filter(|i| i.account_id == account)
            .collect())
    }

    async fn emit_account_identities(&self, account: i64) -> ComposeResult<()> {
        let changed = self
            .account_identities(account)
            .await?
            .into_iter()
            .map(|i| (i.id, Some(i)))
            .collect();
        self.emit_identities(changed).await;
        Ok(())
    }

    pub(crate) async fn create_identity_impl(
        &self,
        new: NewIdentity,
    ) -> ComposeResult<IdentityDto> {
        let identity = write(self.stores(), move |tx| create(tx, &new)).await?;
        self.emit_account_identities(identity.account_id).await?;
        Ok(identity)
    }

    pub(crate) async fn update_identity_impl(
        &self,
        id: i64,
        patch: IdentityPatch,
    ) -> ComposeResult<IdentityDto> {
        let default_changed = patch.make_default;
        let identity = write(self.stores(), move |tx| update(tx, id, &patch)).await?;
        if default_changed {
            self.emit_account_identities(identity.account_id).await?;
        } else {
            self.emit_identities(vec![(id, Some(identity.clone()))])
                .await;
        }
        Ok(identity)
    }

    pub(crate) async fn delete_identity_impl(&self, id: i64) -> ComposeResult<()> {
        let promoted = write(self.stores(), move |tx| delete(tx, id)).await?;
        let mut changed = vec![(id, None)];
        changed.extend(promoted.map(|i| (i.id, Some(i))));
        self.emit_identities(changed).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tagmail_core::AccountId;
    use tagmail_store::ConfigDb;
    use tempfile::TempDir;

    use super::*;

    fn db() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        drop(ConfigDb::open(&dir.path().join("config.db")).unwrap());
        let c = Connection::open(dir.path().join("config.db")).unwrap();
        c.execute_batch(
            "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
             smtp_port, smtp_tls, username, secret_ct, secret_nonce, email) VALUES \
             (1, 'a', 'i', 993, 'tls', 's', 465, 'tls', 'a@a', x'00', x'00', 'a@a'), \
             (2, 'b', 'i', 993, 'tls', 's', 465, 'tls', 'b@b', x'00', x'00', 'b@b'); \
             INSERT INTO identity (id, account_id, address, is_default) VALUES \
             (1, 1, 'a@a', 1), (2, 2, 'b@b', 1);",
        )
        .unwrap();
        (dir, c)
    }

    fn new(account: i64, address: &str, is_default: bool) -> NewIdentity {
        NewIdentity {
            account_id: AccountId(account),
            address: address.to_owned(),
            name: Some("B".to_owned()),
            signature: Some("<p>sig</p>".to_owned()),
            is_default,
        }
    }

    fn defaults(c: &Connection) -> Vec<(i64, i64)> {
        identities(c)
            .unwrap()
            .into_iter()
            .filter(|i| i.is_default)
            .map(|i| (i.account_id, i.id))
            .collect()
    }

    #[test]
    fn crud_keeps_one_default_per_account() {
        let (_dir, c) = db();
        let alias = create(&c, &new(2, "alias@b", false)).unwrap();
        assert_eq!(
            (alias.is_default, alias.signature.as_deref()),
            (false, Some("<p>sig</p>"))
        );
        assert_eq!(defaults(&c), [(1, 1), (2, 2)]);

        let promoted = create(&c, &new(2, "boss@b", true)).unwrap();
        assert_eq!(defaults(&c), [(1, 1), (2, promoted.id)]);

        let patched = update(
            &c,
            alias.id,
            &IdentityPatch {
                name: Some(None),
                signature: Some(None),
                make_default: true,
                ..IdentityPatch::default()
            },
        )
        .unwrap();
        assert_eq!((patched.name, patched.signature), (None, None));
        assert_eq!(patched.address, "alias@b");
        assert_eq!(defaults(&c), [(1, 1), (2, alias.id)]);

        let next = delete(&c, alias.id).unwrap().unwrap();
        assert_eq!(next.id, 2, "lowest remaining id promoted");
        assert_eq!(defaults(&c), [(1, 1), (2, 2)]);
        assert_eq!(delete(&c, promoted.id).unwrap(), None);
    }

    #[test]
    fn conflicts_and_missing_rows_are_refused() {
        let (_dir, c) = db();
        assert!(matches!(
            create(&c, &new(2, "B@B", false)),
            Err(ComposeError::Conflict(_))
        ));
        assert!(
            create(&c, &new(1, "b@b", false)).is_ok(),
            "addresses are per account"
        );
        let alias = create(&c, &new(2, "alias@b", false)).unwrap();
        assert!(matches!(
            update(
                &c,
                alias.id,
                &IdentityPatch {
                    address: Some("b@B".to_owned()),
                    ..IdentityPatch::default()
                }
            ),
            Err(ComposeError::Conflict(_))
        ));
        assert!(matches!(delete(&c, 99), Err(ComposeError::NotFound(_))));
        assert!(matches!(
            create(&c, &new(9, "x@x", false)),
            Err(ComposeError::NotFound("account"))
        ));
    }

    #[test]
    fn the_last_identity_cannot_be_deleted() {
        let (_dir, c) = db();
        assert!(matches!(delete(&c, 2), Err(ComposeError::Conflict(_))));
    }
}
