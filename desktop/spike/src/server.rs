use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LabelOp {
    pub op_id: String,
    pub email_ids: Vec<String>,
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ack {
    Applied,
    Duplicate,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Offline;

#[derive(Clone, Serialize, Deserialize)]
pub struct SnapshotRow {
    pub email_id: String,
    pub thread_id: i64,
    pub subject: String,
    pub sender: String,
    pub date: i64,
    pub labels: Vec<String>,
}

/// Stands in for the api-server: owns the truth and dedupes client op ids
/// the way `op_seen` will.
pub struct MockServer {
    pub online: bool,
    rows: Vec<SnapshotRow>,
    labels: HashMap<String, BTreeSet<String>>,
    op_seen: HashSet<String>,
    pub applied: u64,
}

const MESSAGES_PER_THREAD: usize = 3;
const BASE_DATE: i64 = 1_760_000_000;
const SENDERS: [&str; 4] = [
    "alice@example.org",
    "bob@example.org",
    "carol@example.org",
    "dave@example.org",
];

impl MockServer {
    pub fn synthetic(messages: usize) -> Self {
        let rows: Vec<SnapshotRow> = (0..messages)
            .map(|i| SnapshotRow {
                email_id: format!("E{i:06}"),
                thread_id: (i / MESSAGES_PER_THREAD) as i64,
                subject: format!("Synthetic thread {}", i / MESSAGES_PER_THREAD),
                sender: SENDERS[i % SENDERS.len()].to_owned(),
                date: BASE_DATE + i as i64 * 60,
                labels: if i % 2 == 0 {
                    vec!["Inbox".to_owned()]
                } else {
                    vec!["Inbox".to_owned(), "Work".to_owned()]
                },
            })
            .collect();
        let labels = rows
            .iter()
            .map(|r| (r.email_id.clone(), r.labels.iter().cloned().collect()))
            .collect();
        Self {
            online: true,
            rows,
            labels,
            op_seen: HashSet::new(),
            applied: 0,
        }
    }

    /// The `GET /api/replica/snapshot` NDJSON body.
    pub fn snapshot_ndjson(&self) -> String {
        let mut out = String::new();
        for row in &self.rows {
            let labels = self.labels(&row.email_id);
            let row = SnapshotRow {
                labels,
                ..row.clone()
            };
            out.push_str(&serde_json::to_string(&row).expect("serializable"));
            out.push('\n');
        }
        out
    }

    pub fn labels(&self, email_id: &str) -> Vec<String> {
        self.labels
            .get(email_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn apply(&mut self, op: &LabelOp) -> Result<Ack, Offline> {
        if !self.online {
            return Err(Offline);
        }
        if !self.op_seen.insert(op.op_id.clone()) {
            return Ok(Ack::Duplicate);
        }
        for id in &op.email_ids {
            let set = self.labels.entry(id.clone()).or_default();
            set.extend(op.add.iter().cloned());
            for label in &op.remove {
                set.remove(label);
            }
        }
        self.applied += 1;
        Ok(Ack::Applied)
    }
}
