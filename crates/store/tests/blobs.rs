use std::io::Read;
use std::time::{Duration, SystemTime};

use tagmail_core::{AccountId, EmailId};
use tagmail_store::{BlobKey, BlobStore, Error, TEXT_CAP};
use tempfile::TempDir;

fn key(n: u64) -> BlobKey {
    BlobKey::for_message(AccountId(1), &EmailId::from_gmail_msgid(n))
}

fn read_raw(store: &BlobStore, key: &BlobKey) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    store.open_raw(key).unwrap()?.read_to_end(&mut out).unwrap();
    Some(out)
}

fn raw_path(root: &std::path::Path, key: &BlobKey) -> std::path::PathBuf {
    root.join("raw")
        .join(&key.as_str()[..2])
        .join(format!("{}.zst", key.as_str()))
}

#[test]
fn keys_are_stable_and_validated() {
    let a = key(1);
    assert_eq!(a, key(1));
    assert_ne!(a, key(2));
    assert_ne!(
        a,
        BlobKey::for_message(AccountId(2), &EmailId::from_gmail_msgid(1))
    );
    assert_eq!(BlobKey::parse(a.as_str()).unwrap(), a);
    assert!(BlobKey::parse("../etc/passwd").is_err());
    assert!(BlobKey::parse(&a.as_str().to_uppercase()).is_err());
}

#[test]
fn raw_blob_streams_chunks_and_compresses() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::open(dir.path()).unwrap();
    let message = b"Subject: hi\r\n\r\n".repeat(10_000);
    let mut writer = store.raw_writer(key(1), u64::MAX).unwrap();
    for chunk in message.chunks(4096) {
        writer.write_chunk(chunk).unwrap();
    }
    assert!(
        read_raw(&store, &key(1)).is_none(),
        "invisible until finished"
    );
    let k = writer.finish().unwrap();
    assert_eq!(read_raw(&store, &k).unwrap(), message);
    let stored = std::fs::metadata(raw_path(dir.path(), &k)).unwrap().len();
    assert!(stored < message.len() as u64 / 10, "{stored} bytes stored");
}

#[test]
fn raw_blob_over_cap_is_abandoned() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::open(dir.path()).unwrap();
    let mut writer = store.raw_writer(key(1), 10).unwrap();
    writer.write_chunk(b"0123456789").unwrap();
    assert!(matches!(
        writer.write_chunk(b"x"),
        Err(Error::BlobTooLarge { cap: 10 })
    ));
    drop(writer);
    assert!(read_raw(&store, &key(1)).is_none());
    assert_eq!(
        std::fs::read_dir(dir.path().join("tmp")).unwrap().count(),
        0
    );
}

#[test]
fn text_is_capped_on_a_char_boundary() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::open(dir.path()).unwrap();
    assert!(!store.put_text(&key(1), "short text").unwrap());
    assert_eq!(store.read_text(&key(1)).unwrap().unwrap(), "short text");

    let long = "é".repeat(TEXT_CAP);
    assert!(store.put_text(&key(2), &long).unwrap());
    let kept = store.read_text(&key(2)).unwrap().unwrap();
    assert_eq!(kept.len(), TEXT_CAP);
    assert!(long.starts_with(&kept));

    store.delete_text(&key(1)).unwrap();
    assert!(store.read_text(&key(1)).unwrap().is_none());
    store.delete_text(&key(1)).unwrap();
}

#[test]
fn compressed_text_moves_between_stores_unchanged() {
    let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let (server, replica) = (
        BlobStore::open(a.path()).unwrap(),
        BlobStore::open(b.path()).unwrap(),
    );
    assert!(server.read_text_zstd(&key(1)).unwrap().is_none());
    server.put_text(&key(1), "héllo").unwrap();
    let shipped = server.read_text_zstd(&key(1)).unwrap().unwrap();
    replica.put_text_zstd(&key(1), &shipped).unwrap();
    assert_eq!(replica.read_text(&key(1)).unwrap().unwrap(), "héllo");
    assert_eq!(replica.read_text_zstd(&key(1)).unwrap().unwrap(), shipped);

    assert!(matches!(
        replica.put_text_zstd(&key(2), b"not zstd"),
        Err(Error::Io(_) | Error::Corrupt(_))
    ));
    let huge = zstd::encode_all("x".repeat(TEXT_CAP + 1).as_bytes(), 3).unwrap();
    assert!(matches!(
        replica.put_text_zstd(&key(3), &huge),
        Err(Error::Corrupt(_))
    ));
    assert!(replica.read_text(&key(3)).unwrap().is_none());
}

#[test]
fn compressed_text_with_a_huge_window_is_refused() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::open(dir.path()).unwrap();
    let mut encoder = zstd::Encoder::new(Vec::new(), 3).unwrap();
    encoder.window_log(27).unwrap();
    encoder.include_contentsize(false).unwrap();
    std::io::Write::write_all(&mut encoder, b"tiny").unwrap();
    let huge_window = encoder.finish().unwrap();
    assert!(matches!(
        store.put_text_zstd(&key(1), &huge_window),
        Err(Error::Io(_) | Error::Corrupt(_))
    ));

    let text: String = (0..TEXT_CAP)
        .map(|i| char::from(b'a' + (i * 7 % 26) as u8))
        .collect();
    store.put_text(&key(2), &text).unwrap();
    let shipped = store.read_text_zstd(&key(2)).unwrap().unwrap();
    assert_eq!(store.put_text_zstd(&key(3), &shipped).unwrap(), text);
    assert_eq!(store.read_text(&key(3)).unwrap().unwrap(), text);
}

#[test]
fn eviction_removes_least_recently_used_raw_only() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::open(dir.path()).unwrap();
    let base = SystemTime::now() - Duration::from_secs(3600);
    for n in 0..4u64 {
        let mut w = store.raw_writer(key(n), u64::MAX).unwrap();
        w.write_chunk(&n.to_be_bytes().repeat(1000)).unwrap();
        w.finish().unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(raw_path(dir.path(), &key(n)))
            .unwrap();
        file.set_modified(base + Duration::from_secs(n * 60))
            .unwrap();
    }
    store.put_text(&key(0), "kept forever").unwrap();
    read_raw(&store, &key(0)).unwrap();

    let size = |n| {
        std::fs::metadata(raw_path(dir.path(), &key(n)))
            .unwrap()
            .len()
    };
    let evicted = size(1) + size(2);
    let freed = store.evict_raw(size(0) + size(3)).unwrap();
    assert_eq!(freed, evicted);
    assert!(read_raw(&store, &key(1)).is_none());
    assert!(read_raw(&store, &key(2)).is_none());
    assert!(read_raw(&store, &key(3)).is_some());
    assert!(read_raw(&store, &key(0)).is_some(), "recently read");
    assert_eq!(store.read_text(&key(0)).unwrap().unwrap(), "kept forever");
}

#[test]
fn reopen_discards_unfinished_writes_and_wipe_removes_all() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("blobs");
    let store = BlobStore::open(&root).unwrap();
    store.put_text(&key(1), "text").unwrap();
    let mut writer = store.raw_writer(key(2), u64::MAX).unwrap();
    writer.write_chunk(b"partial").unwrap();
    std::mem::forget(writer);

    let store = BlobStore::open(&root).unwrap();
    assert_eq!(std::fs::read_dir(root.join("tmp")).unwrap().count(), 0);
    assert_eq!(store.read_text(&key(1)).unwrap().unwrap(), "text");

    BlobStore::wipe(&root).unwrap();
    assert!(!root.exists());
    BlobStore::wipe(&root).unwrap();
}
