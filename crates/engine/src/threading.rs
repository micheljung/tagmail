//! Conversation threading (plan §3.5): a message joins every thread that
//! shares one of its ids (own Message-ID, In-Reply-To, References) or that
//! holds a message with the same normalized subject, a date within
//! [`SUBJECT_WINDOW_SECS`] of it and a common participant. Joining several
//! threads merges them into the largest, so the threads are the connected
//! components of those edges whatever the arrival order. Threads never
//! split.

use std::collections::BTreeSet;

use serde_json::json;
use tagmail_store::rusqlite::{OptionalExtension, params};
use tagmail_store::{ChangeOp, Entity, Result};

use crate::cache::{W, parse_addresses};
use crate::message::participants;

/// Two messages with the same subject are linked only within 14 days of
/// each other.
pub const SUBJECT_WINDOW_SECS: i64 = 14 * 24 * 60 * 60;

#[derive(Debug, Clone, Default)]
pub struct ThreadInput {
    pub msgid: Option<String>,
    pub refs: Vec<String>,
    pub subject_norm: String,
    pub date: i64,
    pub participants: Vec<String>,
}

/// Follows `merged_into` to the live thread.
fn live(w: &W<'_>, mut thread: i64) -> Result<i64> {
    loop {
        let next: Option<i64> = w
            .conn
            .query_row(
                "SELECT merged_into FROM thread WHERE id = ?1",
                [thread],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match next {
            Some(n) => thread = n,
            None => return Ok(thread),
        }
    }
}

fn subject_neighbours(w: &W<'_>, message: i64, input: &ThreadInput) -> Result<Vec<i64>> {
    if input.subject_norm.is_empty() || input.participants.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = w.conn.prepare_cached(
        "SELECT thread_id, from_json, to_json, cc_json FROM message \
         WHERE account_id = ?1 AND subject_norm = ?2 AND date BETWEEN ?3 AND ?4 \
           AND id != ?5 AND tombstoned_at IS NULL AND thread_id IS NOT NULL",
    )?;
    let rows = stmt.query_map(
        params![
            w.account,
            input.subject_norm,
            input.date.saturating_sub(SUBJECT_WINDOW_SECS),
            input.date.saturating_add(SUBJECT_WINDOW_SECS),
            message
        ],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (thread, from, to, cc) = row?;
        let theirs = participants([
            &parse_addresses(from.as_deref()),
            &parse_addresses(to.as_deref()),
            &parse_addresses(cc.as_deref()),
        ]);
        if theirs
            .iter()
            .any(|p| input.participants.binary_search(p).is_ok())
        {
            out.push(thread);
        }
    }
    Ok(out)
}

fn message_count(w: &W<'_>, thread: i64) -> Result<i64> {
    Ok(w.conn.query_row(
        "SELECT count(*) FROM message WHERE thread_id = ?1",
        [thread],
        |r| r.get(0),
    )?)
}

/// Moves everything of `from` into `into`; `from` keeps no list rows.
fn merge(w: &mut W<'_>, from: i64, into: i64) -> Result<()> {
    w.conn.execute(
        "UPDATE message SET thread_id = ?2 WHERE thread_id = ?1",
        params![from, into],
    )?;
    w.conn.execute(
        "UPDATE thread_ref SET thread_id = ?2 WHERE thread_id = ?1",
        params![from, into],
    )?;
    w.conn
        .execute("DELETE FROM thread_mailbox WHERE thread_id = ?1", [from])?;
    w.conn.execute(
        "UPDATE thread SET merged_into = ?2 WHERE id = ?1 OR merged_into = ?1",
        params![from, into],
    )?;
    w.conn.execute(
        "UPDATE thread SET msg_count = 0, unread_count = 0, starred = 0 WHERE id = ?1",
        [from],
    )?;
    w.emit(
        Entity::Thread,
        from,
        ChangeOp::Merged { target: into },
        Some(json!({ "target": into })),
    );
    Ok(())
}

/// Puts a freshly inserted message into its thread.
pub fn assign(w: &mut W<'_>, message: i64, input: &ThreadInput) -> Result<i64> {
    let mut ids: Vec<&str> = input.msgid.iter().map(String::as_str).collect();
    for r in &input.refs {
        if !ids.contains(&r.as_str()) {
            ids.push(r);
        }
    }
    let mut candidates = BTreeSet::new();
    {
        let mut stmt = w.conn.prepare_cached(
            "SELECT thread_id FROM thread_ref WHERE account_id = ?1 AND msgid_hdr = ?2",
        )?;
        for id in &ids {
            if let Some(t) = stmt
                .query_row(params![w.account, id], |r| r.get::<_, i64>(0))
                .optional()?
            {
                candidates.insert(t);
            }
        }
    }
    candidates.extend(subject_neighbours(w, message, input)?);
    let mut threads = BTreeSet::new();
    for t in candidates {
        threads.insert(live(w, t)?);
    }

    let target = if threads.is_empty() {
        w.conn.execute(
            "INSERT INTO thread (account_id, subject_norm, last_date) VALUES (?1, ?2, ?3)",
            params![w.account, input.subject_norm, input.date],
        )?;
        w.conn.last_insert_rowid()
    } else {
        let mut best = (i64::MIN, 0);
        for &t in &threads {
            let size = message_count(w, t)?;
            if size > best.0 {
                best = (size, t);
            }
        }
        let target = best.1;
        for &t in &threads {
            if t != target {
                merge(w, t, target)?;
            }
        }
        target
    };
    let mut stmt = w.conn.prepare_cached(
        "INSERT INTO thread_ref (account_id, msgid_hdr, thread_id) VALUES (?1, ?2, ?3) \
         ON CONFLICT (account_id, msgid_hdr) DO UPDATE SET thread_id = excluded.thread_id",
    )?;
    for id in &ids {
        stmt.execute(params![w.account, id, target])?;
    }
    w.conn.execute(
        "UPDATE message SET thread_id = ?2 WHERE id = ?1",
        params![message, target],
    )?;
    w.touch_thread(target);
    Ok(target)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;

    use crate::testutil::{Msg, TestCache, msg, open};

    type Partition = BTreeSet<BTreeSet<String>>;

    async fn partition(t: &TestCache) -> Partition {
        let rows = t
            .query("SELECT email_id, thread_id FROM message", |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .await;
        let mut by_thread: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
        for (email, thread) in rows {
            by_thread
                .entry(thread)
                .or_default()
                .insert(email.trim_start_matches("o:").to_owned());
        }
        by_thread.into_values().collect()
    }

    fn expect(groups: &[&[&str]]) -> Partition {
        groups
            .iter()
            .map(|g| g.iter().map(|s| (*s).to_owned()).collect())
            .collect()
    }

    /// AC 7 corpus: reply chains, missing References, Outlook replies with
    /// only `Thread-Index` (no ids, subject fallback), subject changes, late
    /// parents, cross-folder Sent replies, unrelated same-subject mail and a
    /// bridge across the 14-day window.
    fn corpus() -> Vec<(usize, Msg)> {
        vec![
            (0, msg("a1", "Lunch", "ann@x", "bob@x", 0)),
            (0, msg("a2", "Re: Lunch", "bob@x", "ann@x", 1).refs(&["a1"])),
            (
                1,
                msg("a3", "Re: Lunch", "ann@x", "bob@x", 2).refs(&["a1", "a2"]),
            ),
            (0, msg("b1", "Invoice", "shop@y", "ann@x", 3)),
            (
                0,
                msg("b2", "Re: Invoice", "ann@x", "shop@y", 4)
                    .refs(&["b1"])
                    .no_msgid(),
            ),
            (0, msg("c1", "Budget 2027", "cfo@z", "ann@x", 5)),
            (
                0,
                msg("c2", "RE: Budget 2027", "ann@x", "cfo@z", 6).no_msgid(),
            ),
            (
                0,
                msg("c3", "AW: Budget 2027", "cfo@z", "ann@x", 7).no_msgid(),
            ),
            (0, msg("d1", "Plans", "eve@w", "ann@x", 8)),
            (
                0,
                msg("d2", "Totally new topic", "ann@x", "eve@w", 9).refs(&["d1"]),
            ),
            (
                0,
                msg("e2", "Re: Party", "fred@v", "ann@x", 11).refs(&["e1"]),
            ),
            (
                0,
                msg("e3", "Re: Party", "ann@x", "fred@v", 12).refs(&["e1", "e2"]),
            ),
            (0, msg("e1", "Party", "ann@x", "fred@v", 10)),
            (0, msg("f1", "Hello", "zed@q", "yan@q", 0)),
            (0, msg("f2", "Hello", "kim@r", "lou@r", 1)),
            (0, msg("g1", "Weekly", "boss@s", "ann@x", 0)),
            (0, msg("g3", "Weekly", "boss@s", "ann@x", 20)),
            (0, msg("g2", "Re: Weekly", "ann@x", "boss@s", 10)),
            (0, msg("h1", "Status", "ops@t", "ann@x", 0)),
            (0, msg("h2", "Status", "ops@t", "ann@x", 40)),
        ]
    }

    fn expected() -> Partition {
        expect(&[
            &["a1", "a2", "a3"],
            &["b1", "b2"],
            &["c1", "c2", "c3"],
            &["d1", "d2"],
            &["e1", "e2", "e3"],
            &["f1"],
            &["f2"],
            &["g1", "g2", "g3"],
            &["h1"],
            &["h2"],
        ])
    }

    async fn insert(order: &[usize]) -> Partition {
        let t = open();
        let mbs = t.mailboxes(&["INBOX", "Sent"]).await;
        let corpus = corpus();
        for (uid, &i) in order.iter().enumerate() {
            let (mailbox, m) = &corpus[i];
            t.add(mbs[*mailbox], vec![m.fetched(uid as u32 + 1)]).await;
        }
        partition(&t).await
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn corpus_threads_as_expected_in_order_and_newest_first() {
        let rt = runtime();
        let n = corpus().len();
        let forward: Vec<usize> = (0..n).collect();
        assert_eq!(rt.block_on(insert(&forward)), expected());
        let mut by_date: Vec<usize> = (0..n).collect();
        by_date.sort_by_key(|&i| std::cmp::Reverse(corpus()[i].1.day));
        assert_eq!(rt.block_on(insert(&by_date)), expected());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn partition_is_independent_of_arrival_order(
            order in Just((0..corpus().len()).collect::<Vec<_>>()).prop_shuffle()
        ) {
            prop_assert_eq!(runtime().block_on(insert(&order)), expected());
        }
    }

    #[tokio::test]
    async fn merging_emits_merged_and_leaves_no_list_rows() {
        let t = open();
        let inbox = t.mailboxes(&["INBOX"]).await[0];
        t.add(inbox, vec![msg("x1", "One", "a@x", "b@x", 0).fetched(1)])
            .await;
        t.add(inbox, vec![msg("y1", "Two", "c@x", "d@x", 0).fetched(2)])
            .await;
        t.add(
            inbox,
            vec![
                msg("z1", "Re: both", "a@x", "c@x", 1)
                    .refs(&["x1", "y1"])
                    .fetched(3),
            ],
        )
        .await;
        let merged = t
            .query(
                "SELECT entity_id, op FROM change_log WHERE op LIKE 'merged:%'",
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .await;
        assert_eq!(merged.len(), 1, "{merged:?}");
        let (from, op) = &merged[0];
        let target: i64 = op.trim_start_matches("merged:").parse().unwrap();
        let rows = t
            .query("SELECT thread_id FROM thread_mailbox", |r| {
                r.get::<_, i64>(0)
            })
            .await;
        assert_eq!(rows, [target]);
        assert!(!rows.contains(from));
        let counts = t
            .query(
                "SELECT id, msg_count, merged_into FROM thread ORDER BY id",
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .await;
        assert!(counts.contains(&(target, 3, None)), "{counts:?}");
        assert!(counts.contains(&(*from, 0, Some(target))), "{counts:?}");
    }
}
