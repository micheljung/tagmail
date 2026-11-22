//! Credential encryption at rest (plan §3.8): each account secret is sealed
//! with XChaCha20-Poly1305 under a random data key (AAD = account id); the
//! data key is stored in `config.db`, wrapped by a master key that lives
//! outside the data directory.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use chacha20poly1305::aead::{Aead, Generate, Key, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use tagmail_api_core::Secret;
use tagmail_core::AccountId;
use thiserror::Error;
use zeroize::Zeroizing;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
/// systemd `LoadCredential=master.key` / `LoadCredentialEncrypted=`.
pub const SYSTEMD_CREDENTIAL: &str = "master.key";
pub const KEY_FILE_ENV: &str = "TAGMAIL_MASTER_KEY_FILE";
pub const DEFAULT_KEY_PATH: &str = "/etc/tagmail/master.key";
/// `config.db` meta key of the wrapped data key.
pub const DATA_KEY_META: &str = "credential_data_key";
const DATA_KEY_AAD: &[u8] = b"tagmail credential data key v1";
#[cfg(unix)]
const KEY_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const KEY_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const GROUP_OTHER_BITS: u32 = 0o077;

#[derive(Debug, Error)]
pub enum KeyError {
    #[error(
        "master key {0} does not exist; provide one (see --master-key) or start once with --init-key to generate it"
    )]
    Missing(PathBuf),
    #[error("master key {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("master key {0} must hold 32 raw bytes, 64 hex digits or 32 bytes in base64")]
    Malformed(PathBuf),
    #[error(
        "the master key does not match config.db: wrong key file, or config.db from another installation"
    )]
    WrongKey,
    #[error("corrupt wrapped data key in config.db")]
    CorruptDataKey,
    #[error("cannot encrypt or decrypt a credential")]
    Crypto,
}

/// Something about the key's location that weakens protection; logged
/// loudly at every start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyWarning {
    /// A backup of the data directory would contain the key that decrypts it.
    InsideDataDir { key: PathBuf, data_dir: PathBuf },
    #[cfg(unix)]
    LoosePermissions { key: PathBuf, mode: u32 },
}

impl std::fmt::Display for KeyWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyWarning::InsideDataDir { key, data_dir } => write!(
                f,
                "MASTER KEY {} IS INSIDE THE DATA DIRECTORY {}: anyone with a copy of the data \
                 directory can decrypt the stored IMAP/SMTP passwords; move the key outside it",
                key.display(),
                data_dir.display()
            ),
            #[cfg(unix)]
            KeyWarning::LoosePermissions { key, mode } => write!(
                f,
                "master key {} has mode {mode:o}; restrict it to the owner (chmod 600)",
                key.display()
            ),
        }
    }
}

/// Picks the key file: the `--master-key` flag, then the systemd
/// credential, then `TAGMAIL_MASTER_KEY_FILE` (Docker secret), then
/// [`DEFAULT_KEY_PATH`].
pub fn resolve_key_path(
    flag: Option<PathBuf>,
    credentials_directory: Option<OsString>,
    key_file_env: Option<OsString>,
) -> PathBuf {
    flag.or_else(|| credentials_directory.map(|d| PathBuf::from(d).join(SYSTEMD_CREDENTIAL)))
        .or_else(|| key_file_env.map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_KEY_PATH))
}

pub struct MasterKey(Zeroizing<[u8; KEY_LEN]>);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

impl MasterKey {
    /// Reads the key at `path`. With `init`, a missing key is generated
    /// first (owner-only permissions, parent directories created).
    pub fn load(
        path: &Path,
        data_dir: &Path,
        init: bool,
    ) -> Result<(MasterKey, Vec<KeyWarning>), KeyError> {
        let io_err = |source| KeyError::Io {
            path: path.to_owned(),
            source,
        };
        let bytes = match fs::read(path) {
            Ok(bytes) => Zeroizing::new(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound && init => {
                generate(path).map_err(io_err)?;
                tracing::warn!(path = %path.display(), "generated a new master key; back it up together with config.db");
                Zeroizing::new(fs::read(path).map_err(io_err)?)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(KeyError::Missing(path.to_owned()));
            }
            Err(e) => return Err(io_err(e)),
        };
        let key = parse(&bytes).ok_or_else(|| KeyError::Malformed(path.to_owned()))?;

        let mut warnings = Vec::new();
        if let (Ok(key_path), Ok(data)) = (path.canonicalize(), data_dir.canonicalize())
            && key_path.starts_with(&data)
        {
            warnings.push(KeyWarning::InsideDataDir {
                key: key_path,
                data_dir: data,
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path).map_err(io_err)?.permissions().mode();
            if mode & GROUP_OTHER_BITS != 0 {
                warnings.push(KeyWarning::LoosePermissions {
                    key: path.to_owned(),
                    mode: mode & 0o777,
                });
            }
        }
        Ok((MasterKey(key), warnings))
    }

    #[cfg(test)]
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> MasterKey {
        MasterKey(Zeroizing::new(bytes))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&Key::<XChaCha20Poly1305>::from(*self.0))
    }
}

/// Raw 32 bytes, or 64 hex digits / base64 of 32 bytes (`openssl rand
/// -base64 32`), surrounding whitespace ignored.
fn parse(bytes: &[u8]) -> Option<Zeroizing<[u8; KEY_LEN]>> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    if bytes.len() == KEY_LEN {
        key.copy_from_slice(bytes);
        return Some(key);
    }
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if hex::decode_to_slice(text, key.as_mut_slice()).is_ok() {
        return Some(key);
    }
    let decoded = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .ok()?,
    );
    (decoded.len() == KEY_LEN).then(|| {
        key.copy_from_slice(&decoded);
        key
    })
}

fn generate(path: &Path) -> io::Result<()> {
    let key = Key::<XChaCha20Poly1305>::generate();
    let text = Zeroizing::new(format!("{}\n", hex::encode(key.as_slice())));
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let mut dirs = fs::DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dirs, KEY_DIR_MODE);
        dirs.create(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, KEY_FILE_MODE);
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// An encrypted secret as stored in `account.secret_ct` / `secret_nonce`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

/// Holds the unwrapped data key.
pub struct Vault {
    cipher: XChaCha20Poly1305,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Vault(<redacted>)")
    }
}

fn account_aad(account: AccountId) -> [u8; 8] {
    account.0.to_le_bytes()
}

impl Vault {
    /// Unwraps `wrapped` (nonce ‖ ciphertext) with the master key.
    pub fn unwrap(master: &MasterKey, wrapped: &[u8]) -> Result<Vault, KeyError> {
        if wrapped.len() <= NONCE_LEN {
            return Err(KeyError::CorruptDataKey);
        }
        let (nonce, ct) = wrapped.split_at(NONCE_LEN);
        let nonce = XNonce::try_from(nonce).map_err(|_| KeyError::CorruptDataKey)?;
        let data_key = Zeroizing::new(
            master
                .cipher()
                .decrypt(
                    &nonce,
                    Payload {
                        msg: ct,
                        aad: DATA_KEY_AAD,
                    },
                )
                .map_err(|_| KeyError::WrongKey)?,
        );
        let key = Key::<XChaCha20Poly1305>::try_from(data_key.as_slice())
            .map_err(|_| KeyError::CorruptDataKey)?;
        Ok(Vault {
            cipher: XChaCha20Poly1305::new(&key),
        })
    }

    /// A new random data key; returns it wrapped for storage.
    pub fn create(master: &MasterKey) -> Result<(Vault, Vec<u8>), KeyError> {
        let data_key = Key::<XChaCha20Poly1305>::generate();
        let nonce = XNonce::generate();
        let ct = master
            .cipher()
            .encrypt(
                &nonce,
                Payload {
                    msg: data_key.as_slice(),
                    aad: DATA_KEY_AAD,
                },
            )
            .map_err(|_| KeyError::Crypto)?;
        let wrapped = [nonce.as_slice(), &ct].concat();
        Ok((
            Vault {
                cipher: XChaCha20Poly1305::new(&data_key),
            },
            wrapped,
        ))
    }

    pub fn seal(&self, account: AccountId, secret: &Secret) -> Result<Sealed, KeyError> {
        let nonce = XNonce::generate();
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: secret.expose().as_bytes(),
                    aad: &account_aad(account),
                },
            )
            .map_err(|_| KeyError::Crypto)?;
        Ok(Sealed {
            ciphertext,
            nonce: nonce.to_vec(),
        })
    }

    pub fn open(&self, account: AccountId, sealed: &Sealed) -> Result<Secret, KeyError> {
        let nonce = XNonce::try_from(sealed.nonce.as_slice()).map_err(|_| KeyError::Crypto)?;
        let plain = Zeroizing::new(
            self.cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: &sealed.ciphertext,
                        aad: &account_aad(account),
                    },
                )
                .map_err(|_| KeyError::Crypto)?,
        );
        let text = std::str::from_utf8(&plain).map_err(|_| KeyError::Crypto)?;
        Ok(Secret::new(text.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Secret {
        Secret::new("correct horse battery staple".to_owned())
    }

    #[test]
    fn secrets_round_trip_through_a_wrapped_data_key() {
        let master = MasterKey::from_bytes([7; KEY_LEN]);
        let (vault, wrapped) = Vault::create(&master).unwrap();
        let sealed = vault.seal(AccountId(3), &secret()).unwrap();
        assert!(
            !sealed.ciphertext.windows(5).any(|w| w == b"horse"),
            "plaintext leaked"
        );

        let reopened = Vault::unwrap(&master, &wrapped).unwrap();
        assert_eq!(reopened.open(AccountId(3), &sealed).unwrap(), secret());
    }

    #[test]
    fn wrong_master_key_is_rejected() {
        let (_, wrapped) = Vault::create(&MasterKey::from_bytes([7; KEY_LEN])).unwrap();
        let wrong = MasterKey::from_bytes([8; KEY_LEN]);
        assert!(matches!(
            Vault::unwrap(&wrong, &wrapped),
            Err(KeyError::WrongKey)
        ));
        assert!(matches!(
            Vault::unwrap(&wrong, &wrapped[..NONCE_LEN]),
            Err(KeyError::CorruptDataKey)
        ));
    }

    #[test]
    fn ciphertext_is_bound_to_its_account() {
        let (vault, _) = Vault::create(&MasterKey::from_bytes([1; KEY_LEN])).unwrap();
        let sealed = vault.seal(AccountId(1), &secret()).unwrap();
        assert!(vault.open(AccountId(2), &sealed).is_err());
        let mut tampered = sealed.clone();
        tampered.ciphertext[0] ^= 1;
        assert!(vault.open(AccountId(1), &tampered).is_err());
        let (other, _) = Vault::create(&MasterKey::from_bytes([1; KEY_LEN])).unwrap();
        assert!(other.open(AccountId(1), &sealed).is_err());
    }

    #[test]
    fn key_path_precedence() {
        let flag = Some(PathBuf::from("/flag.key"));
        let creds = Some(OsString::from("/run/credentials/tagmail.service"));
        let env = Some(OsString::from("/run/secrets/master_key"));
        assert_eq!(
            resolve_key_path(flag, creds.clone(), env.clone()),
            Path::new("/flag.key")
        );
        assert_eq!(
            resolve_key_path(None, creds, env.clone()),
            Path::new("/run/credentials/tagmail.service/master.key")
        );
        assert_eq!(
            resolve_key_path(None, None, env),
            Path::new("/run/secrets/master_key")
        );
        assert_eq!(
            resolve_key_path(None, None, None),
            Path::new(DEFAULT_KEY_PATH)
        );
    }

    #[test]
    fn missing_key_refuses_without_init() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys/master.key");
        let data = dir.path().join("data");
        fs::create_dir(&data).unwrap();
        assert!(matches!(
            MasterKey::load(&path, &data, false),
            Err(KeyError::Missing(_))
        ));
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn init_generates_an_owner_only_key_once() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys/master.key");
        let data = dir.path().join("data");
        fs::create_dir(&data).unwrap();

        let (first, warnings) = MasterKey::load(&path, &data, true).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, KEY_FILE_MODE);
        let (second, _) = MasterKey::load(&path, &data, true).unwrap();
        assert_eq!(*first.0, *second.0, "an existing key is never replaced");
    }

    #[test]
    fn key_inside_the_data_dir_warns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/master.key");
        let (_, warnings) = MasterKey::load(&path, dir.path(), true).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w, KeyWarning::InsideDataDir { .. })),
            "{warnings:?}"
        );
        assert!(
            warnings[0]
                .to_string()
                .contains("INSIDE THE DATA DIRECTORY")
        );
    }

    #[cfg(unix)]
    #[test]
    fn readable_key_warns() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        fs::write(&path, [9u8; KEY_LEN]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let (_, warnings) = MasterKey::load(&path, data.path(), false).unwrap();
        assert_eq!(
            warnings,
            [KeyWarning::LoosePermissions {
                key: path,
                mode: 0o644
            }]
        );
    }

    #[test]
    fn key_file_formats() {
        let raw = [5u8; KEY_LEN];
        let hex_text = format!("{}\n", hex::encode(raw));
        let b64 = format!(
            " {}\n",
            base64::engine::general_purpose::STANDARD.encode(raw)
        );
        for input in [raw.to_vec(), hex_text.into_bytes(), b64.into_bytes()] {
            assert_eq!(*parse(&input).unwrap(), raw);
        }
        for bad in [&b"short"[..], &[0u8; 33][..], b"zz"] {
            assert!(parse(bad).is_none());
        }
    }
}
