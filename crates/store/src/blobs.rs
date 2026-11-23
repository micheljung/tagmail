//! On-disk message blobs, zstd-compressed (plan §3.2):
//! `raw/ab/<key>.zst` holds RFC 822 messages, evicted least-recently-used
//! under a size cap and re-fetched on demand; `text/ab/<key>.zst` holds the
//! plain text (≤ [`TEXT_CAP`]) that feeds FTS rebuilds, highlighting and the
//! replica, and is never evicted. All calls block; run them off the async
//! runtime.

use std::fs::{self, File};
use std::io::{BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};
use tagmail_core::{AccountId, EmailId};
use tempfile::NamedTempFile;

use crate::error::{Error, Result};

/// Plain text kept per message.
pub const TEXT_CAP: usize = 64 * 1024;

/// Raw message size beyond which a fetch is abandoned (plan §3.4 step 6).
pub const DEFAULT_RAW_CAP: u64 = 50 * 1024 * 1024;

const ZSTD_LEVEL: i32 = 3;
const KEY_HEX_LEN: usize = 32;
const EXTENSION: &str = "zst";

/// File name of a message's blobs, derived from IMAP-stable identity so it
/// is the same after a cache rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobKey(String);

impl BlobKey {
    pub fn for_message(account: AccountId, email_id: &EmailId) -> BlobKey {
        let mut hasher = Sha256::new();
        hasher.update(account.0.to_be_bytes());
        hasher.update(email_id.as_str().as_bytes());
        BlobKey(hex::encode(&hasher.finalize()[..KEY_HEX_LEN / 2]))
    }

    /// Validates a key read back from `message.raw_blob` / `text_blob`.
    pub fn parse(stored: &str) -> Result<BlobKey> {
        let valid = stored.len() == KEY_HEX_LEN
            && stored
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        valid
            .then(|| BlobKey(stored.to_owned()))
            .ok_or_else(|| Error::Corrupt(format!("blob key {stored:?}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Raw,
    Text,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Raw => "raw",
            Kind::Text => "text",
        }
    }
}

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Creates the layout under `root` and discards unfinished writes.
    pub fn open(root: &Path) -> Result<BlobStore> {
        let tmp = root.join("tmp");
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        for dir in [Kind::Raw.dir(), Kind::Text.dir(), "tmp"] {
            fs::create_dir_all(root.join(dir))?;
        }
        Ok(BlobStore {
            root: root.to_owned(),
        })
    }

    /// Deletes all blobs; part of a cache wipe.
    pub fn wipe(root: &Path) -> Result<()> {
        match fs::remove_dir_all(root) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn path(&self, kind: Kind, key: &BlobKey) -> PathBuf {
        self.root
            .join(kind.dir())
            .join(&key.0[..2])
            .join(format!("{}.{EXTENSION}", key.0))
    }

    fn temp(&self) -> Result<NamedTempFile> {
        Ok(NamedTempFile::new_in(self.root.join("tmp"))?)
    }

    fn persist(&self, file: NamedTempFile, kind: Kind, key: &BlobKey) -> Result<()> {
        let path = self.path(kind, key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        file.persist(&path).map_err(|e| e.error)?;
        Ok(())
    }

    fn open_file(&self, kind: Kind, key: &BlobKey) -> Result<Option<File>> {
        match File::open(self.path(kind, key)) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete(&self, kind: Kind, key: &BlobKey) -> Result<()> {
        match fs::remove_file(self.path(kind, key)) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Streams a raw message in chunks; it becomes visible on
    /// [`RawWriter::finish`], replacing any previous blob of the key.
    pub fn raw_writer(&self, key: BlobKey, cap: u64) -> Result<RawWriter<'_>> {
        let mut encoder = zstd::Encoder::new(self.temp()?, ZSTD_LEVEL)?;
        encoder.include_checksum(true)?;
        Ok(RawWriter {
            store: self,
            key,
            encoder,
            written: 0,
            cap,
        })
    }

    /// A decompressing reader, or `None` if the blob is absent (never fetched
    /// or evicted). Marks the blob recently used.
    pub fn open_raw(&self, key: &BlobKey) -> Result<Option<impl Read + use<>>> {
        let Some(file) = self.open_file(Kind::Raw, key)? else {
            return Ok(None);
        };
        file.set_modified(SystemTime::now())?;
        Ok(Some(zstd::Decoder::with_buffer(BufReader::new(file))?))
    }

    pub fn delete_raw(&self, key: &BlobKey) -> Result<()> {
        self.delete(Kind::Raw, key)
    }

    /// Deletes least recently used raw blobs until their compressed total is
    /// at most `max_bytes`. Returns the bytes freed.
    pub fn evict_raw(&self, max_bytes: u64) -> Result<u64> {
        let mut blobs = Vec::new();
        for shard in fs::read_dir(self.root.join(Kind::Raw.dir()))? {
            for entry in fs::read_dir(shard?.path())? {
                let entry = entry?;
                let meta = entry.metadata()?;
                blobs.push((meta.modified()?, meta.len(), entry.path()));
            }
        }
        let mut total: u64 = blobs.iter().map(|(_, len, _)| len).sum();
        blobs.sort();
        let mut freed = 0;
        for (_, len, path) in blobs {
            if total <= max_bytes {
                break;
            }
            match fs::remove_file(&path) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            total -= len;
            freed += len;
        }
        Ok(freed)
    }

    /// Stores a message's plain text, cut at [`TEXT_CAP`] bytes on a char
    /// boundary. Returns whether it was truncated.
    pub fn put_text(&self, key: &BlobKey, text: &str) -> Result<bool> {
        let kept = &text[..text.floor_char_boundary(TEXT_CAP)];
        let mut encoder = zstd::Encoder::new(self.temp()?, ZSTD_LEVEL)?;
        encoder.include_checksum(true)?;
        encoder.write_all(kept.as_bytes())?;
        self.persist(encoder.finish()?, Kind::Text, key)?;
        Ok(kept.len() < text.len())
    }

    pub fn read_text(&self, key: &BlobKey) -> Result<Option<String>> {
        let Some(file) = self.open_file(Kind::Text, key)? else {
            return Ok(None);
        };
        let mut text = String::new();
        zstd::Decoder::new(file)?
            .take(TEXT_CAP as u64 + 1)
            .read_to_string(&mut text)?;
        if text.len() > TEXT_CAP {
            return Err(Error::Corrupt(format!("text blob {} over cap", key.0)));
        }
        Ok(Some(text))
    }

    /// The stored text blob as is (zstd), for shipping to a replica.
    pub fn read_text_zstd(&self, key: &BlobKey) -> Result<Option<Vec<u8>>> {
        let Some(mut file) = self.open_file(Kind::Text, key)? else {
            return Ok(None);
        };
        let mut compressed = Vec::new();
        file.read_to_end(&mut compressed)?;
        Ok(Some(compressed))
    }

    /// Stores a text blob from [`BlobStore::read_text_zstd`] of another
    /// store; refused unless it decodes within [`TEXT_CAP`].
    pub fn put_text_zstd(&self, key: &BlobKey, compressed: &[u8]) -> Result<()> {
        let mut text = Vec::new();
        zstd::Decoder::new(compressed)?
            .take(TEXT_CAP as u64 + 1)
            .read_to_end(&mut text)?;
        if text.len() > TEXT_CAP || std::str::from_utf8(&text).is_err() {
            return Err(Error::Corrupt(format!("text blob {} is not text", key.0)));
        }
        let mut file = self.temp()?;
        file.write_all(compressed)?;
        self.persist(file, Kind::Text, key)
    }

    pub fn delete_text(&self, key: &BlobKey) -> Result<()> {
        self.delete(Kind::Text, key)
    }
}

pub struct RawWriter<'a> {
    store: &'a BlobStore,
    key: BlobKey,
    encoder: zstd::Encoder<'static, NamedTempFile>,
    written: u64,
    cap: u64,
}

impl RawWriter<'_> {
    /// Appends a chunk; past the cap the blob is abandoned with
    /// [`Error::BlobTooLarge`].
    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        self.written += chunk.len() as u64;
        if self.written > self.cap {
            return Err(Error::BlobTooLarge { cap: self.cap });
        }
        self.encoder.write_all(chunk)?;
        Ok(())
    }

    pub fn finish(self) -> Result<BlobKey> {
        let file = self.encoder.finish()?;
        self.store.persist(file, Kind::Raw, &self.key)?;
        Ok(self.key)
    }
}
