//! Master-key discovery and loading (plan §3.8). The key unlocks the
//! credential [`Vault`](tagmail_store::Vault) in `config.db`; it must live
//! outside the data directory, so a backup of that directory alone cannot
//! decrypt the stored passwords.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use tagmail_store::credentials::{KEY_LEN, MasterKey};
use thiserror::Error;
use zeroize::Zeroizing;

/// systemd `LoadCredential=master.key` / `LoadCredentialEncrypted=`.
pub const SYSTEMD_CREDENTIAL: &str = "master.key";
pub const KEY_FILE_ENV: &str = "TAGMAIL_MASTER_KEY_FILE";
pub const DEFAULT_KEY_PATH: &str = "/etc/tagmail/master.key";
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

/// Reads the key at `path`. With `init`, a missing key is generated
/// first (owner-only permissions, parent directories created).
pub fn load_master_key(
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
    Ok((MasterKey::from_bytes(key), warnings))
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
    let key = MasterKey::generate();
    let text = Zeroizing::new(format!("{}\n", hex::encode(key.expose())));
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

#[cfg(test)]
mod tests {
    use super::*;

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
            load_master_key(&path, &data, false),
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

        let (first, warnings) = load_master_key(&path, &data, true).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, KEY_FILE_MODE);
        let (second, _) = load_master_key(&path, &data, true).unwrap();
        assert_eq!(
            first.expose(),
            second.expose(),
            "an existing key is never replaced"
        );
    }

    #[test]
    fn key_inside_the_data_dir_warns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/master.key");
        let (_, warnings) = load_master_key(&path, dir.path(), true).unwrap();
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
        let (_, warnings) = load_master_key(&path, data.path(), false).unwrap();
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
