//! Queued writes in `replica-durable.db` (§5): `pending` until the server
//! accepted the op, `sent` while it runs there, then `done` or `failed`.
//! Failed and expired intents stay listed until the user discards them.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, Row, params};
use tagmail_api_core::ops_dto::MailboxRefDto;
use tagmail_core::EmailId;
use tagmail_core::ops::OpKind;

use crate::durable::Durable;
use crate::error::{Error, Result};

/// Below the server's 120-day `op_seen` retention, so a replayed op id is
/// still recognised.
pub const INTENT_TTL_SECS: i64 = 90 * 24 * 60 * 60;
pub const DONE_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;
pub const EXPIRED: &str = "expired";
/// How long the server may run an accepted op before the replica gives up
/// on its verdict.
pub const SENT_TTL_SECS: i64 = 24 * 60 * 60;
pub const SENT_RECHECK_SECS: i64 = 5 * 60;
pub const NO_VERDICT: &str = "No answer from the server";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentState {
    Pending,
    Sent,
    Done,
    Failed,
}

impl IntentState {
    pub fn as_str(self) -> &'static str {
        match self {
            IntentState::Pending => "pending",
            IntentState::Sent => "sent",
            IntentState::Done => "done",
            IntentState::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Result<IntentState> {
        Ok(match s {
            "pending" => IntentState::Pending,
            "sent" => IntentState::Sent,
            "done" => IntentState::Done,
            "failed" => IntentState::Failed,
            other => return Err(Error::Corrupt(format!("intent state {other:?}"))),
        })
    }

    /// Projected over the replica.
    pub fn is_open(self) -> bool {
        matches!(self, IntentState::Pending | IntentState::Sent)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub op_id: String,
    pub seq: i64,
    pub kind: OpKind,
    pub account_id: i64,
    pub email_ids: BTreeSet<EmailId>,
    /// As the client named it; resolved to a keyword when projected.
    pub label: Option<String>,
    pub mailbox: Option<MailboxRefDto>,
    pub created_at: i64,
    pub state: IntentState,
    pub attempts: u32,
    pub next_at: Option<i64>,
    pub error: Option<String>,
}

const COLUMNS: &str = "op_id, seq, kind, account_id, email_ids, label, mailbox_ref, created_at, \
     state, attempts, next_at, error";

struct Raw {
    op_id: String,
    seq: i64,
    kind: String,
    account_id: i64,
    email_ids: String,
    label: Option<String>,
    mailbox: Option<String>,
    created_at: i64,
    state: String,
    attempts: u32,
    next_at: Option<i64>,
    error: Option<String>,
}

fn raw(r: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok(Raw {
        op_id: r.get(0)?,
        seq: r.get(1)?,
        kind: r.get(2)?,
        account_id: r.get(3)?,
        email_ids: r.get(4)?,
        label: r.get(5)?,
        mailbox: r.get(6)?,
        created_at: r.get(7)?,
        state: r.get(8)?,
        attempts: r.get(9)?,
        next_at: r.get(10)?,
        error: r.get(11)?,
    })
}

fn decode(r: Raw) -> Result<Intent> {
    let corrupt =
        |what: &str, e: &dyn std::fmt::Display| Error::Corrupt(format!("intent {what}: {e}"));
    let ids: Vec<String> =
        serde_json::from_str(&r.email_ids).map_err(|e| corrupt("email ids", &e))?;
    Ok(Intent {
        op_id: r.op_id,
        seq: r.seq,
        kind: r.kind.parse().map_err(|e| corrupt("kind", &e))?,
        account_id: r.account_id,
        email_ids: ids
            .iter()
            .map(|e| EmailId::parse(e))
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| corrupt("email id", &e))?,
        label: r.label,
        mailbox: r
            .mailbox
            .map(|m| serde_json::from_str(&m))
            .transpose()
            .map_err(|e| corrupt("mailbox", &e))?,
        created_at: r.created_at,
        state: IntentState::parse(&r.state)?,
        attempts: r.attempts,
        next_at: r.next_at,
        error: r.error,
    })
}

impl Durable {
    /// Inserts unless the op id exists; returns whether it was new.
    pub fn add_intent(&self, i: &Intent) -> Result<bool> {
        let ids: Vec<&str> = i.email_ids.iter().map(EmailId::as_str).collect();
        let mailbox = i
            .mailbox
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| Error::Corrupt(e.to_string()))?;
        let added = self.conn().execute(
            "INSERT OR IGNORE INTO intent (op_id, seq, kind, account_id, email_ids, label, \
             mailbox_ref, created_at, state, attempts, next_at, error) \
             VALUES (?1, (SELECT coalesce(max(seq), 0) + 1 FROM intent), ?2, ?3, ?4, ?5, ?6, ?7, \
             ?8, ?9, ?10, ?11)",
            params![
                i.op_id,
                i.kind.as_str(),
                i.account_id,
                serde_json::to_string(&ids).map_err(|e| Error::Corrupt(e.to_string()))?,
                i.label,
                mailbox,
                i.created_at,
                i.state.as_str(),
                i.attempts,
                i.next_at,
                i.error
            ],
        )?;
        Ok(added == 1)
    }

    pub fn intent(&self, op_id: &str) -> Result<Option<Intent>> {
        let row = self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM intent WHERE op_id = ?1"),
                [op_id],
                raw,
            )
            .optional()?;
        row.map(decode).transpose()
    }

    /// Intents in these states, oldest first.
    pub fn intents(&self, states: &[IntentState]) -> Result<Vec<Intent>> {
        let wanted: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM intent WHERE state IN (SELECT value FROM json_each(?1)) \
             ORDER BY seq"
        ))?;
        let rows = stmt
            .query_map(
                [serde_json::to_string(&wanted).map_err(|e| Error::Corrupt(e.to_string()))?],
                raw,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(decode).collect()
    }

    /// Moves an intent forward only: `pending` → `sent` → `done`/`failed`,
    /// or `pending` straight to a verdict. Whether it moved.
    pub fn advance_intent(
        &self,
        op_id: &str,
        state: IntentState,
        error: Option<&str>,
    ) -> Result<bool> {
        let now = crate::sync::now();
        Ok(self.conn().execute(
            "UPDATE intent SET state = ?2, error = ?3,
                 next_at = CASE WHEN ?2 = 'sent' THEN ?4 + ?5 END,
                 sent_at = CASE WHEN ?2 = 'sent' THEN ?4 ELSE sent_at END
             WHERE op_id = ?1
               AND (state = 'pending' AND ?2 != 'pending'
                    OR state = 'sent' AND ?2 IN ('done', 'failed'))",
            params![op_id, state.as_str(), error, now, SENT_RECHECK_SECS],
        )? == 1)
    }

    /// Asks the server about a `sent` intent again at `next_at`.
    pub fn recheck_at(&self, op_id: &str, next_at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE intent SET next_at = ?2 WHERE op_id = ?1 AND state = 'sent'",
            params![op_id, next_at],
        )?;
        Ok(())
    }

    /// Every `sent` intent is due for a recheck, as after a re-bootstrap
    /// that skipped the server's op changes.
    pub fn recheck_all_sent(&self) -> Result<()> {
        self.conn()
            .execute("UPDATE intent SET next_at = NULL WHERE state = 'sent'", [])?;
        Ok(())
    }

    /// A flush that did not reach a verdict; try again at `next_at`.
    pub fn defer_intent(&self, op_id: &str, next_at: i64, error: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE intent SET attempts = attempts + 1, next_at = ?2, error = ?3
             WHERE op_id = ?1 AND state = 'pending'",
            params![op_id, next_at, error],
        )?;
        Ok(())
    }

    /// Back to `pending` for another try.
    pub fn retry_intent(&self, op_id: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE intent SET state = 'pending', attempts = 0, next_at = NULL, error = NULL, \
             created_at = max(created_at, ?2) WHERE op_id = ?1 AND state = 'failed' AND other_server = 0",
            params![op_id, crate::sync::now() - INTENT_TTL_SECS + 1],
        )? == 1)
    }

    pub fn remove_intent(&self, op_id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM intent WHERE op_id = ?1", [op_id])?
            == 1)
    }

    /// Fails pending intents older than [`INTENT_TTL_SECS`] and sent ones
    /// without a verdict after [`SENT_TTL_SECS`], and prunes old done ones.
    /// Returns the intents that failed.
    pub fn expire_intents(&self, now: i64) -> Result<Vec<Intent>> {
        let stale_sent: BTreeSet<String> = {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT op_id FROM intent
                 WHERE state = 'sent' AND coalesce(sent_at, created_at) <= ?1",
            )?;
            stmt.query_map([now - SENT_TTL_SECS], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut expired = Vec::new();
        for i in self.intents(&[IntentState::Pending, IntentState::Sent])? {
            let error = match i.state {
                IntentState::Pending if i.created_at <= now - INTENT_TTL_SECS => EXPIRED,
                IntentState::Sent if stale_sent.contains(&i.op_id) => NO_VERDICT,
                _ => continue,
            };
            if self.advance_intent(&i.op_id, IntentState::Failed, Some(error))? {
                expired.extend(self.intent(&i.op_id)?);
            }
        }
        self.conn().execute(
            "DELETE FROM intent WHERE state = 'done' AND created_at <= ?1",
            [now - DONE_RETENTION_SECS],
        )?;
        Ok(expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(op_id: &str, created_at: i64) -> Intent {
        Intent {
            op_id: op_id.into(),
            seq: 0,
            kind: OpKind::Archive,
            account_id: 1,
            email_ids: BTreeSet::from([EmailId::parse("g:1").unwrap()]),
            label: None,
            mailbox: Some(MailboxRefDto {
                name: "Archive".into(),
                special_use: None,
                uidvalidity: Some(3),
            }),
            created_at,
            state: IntentState::Pending,
            attempts: 0,
            next_at: None,
            error: None,
        }
    }

    #[test]
    fn a_verdict_is_never_undone() {
        let dir = tempfile::tempdir().unwrap();
        let d = Durable::open(&dir.path().join("d.db")).unwrap();
        d.add_intent(&intent("done", 10)).unwrap();
        assert!(d.advance_intent("done", IntentState::Done, None).unwrap());
        assert!(!d.advance_intent("done", IntentState::Sent, None).unwrap());
        assert_eq!(d.intent("done").unwrap().unwrap().state, IntentState::Done);

        d.add_intent(&intent("sent", 10)).unwrap();
        assert!(d.advance_intent("sent", IntentState::Sent, None).unwrap());
        assert!(
            !d.advance_intent("sent", IntentState::Pending, None)
                .unwrap()
        );
        assert!(
            d.advance_intent("sent", IntentState::Failed, Some("x"))
                .unwrap()
        );
        assert!(!d.advance_intent("sent", IntentState::Done, None).unwrap());
    }

    #[test]
    fn a_sent_intent_without_a_verdict_fails_in_time() {
        let dir = tempfile::tempdir().unwrap();
        let d = Durable::open(&dir.path().join("d.db")).unwrap();
        let now = crate::sync::now();
        d.add_intent(&intent("old", now)).unwrap();
        d.add_intent(&intent("new", now)).unwrap();
        d.advance_intent("old", IntentState::Sent, None).unwrap();
        d.advance_intent("new", IntentState::Sent, None).unwrap();
        d.conn()
            .execute(
                "UPDATE intent SET sent_at = ?1 WHERE op_id = 'old'",
                [now - SENT_TTL_SECS],
            )
            .unwrap();

        let failed = d.expire_intents(now).unwrap();

        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].op_id, "old");
        assert_eq!(failed[0].state, IntentState::Failed);
        assert_eq!(failed[0].error.as_deref(), Some(NO_VERDICT));
        assert_eq!(d.intent("new").unwrap().unwrap().state, IntentState::Sent);
    }

    #[test]
    fn a_sent_intent_from_before_sent_at_also_fails_in_time() {
        let dir = tempfile::tempdir().unwrap();
        let d = Durable::open(&dir.path().join("d.db")).unwrap();
        let now = crate::sync::now();
        d.add_intent(&intent("legacy", now - SENT_TTL_SECS))
            .unwrap();
        d.advance_intent("legacy", IntentState::Sent, None).unwrap();
        d.conn()
            .execute("UPDATE intent SET sent_at = NULL", [])
            .unwrap();

        let failed = d.expire_intents(now).unwrap();

        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].error.as_deref(), Some(NO_VERDICT));
    }

    #[test]
    fn round_trips_and_ignores_a_replayed_op_id() {
        let dir = tempfile::tempdir().unwrap();
        let d = Durable::open(&dir.path().join("d.db")).unwrap();
        assert!(d.add_intent(&intent("a", 10)).unwrap());
        assert!(!d.add_intent(&intent("a", 99)).unwrap());
        assert!(d.add_intent(&intent("b", 11)).unwrap());

        let a = d.intent("a").unwrap().unwrap();
        assert_eq!(
            a,
            Intent {
                seq: 1,
                ..intent("a", 10)
            }
        );
        let all = d.intents(&[IntentState::Pending]).unwrap();
        assert_eq!(all.iter().map(|i| i.seq).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn expiry_fails_old_pending_intents_and_prunes_old_done_ones() {
        let dir = tempfile::tempdir().unwrap();
        let d = Durable::open(&dir.path().join("d.db")).unwrap();
        let now = 1_000_000_000;
        let day = 24 * 60 * 60;
        d.add_intent(&intent("old", now - 91 * day)).unwrap();
        d.add_intent(&intent("new", now - day)).unwrap();
        d.add_intent(&intent("done", now - 8 * day)).unwrap();
        d.advance_intent("done", IntentState::Done, None).unwrap();

        let expired = d.expire_intents(now).unwrap();

        assert_eq!(expired.len(), 1);
        let old = d.intent("old").unwrap().unwrap();
        assert_eq!(old.state, IntentState::Failed);
        assert_eq!(old.error.as_deref(), Some(EXPIRED));
        assert_eq!(
            d.intent("new").unwrap().unwrap().state,
            IntentState::Pending
        );
        assert!(d.intent("done").unwrap().is_none());
    }
}
