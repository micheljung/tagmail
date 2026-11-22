use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::server::{Ack, LabelOp, MockServer, Offline, SnapshotRow};

pub struct Replica {
    db: Connection,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ThreadRow {
    pub thread_id: i64,
    pub subject: String,
    pub email_ids: Vec<String>,
    pub labels: Vec<String>,
    pub pending: bool,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct FlushReport {
    pub sent: usize,
    pub duplicates: usize,
    pub remaining: usize,
}

const SCHEMA: &str = "
CREATE TABLE message(
  email_id TEXT PRIMARY KEY,
  thread_id INTEGER NOT NULL,
  subject TEXT NOT NULL,
  sender TEXT NOT NULL,
  date INTEGER NOT NULL
);
CREATE INDEX message_thread ON message(thread_id, date);
CREATE TABLE message_label(
  email_id TEXT NOT NULL,
  label TEXT NOT NULL,
  PRIMARY KEY(email_id, label)
) WITHOUT ROWID;
CREATE TABLE intent(
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  op_id TEXT NOT NULL UNIQUE,
  op TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending'
);
";

impl Replica {
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db })
    }

    /// Loads a `GET /api/replica/snapshot` NDJSON body in one transaction.
    pub fn bootstrap(&mut self, ndjson: &str) -> rusqlite::Result<usize> {
        let tx = self.db.transaction()?;
        let mut count = 0;
        {
            let mut msg = tx.prepare(
                "INSERT INTO message(email_id, thread_id, subject, sender, date) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut lbl =
                tx.prepare("INSERT INTO message_label(email_id, label) VALUES (?1, ?2)")?;
            for line in ndjson.lines().filter(|l| !l.is_empty()) {
                let row: SnapshotRow = serde_json::from_str(line)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
                msg.execute(params![
                    row.email_id,
                    row.thread_id,
                    row.subject,
                    row.sender,
                    row.date
                ])?;
                for label in &row.labels {
                    lbl.execute(params![row.email_id, label])?;
                }
                count += 1;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn message_count(&self) -> rusqlite::Result<i64> {
        self.db
            .query_row("SELECT count(*) FROM message", [], |r| r.get(0))
    }

    /// Newest threads first, with pending intents projected over the base labels.
    pub fn threads(&self, limit: usize) -> rusqlite::Result<Vec<ThreadRow>> {
        let mut stmt = self.db.prepare(
            "SELECT thread_id, max(subject), group_concat(email_id) FROM message
             GROUP BY thread_id ORDER BY max(date) DESC LIMIT ?1",
        )?;
        let base: Vec<(i64, String, String)> = stmt
            .query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let pending = self.pending()?;
        base.into_iter()
            .map(|(thread_id, subject, ids)| {
                let email_ids: Vec<String> = ids.split(',').map(str::to_owned).collect();
                let mut labels = BTreeMap::new();
                for id in &email_ids {
                    labels.insert(id.clone(), self.base_labels(id)?);
                }
                let mut touched = false;
                for op in &pending {
                    for id in &op.email_ids {
                        let Some(set) = labels.get_mut(id) else {
                            continue;
                        };
                        set.extend(op.add.iter().cloned());
                        for l in &op.remove {
                            set.remove(l);
                        }
                        touched = true;
                    }
                }
                let union: BTreeSet<String> = labels.into_values().flatten().collect();
                Ok(ThreadRow {
                    thread_id,
                    subject,
                    email_ids,
                    labels: union.into_iter().collect(),
                    pending: touched,
                })
            })
            .collect()
    }

    fn base_labels(&self, email_id: &str) -> rusqlite::Result<BTreeSet<String>> {
        let mut stmt = self
            .db
            .prepare_cached("SELECT label FROM message_label WHERE email_id = ?1")?;
        stmt.query_map([email_id], |r| r.get(0))?.collect()
    }

    pub fn queue(
        &self,
        email_ids: Vec<String>,
        add: Vec<String>,
        remove: Vec<String>,
    ) -> rusqlite::Result<String> {
        let op = LabelOp {
            op_id: uuid::Uuid::new_v4().to_string(),
            email_ids,
            add,
            remove,
        };
        let json = serde_json::to_string(&op).expect("serializable");
        self.db.execute(
            "INSERT INTO intent(op_id, op) VALUES (?1, ?2)",
            params![op.op_id, json],
        )?;
        Ok(op.op_id)
    }

    pub fn pending(&self) -> rusqlite::Result<Vec<LabelOp>> {
        let mut stmt = self
            .db
            .prepare_cached("SELECT op FROM intent WHERE state = 'pending' ORDER BY seq")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .map(|json| json.map(|j| serde_json::from_str(&j).expect("stored by queue")))
            .collect()
    }

    /// Sends pending intents in order. Stops at the first `Offline`; the
    /// rest stays queued. `lose_acks` drops the server's acks to model a
    /// crash between send and commit.
    pub fn flush(
        &mut self,
        server: &mut MockServer,
        lose_acks: bool,
    ) -> rusqlite::Result<FlushReport> {
        let mut report = FlushReport::default();
        for op in self.pending()? {
            match server.apply(&op) {
                Err(Offline) => break,
                Ok(ack) => {
                    report.sent += 1;
                    if ack == Ack::Duplicate {
                        report.duplicates += 1;
                    }
                    if !lose_acks {
                        self.commit(&op)?;
                    }
                }
            }
        }
        report.remaining = self.pending()?.len();
        Ok(report)
    }

    fn commit(&mut self, op: &LabelOp) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        for id in &op.email_ids {
            for l in &op.add {
                tx.execute(
                    "INSERT OR IGNORE INTO message_label(email_id, label) VALUES (?1, ?2)",
                    params![id, l],
                )?;
            }
            for l in &op.remove {
                tx.execute(
                    "DELETE FROM message_label WHERE email_id = ?1 AND label = ?2",
                    params![id, l],
                )?;
            }
        }
        tx.execute(
            "UPDATE intent SET state = 'done' WHERE op_id = ?1",
            [&op.op_id],
        )?;
        tx.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bootstrapped(n: usize) -> (Replica, MockServer) {
        let server = MockServer::synthetic(n);
        let mut replica = Replica::open_in_memory().unwrap();
        assert_eq!(replica.bootstrap(&server.snapshot_ndjson()).unwrap(), n);
        (replica, server)
    }

    #[test]
    fn bootstrap_1k() {
        let (replica, _) = bootstrapped(1000);
        assert_eq!(replica.message_count().unwrap(), 1000);
        let threads = replica.threads(50).unwrap();
        assert_eq!(threads.len(), 50);
        assert_eq!(threads[0].thread_id, 333);
        assert_eq!(threads[1].email_ids.len(), 3);
    }

    #[test]
    fn offline_label_op_round_trip() {
        let (mut replica, mut server) = bootstrapped(1000);
        let thread = replica.threads(1).unwrap().remove(0);
        server.online = false;

        replica
            .queue(
                thread.email_ids.clone(),
                vec!["Spike".into()],
                vec!["Work".into()],
            )
            .unwrap();
        let shown = replica.threads(1).unwrap().remove(0);
        assert!(shown.pending);
        assert_eq!(shown.labels, ["Inbox", "Spike"]);
        assert_eq!(
            replica.flush(&mut server, false).unwrap(),
            FlushReport {
                sent: 0,
                duplicates: 0,
                remaining: 1
            }
        );
        assert!(
            !server
                .labels(&thread.email_ids[0])
                .contains(&"Spike".to_owned())
        );

        server.online = true;
        assert_eq!(
            replica.flush(&mut server, false).unwrap(),
            FlushReport {
                sent: 1,
                duplicates: 0,
                remaining: 0
            }
        );
        for id in &thread.email_ids {
            assert_eq!(server.labels(id), ["Inbox", "Spike"]);
        }
        let shown = replica.threads(1).unwrap().remove(0);
        assert!(!shown.pending);
        assert_eq!(shown.labels, ["Inbox", "Spike"]);
    }

    #[test]
    fn lost_ack_is_deduped_by_op_id() {
        let (mut replica, mut server) = bootstrapped(30);
        let thread = replica.threads(1).unwrap().remove(0);
        replica
            .queue(thread.email_ids, vec!["Spike".into()], vec![])
            .unwrap();

        assert_eq!(
            replica.flush(&mut server, true).unwrap(),
            FlushReport {
                sent: 1,
                duplicates: 0,
                remaining: 1
            }
        );
        assert_eq!(server.applied, 1);
        assert_eq!(
            replica.flush(&mut server, false).unwrap(),
            FlushReport {
                sent: 1,
                duplicates: 1,
                remaining: 0
            }
        );
        assert_eq!(server.applied, 1);
    }

    #[test]
    fn intents_flush_in_order() {
        let (mut replica, mut server) = bootstrapped(30);
        let ids = replica.threads(1).unwrap().remove(0).email_ids;
        server.online = false;
        replica
            .queue(ids.clone(), vec!["A".into()], vec![])
            .unwrap();
        replica
            .queue(ids.clone(), vec![], vec!["A".into()])
            .unwrap();
        assert!(
            !replica.threads(1).unwrap()[0]
                .labels
                .contains(&"A".to_owned())
        );
        server.online = true;
        replica.flush(&mut server, false).unwrap();
        assert!(!server.labels(&ids[0]).contains(&"A".to_owned()));
    }
}
