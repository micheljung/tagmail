use tagmail_core::{CacheEpoch, ChangeCursor, ChangeFilter};
use tagmail_store::changes::{append, append_all};
use tagmail_store::{CacheDb, Change, ChangeOp, Changes, Entity, NewChange};
use tempfile::TempDir;

fn thread(id: i64, op: ChangeOp) -> NewChange {
    NewChange {
        entity: Entity::Thread,
        entity_id: id,
        op,
        payload: None,
    }
}

const MAIL: ChangeFilter = ChangeFilter::Mail;

fn event(id: i64) -> NewChange {
    NewChange {
        entity: Entity::Event,
        ..thread(id, ChangeOp::Upsert)
    }
}

fn page(changes: Changes) -> (Vec<Change>, ChangeCursor, bool) {
    match changes {
        Changes::Page {
            changes,
            next,
            more,
        } => (changes, next, more),
        Changes::Gone => panic!("unexpected Gone"),
    }
}

#[tokio::test]
async fn appended_changes_page_in_order() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    let start = db.head().await.unwrap();
    assert_eq!(start.seq, 0);
    assert_eq!(start.epoch, db.epoch());

    let batch = vec![
        thread(1, ChangeOp::Upsert),
        NewChange {
            payload: Some(r#"{"last_date":1700000000}"#.into()),
            ..thread(2, ChangeOp::Upsert)
        },
        thread(3, ChangeOp::Merged { target: 2 }),
        NewChange {
            entity: Entity::Mailbox,
            ..thread(5, ChangeOp::Bulk { mailbox: 5 })
        },
        thread(1, ChangeOp::Delete),
    ];
    let last = db
        .write(move |tx| {
            append(tx, &thread(9, ChangeOp::Upsert))?;
            append_all(tx, &batch)
        })
        .await
        .unwrap();
    assert_eq!(last, Some(6));
    assert_eq!(db.head().await.unwrap().seq, 6);

    let (first, next, more) = page(db.changes_since(start, 4, MAIL).await.unwrap());
    assert!(more);
    assert_eq!(
        first.iter().map(|c| c.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        first[2].payload.as_deref(),
        Some(r#"{"last_date":1700000000}"#)
    );
    assert_eq!(first[3].op, ChangeOp::Merged { target: 2 });
    assert_eq!(next.seq, 4);

    let (rest, next, more) = page(db.changes_since(next, 4, MAIL).await.unwrap());
    assert!(!more);
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0].entity, Entity::Mailbox);
    assert_eq!(rest[0].op, ChangeOp::Bulk { mailbox: 5 });
    assert_eq!(rest[1].op, ChangeOp::Delete);

    let (none, same, more) = page(db.changes_since(next, 4, MAIL).await.unwrap());
    assert!(none.is_empty() && !more);
    assert_eq!(same, next);
}

#[tokio::test]
async fn failed_write_appends_nothing() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    let result: tagmail_store::Result<()> = db
        .write(|tx| {
            append(tx, &thread(1, ChangeOp::Upsert))?;
            Err(tagmail_store::Error::Corrupt("abort".into()))
        })
        .await;
    assert!(result.is_err());
    assert_eq!(db.head().await.unwrap().seq, 0);
}

#[tokio::test]
async fn foreign_or_future_cursors_are_gone() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    db.write(|tx| append(tx, &thread(1, ChangeOp::Upsert)))
        .await
        .unwrap();
    let other = ChangeCursor {
        epoch: CacheEpoch(db.epoch().0.wrapping_add(1)),
        seq: 0,
    };
    assert_eq!(
        db.changes_since(other, 10, MAIL).await.unwrap(),
        Changes::Gone
    );
    let future = ChangeCursor {
        epoch: db.epoch(),
        seq: 2,
    };
    assert_eq!(
        db.changes_since(future, 10, MAIL).await.unwrap(),
        Changes::Gone
    );
}

#[tokio::test]
async fn cache_wipe_starts_a_new_epoch() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("cache.db");
    let db = CacheDb::open(&path).unwrap();
    db.write(|tx| append(tx, &thread(1, ChangeOp::Upsert)))
        .await
        .unwrap();
    let cursor = db.head().await.unwrap();
    db.close().await.unwrap();

    CacheDb::wipe(&path).unwrap();
    assert!(!path.exists());
    CacheDb::wipe(&path).unwrap();

    let db = CacheDb::open(&path).unwrap();
    assert_ne!(db.epoch(), cursor.epoch);
    assert_eq!(
        db.changes_since(cursor, 10, MAIL).await.unwrap(),
        Changes::Gone
    );
    db.write(|tx| append(tx, &thread(1, ChangeOp::Upsert)))
        .await
        .unwrap();
    let same_seq = ChangeCursor {
        epoch: cursor.epoch,
        seq: 1,
    };
    assert_eq!(
        db.changes_since(same_seq, 10, MAIL).await.unwrap(),
        Changes::Gone
    );
}

#[tokio::test]
async fn trim_raises_the_floor() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    db.write(|tx| {
        for id in 1..=10 {
            append(tx, &thread(id, ChangeOp::Upsert))?;
        }
        tx.execute("UPDATE change_log SET created_at = 100 WHERE seq <= 3", [])?;
        tx.execute("UPDATE change_log SET created_at = 200 WHERE seq > 3", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let at = |seq| ChangeCursor {
        epoch: db.epoch(),
        seq,
    };

    assert_eq!(db.trim_changes(150, 1_000).await.unwrap(), 3, "by age");
    assert_eq!(
        db.changes_since(at(2), 100, MAIL).await.unwrap(),
        Changes::Gone
    );
    let (rest, _, _) = page(db.changes_since(at(3), 100, MAIL).await.unwrap());
    assert_eq!(rest.first().map(|c| c.seq), Some(4));

    assert_eq!(db.trim_changes(150, 4).await.unwrap(), 6, "by count");
    assert_eq!(
        db.changes_since(at(5), 100, MAIL).await.unwrap(),
        Changes::Gone
    );
    let (rest, _, _) = page(db.changes_since(at(6), 100, MAIL).await.unwrap());
    assert_eq!(rest.len(), 4);

    assert_eq!(
        db.trim_changes(0, 1_000).await.unwrap(),
        6,
        "floor never drops"
    );

    assert_eq!(db.trim_changes(i64::MAX, 0).await.unwrap(), 10, "trim all");
    assert_eq!(db.head().await.unwrap().seq, 10, "seqs are never reused");
    db.write(|tx| append(tx, &thread(1, ChangeOp::Upsert)))
        .await
        .unwrap();
    let (rest, _, _) = page(db.changes_since(at(10), 100, MAIL).await.unwrap());
    assert_eq!(rest[0].seq, 11);
}

#[tokio::test]
async fn changes_filter_applies_before_limit() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    let start = db.head().await.unwrap();
    db.write(|tx| {
        append(tx, &thread(1, ChangeOp::Upsert))?;
        append_all(tx, &[event(1), event(2), event(3)])?;
        append(tx, &thread(2, ChangeOp::Upsert))?;
        append(tx, &thread(3, ChangeOp::Upsert))
    })
    .await
    .unwrap();

    let (mail, next, more) = page(db.changes_since(start, 2, MAIL).await.unwrap());
    assert_eq!(mail.iter().map(|c| c.seq).collect::<Vec<_>>(), [1, 5]);
    assert!(more);
    assert_eq!(next.seq, 5);

    let all = ChangeFilter::WithCalendar;
    let (with_cal, next, more) = page(db.changes_since(start, 2, all).await.unwrap());
    assert_eq!(with_cal.iter().map(|c| c.seq).collect::<Vec<_>>(), [1, 2]);
    assert!(more);
    assert_eq!(next.seq, 2);
}

#[tokio::test]
async fn filtered_calendar_only_tail_returns_next_at_head() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    let start = db.head().await.unwrap();
    db.write(|tx| {
        append(tx, &thread(1, ChangeOp::Upsert))?;
        append_all(tx, &[event(1), event(2)])
    })
    .await
    .unwrap();

    let (mail, next, more) = page(db.changes_since(start, 10, MAIL).await.unwrap());
    assert_eq!(mail.len(), 1);
    assert!(!more);
    assert_eq!(next, db.head().await.unwrap());

    let (none, again, more) = page(db.changes_since(next, 10, MAIL).await.unwrap());
    assert!(none.is_empty() && !more);
    assert_eq!(again, next);

    let behind = ChangeCursor { seq: 1, ..start };
    let (none, next, _) = page(db.changes_since(behind, 10, MAIL).await.unwrap());
    assert!(none.is_empty());
    assert_eq!(next.seq, 3, "an empty filtered page still reaches the head");
}

#[tokio::test]
async fn trim_after_calendar_only_churn_does_not_gone_a_mail_only_cursor() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    let start = db.head().await.unwrap();
    db.write(|tx| append(tx, &thread(1, ChangeOp::Upsert)))
        .await
        .unwrap();
    let (_, cursor, _) = page(db.changes_since(start, 10, MAIL).await.unwrap());

    let mut cursor = cursor;
    for round in 0..3 {
        db.write(move |tx| append_all(tx, &[event(round), event(round + 10)]))
            .await
            .unwrap();
        let (none, next, _) = page(db.changes_since(cursor, 10, MAIL).await.unwrap());
        assert!(none.is_empty());
        cursor = next;
        db.trim_changes(i64::MAX, 0).await.unwrap();
    }
    assert_eq!(cursor, db.head().await.unwrap());
    assert!(matches!(
        db.changes_since(cursor, 10, MAIL).await.unwrap(),
        Changes::Page { .. }
    ));
}
