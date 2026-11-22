use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use tagmail_store::rusqlite::{Connection, params};
use tagmail_store::{CacheDb, ConfigDb, Error};
use tempfile::TempDir;

const WRITERS: usize = 64;

/// Tracks how many closures run at once.
#[derive(Default)]
struct InFlight {
    now: AtomicUsize,
    max: AtomicUsize,
}

impl InFlight {
    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1));
        let out = f();
        self.now.fetch_sub(1, Ordering::SeqCst);
        out
    }
}

fn counter(conn: &Connection) -> tagmail_store::Result<i64> {
    let value: String =
        conn.query_row("SELECT value FROM meta WHERE key = 'counter'", [], |r| {
            r.get(0)
        })?;
    value
        .parse()
        .map_err(|_| Error::Corrupt(format!("counter {value}")))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writer_serializes_concurrent_writes() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(ConfigDb::open(&dir.path().join("config.db")).unwrap());
    db.write(|tx| Ok(tx.execute("INSERT INTO meta VALUES ('counter', '0')", [])?))
        .await
        .unwrap();

    let in_flight = Arc::new(InFlight::default());
    let tasks: Vec<_> = (0..WRITERS)
        .map(|_| {
            let db = db.clone();
            let in_flight = in_flight.clone();
            tokio::spawn(async move {
                db.write(move |tx| {
                    in_flight.run(|| {
                        let next = counter(tx)? + 1;
                        tx.execute(
                            "UPDATE meta SET value = ?1 WHERE key = 'counter'",
                            params![next.to_string()],
                        )?;
                        Ok(())
                    })
                })
                .await
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }

    assert_eq!(in_flight.max.load(Ordering::SeqCst), 1);
    assert_eq!(db.read(counter).await.unwrap(), WRITERS as i64);
}

#[tokio::test]
async fn writes_from_one_task_apply_in_order() {
    let dir = TempDir::new().unwrap();
    let db = ConfigDb::open(&dir.path().join("config.db")).unwrap();
    for i in 0..10 {
        db.write(move |tx| {
            Ok(tx.execute(
                "INSERT INTO meta VALUES ('counter', ?1) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![i.to_string()],
            )?)
        })
        .await
        .unwrap();
    }
    assert_eq!(db.read(counter).await.unwrap(), 9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_has_two_concurrent_readers() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(CacheDb::open(&dir.path().join("cache.db")).unwrap());
    let barrier = Arc::new(Barrier::new(2));
    let read = |db: Arc<CacheDb>, barrier: Arc<Barrier>| {
        tokio::spawn(async move {
            db.read(move |_| {
                barrier.wait();
                Ok(())
            })
            .await
        })
    };
    let a = read(db.clone(), barrier.clone());
    let b = read(db.clone(), barrier.clone());
    let both = async { (a.await.unwrap(), b.await.unwrap()) };
    let (a, b) = tokio::time::timeout(Duration::from_secs(10), both)
        .await
        .expect("two reads ran concurrently");
    a.unwrap();
    b.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_reads_queue_for_one_connection() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(ConfigDb::open(&dir.path().join("config.db")).unwrap());
    let in_flight = Arc::new(InFlight::default());
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            let in_flight = in_flight.clone();
            tokio::spawn(async move { db.read(move |_| in_flight.run(|| Ok(()))).await })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(in_flight.max.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reader_panic_is_reported_and_pool_recovers() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    let panicked: tagmail_store::Result<()> = db.read(|_| panic!("reader bug")).await;
    assert!(matches!(panicked, Err(Error::ReaderPanicked)));
    for _ in 0..4 {
        db.read(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn readers_see_committed_writes() {
    let dir = TempDir::new().unwrap();
    let db = ConfigDb::open(&dir.path().join("config.db")).unwrap();
    db.write(|tx| Ok(tx.execute("INSERT INTO meta VALUES ('counter', '41')", [])?))
        .await
        .unwrap();
    assert_eq!(db.read(counter).await.unwrap(), 41);
    db.close().await.unwrap();
}
