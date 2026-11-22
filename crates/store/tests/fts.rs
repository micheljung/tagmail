use tagmail_core::MessageRowId;
use tagmail_store::rusqlite::Connection;
use tagmail_store::{CacheDb, FtsDocument, fts};
use tempfile::TempDir;

fn doc(subject: &'static str, body: &'static str) -> FtsDocument<'static> {
    FtsDocument {
        subject,
        from: "alice@example.com Alice",
        to: "bob@example.com Bob",
        body,
    }
}

async fn search(db: &CacheDb, expr: &'static str) -> Vec<i64> {
    db.read(move |c| fts::search(c, expr, 100))
        .await
        .unwrap()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

#[tokio::test]
async fn phrase_queries_match_word_order() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    db.write(|tx| {
        fts::index(tx, MessageRowId(1), &doc("Invoice", "the quick brown fox"))?;
        fts::index(tx, MessageRowId(2), &doc("Report", "brown quick the fox"))?;
        fts::index(tx, MessageRowId(3), &doc("Résumé", "Quick, brown! Fox."))
    })
    .await
    .unwrap();

    assert_eq!(search(&db, "quick brown").await, [3, 2, 1]);
    assert_eq!(search(&db, "\"quick brown\"").await, [3, 1]);
    assert_eq!(search(&db, "NEAR(the fox, 0)").await, [2]);
    assert_eq!(search(&db, "subject:invoice").await, [1]);
    assert_eq!(search(&db, "resume").await, [3], "diacritics removed");
    assert_eq!(search(&db, "alice").await, [3, 2, 1]);
}

#[test]
fn phrase_queries_fail_without_full_detail() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE message_fts USING fts5 (body, content = '', contentless_delete = 1, detail = column);
         INSERT INTO message_fts (rowid, body) VALUES (1, 'quick brown fox');",
    )
    .unwrap();
    assert!(fts::search(&conn, "quick", 10).is_ok());
    assert!(fts::search(&conn, "\"quick brown\"", 10).is_err());
}

#[tokio::test]
async fn delete_and_reindex_replace_rows() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    db.write(|tx| {
        fts::index(tx, MessageRowId(1), &doc("a", "alpha"))?;
        fts::index(tx, MessageRowId(2), &doc("b", "alpha beta"))
    })
    .await
    .unwrap();
    db.write(|tx| {
        fts::delete(tx, MessageRowId(2))?;
        fts::index(tx, MessageRowId(1), &doc("a", "gamma"))
    })
    .await
    .unwrap();
    assert_eq!(search(&db, "alpha").await, Vec::<i64>::new());
    assert_eq!(search(&db, "beta").await, Vec::<i64>::new());
    assert_eq!(search(&db, "gamma").await, [1]);
}

#[tokio::test]
async fn search_respects_limit() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    db.write(|tx| {
        for id in 1..=5 {
            fts::index(tx, MessageRowId(id), &doc("s", "common"))?;
        }
        Ok(())
    })
    .await
    .unwrap();
    let hits = db.read(|c| fts::search(c, "common", 2)).await.unwrap();
    assert_eq!(hits, [MessageRowId(5), MessageRowId(4)]);
}
