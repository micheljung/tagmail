//! The device token: in the OS keychain (Secret Service, macOS Keychain,
//! Windows Credential Manager), or, where none is available, in a file only
//! the user can read (owner decision), which the status reports as a
//! warning.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

const SERVICE: &str = "dev.tagmail.desktop";
const ACCOUNT: &str = "device-token";
pub const FILE_WARNING: &str =
    "No system keychain is available, so the device token is kept in a file only you can read.";

pub struct TokenStore {
    file: PathBuf,
    keychain: bool,
}

impl TokenStore {
    pub fn new(file: PathBuf, keychain: bool) -> TokenStore {
        TokenStore { file, keychain }
    }

    /// Whether the token lives in the fallback file.
    pub fn in_file(&self) -> bool {
        self.file.exists()
    }

    pub fn load(&self) -> io::Result<Option<String>> {
        if self.keychain
            && let Ok(token) = entry().and_then(|e| e.get_password())
        {
            return Ok(Some(token));
        }
        match fs::read_to_string(&self.file) {
            Ok(token) => Ok(Some(token.trim().to_owned())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, token: &str) -> io::Result<()> {
        if self.keychain {
            match entry().and_then(|e| e.set_password(token)) {
                Ok(()) => return remove(&self.file),
                Err(e) => tracing::warn!(error = %e, "keychain unavailable; using the token file"),
            }
        }
        write_private(&self.file, token)
    }

    pub fn delete(&self) -> io::Result<()> {
        if self.keychain
            && let Err(e) = entry().and_then(|e| e.delete_credential())
            && !matches!(e, keyring::Error::NoEntry)
        {
            tracing::warn!(error = %e, "removing the keychain entry failed");
        }
        remove(&self.file)
    }
}

fn entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, ACCOUNT)
}

fn remove(path: &PathBuf) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn write_private(path: &PathBuf, token: &str) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let mut file = options.open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(token.as_bytes())?;
        file.sync_all()
    }
    #[cfg(not(unix))]
    {
        let mut file = options.open(path)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn the_file_fallback_is_private_and_removable() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path().join("token"), false);
        assert_eq!(store.load().unwrap(), None);
        assert!(!store.in_file());
        store.save("secret").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("secret"));
        assert!(store.in_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("token"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.delete().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }
}
