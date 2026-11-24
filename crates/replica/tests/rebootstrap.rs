//! A re-bootstrap while the SPA holds the old store (an open event
//! session): the replica keeps serving and swaps without waiting.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::Fixture;

fn replica_dirs(root: &std::path::Path) -> usize {
    std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .count()
}

#[tokio::test]
async fn a_rebootstrap_with_an_open_session_never_leaves_the_replica_empty() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    let session = replica.store().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let watcher = tokio::spawn({
        let replica = Arc::clone(&replica);
        let done = Arc::clone(&done);
        async move {
            let mut empty = 0;
            while !done.load(Ordering::Acquire) {
                if replica.store().is_none() {
                    empty += 1;
                }
                tokio::task::yield_now().await;
            }
            empty
        }
    });

    let started = Instant::now();
    replica.rebootstrap().await.unwrap();
    let took = started.elapsed();
    done.store(true, Ordering::Release);

    assert_eq!(watcher.await.unwrap(), 0, "the replica was empty");
    assert!(took < Duration::from_secs(3), "took {took:?}");
    assert!(!Arc::ptr_eq(&session, &replica.store().unwrap()));
    assert_eq!(
        replica_dirs(f.desktop.path()),
        2,
        "the old one is still open"
    );

    drop(session);
    let deadline = Instant::now() + Duration::from_secs(10);
    while replica_dirs(f.desktop.path()) > 1 {
        assert!(Instant::now() < deadline, "the old replica stays");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
