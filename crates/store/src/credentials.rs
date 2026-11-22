//! Credential encryption at rest (plan §3.8): each account secret is sealed
//! with XChaCha20-Poly1305 under a random data key (AAD = account id). The
//! data key is stored in `config.db` meta, wrapped by a master key that the
//! caller loads from outside the data directory.

use std::fmt;

use chacha20poly1305::aead::{Aead, Generate, Key, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rusqlite::{OptionalExtension, params};
use tagmail_core::AccountId;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::db::ConfigDb;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
/// `config.db` meta key of the wrapped data key (hex of nonce ‖ ciphertext).
pub const DATA_KEY_META: &str = "credential_data_key";
const DATA_KEY_AAD: &[u8] = b"tagmail credential data key v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CredentialError {
    #[error(
        "the master key does not match config.db: wrong key file, or config.db from another installation"
    )]
    WrongKey,
    #[error("corrupt wrapped data key in config.db")]
    CorruptDataKey,
    #[error("cannot encrypt or decrypt a credential")]
    Crypto,
}

pub struct MasterKey(Zeroizing<[u8; KEY_LEN]>);

impl MasterKey {
    pub fn from_bytes(bytes: Zeroizing<[u8; KEY_LEN]>) -> MasterKey {
        MasterKey(bytes)
    }

    /// A new random key, e.g. for `--init-key`.
    pub fn generate() -> MasterKey {
        let key = Key::<XChaCha20Poly1305>::generate();
        let mut bytes = Zeroizing::new([0u8; KEY_LEN]);
        bytes.copy_from_slice(key.as_slice());
        MasterKey(bytes)
    }

    pub fn expose(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&Key::<XChaCha20Poly1305>::from(*self.0))
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
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

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vault(<redacted>)")
    }
}

fn account_aad(account: AccountId) -> [u8; 8] {
    account.0.to_le_bytes()
}

impl Vault {
    /// Unwraps the data key in `config`, creating and storing one on first
    /// use.
    pub async fn unlock(config: &ConfigDb, master: &MasterKey) -> crate::Result<Vault> {
        let wrapped: Option<String> = config
            .read(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT value FROM meta WHERE key = ?1",
                        [DATA_KEY_META],
                        |r| r.get(0),
                    )
                    .optional()?)
            })
            .await?;
        if let Some(value) = wrapped {
            let bytes = hex::decode(value).map_err(|_| CredentialError::CorruptDataKey)?;
            return Ok(Vault::unwrap(master, &bytes)?);
        }
        let (vault, wrapped) = Vault::create(master)?;
        let value = hex::encode(wrapped);
        config
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                    params![DATA_KEY_META, value],
                )?;
                Ok(())
            })
            .await?;
        Ok(vault)
    }

    /// Unwraps `wrapped` (nonce ‖ ciphertext) with the master key.
    pub fn unwrap(master: &MasterKey, wrapped: &[u8]) -> Result<Vault, CredentialError> {
        if wrapped.len() <= NONCE_LEN {
            return Err(CredentialError::CorruptDataKey);
        }
        let (nonce, ct) = wrapped.split_at(NONCE_LEN);
        let nonce = XNonce::try_from(nonce).map_err(|_| CredentialError::CorruptDataKey)?;
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
                .map_err(|_| CredentialError::WrongKey)?,
        );
        let key = Key::<XChaCha20Poly1305>::try_from(data_key.as_slice())
            .map_err(|_| CredentialError::CorruptDataKey)?;
        Ok(Vault {
            cipher: XChaCha20Poly1305::new(&key),
        })
    }

    /// A new random data key; returns it wrapped for storage.
    pub fn create(master: &MasterKey) -> Result<(Vault, Vec<u8>), CredentialError> {
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
            .map_err(|_| CredentialError::Crypto)?;
        let wrapped = [nonce.as_slice(), &ct].concat();
        Ok((
            Vault {
                cipher: XChaCha20Poly1305::new(&data_key),
            },
            wrapped,
        ))
    }

    pub fn seal(&self, account: AccountId, secret: &str) -> Result<Sealed, CredentialError> {
        let nonce = XNonce::generate();
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: secret.as_bytes(),
                    aad: &account_aad(account),
                },
            )
            .map_err(|_| CredentialError::Crypto)?;
        Ok(Sealed {
            ciphertext,
            nonce: nonce.to_vec(),
        })
    }

    pub fn open(
        &self,
        account: AccountId,
        sealed: &Sealed,
    ) -> Result<Zeroizing<String>, CredentialError> {
        let nonce =
            XNonce::try_from(sealed.nonce.as_slice()).map_err(|_| CredentialError::Crypto)?;
        let plain = Zeroizing::new(
            self.cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: &sealed.ciphertext,
                        aad: &account_aad(account),
                    },
                )
                .map_err(|_| CredentialError::Crypto)?,
        );
        let text = std::str::from_utf8(&plain).map_err(|_| CredentialError::Crypto)?;
        Ok(Zeroizing::new(text.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "correct horse battery staple";

    fn key(byte: u8) -> MasterKey {
        MasterKey::from_bytes(Zeroizing::new([byte; KEY_LEN]))
    }

    #[test]
    fn secrets_round_trip_through_a_wrapped_data_key() {
        let master = key(7);
        let (vault, wrapped) = Vault::create(&master).unwrap();
        let sealed = vault.seal(AccountId(3), SECRET).unwrap();
        assert!(
            !sealed.ciphertext.windows(5).any(|w| w == b"horse"),
            "plaintext leaked"
        );

        let reopened = Vault::unwrap(&master, &wrapped).unwrap();
        assert_eq!(*reopened.open(AccountId(3), &sealed).unwrap(), SECRET);
    }

    #[test]
    fn wrong_master_key_is_rejected() {
        let (_, wrapped) = Vault::create(&key(7)).unwrap();
        let wrong = key(8);
        assert_eq!(
            Vault::unwrap(&wrong, &wrapped).err(),
            Some(CredentialError::WrongKey)
        );
        assert_eq!(
            Vault::unwrap(&wrong, &wrapped[..NONCE_LEN]).err(),
            Some(CredentialError::CorruptDataKey)
        );
    }

    #[test]
    fn ciphertext_is_bound_to_its_account() {
        let (vault, _) = Vault::create(&key(1)).unwrap();
        let sealed = vault.seal(AccountId(1), SECRET).unwrap();
        assert!(vault.open(AccountId(2), &sealed).is_err());
        let mut tampered = sealed.clone();
        tampered.ciphertext[0] ^= 1;
        assert!(vault.open(AccountId(1), &tampered).is_err());
        let (other, _) = Vault::create(&key(1)).unwrap();
        assert!(other.open(AccountId(1), &sealed).is_err());
    }

    #[test]
    fn debug_is_redacted() {
        assert_eq!(format!("{:?}", key(1)), "MasterKey(<redacted>)");
        let (vault, _) = Vault::create(&key(1)).unwrap();
        assert_eq!(format!("{vault:?}"), "Vault(<redacted>)");
    }

    #[tokio::test]
    async fn unlock_creates_once_then_reuses_the_data_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.db");
        let config = ConfigDb::open(&path).unwrap();
        let sealed = Vault::unlock(&config, &key(4))
            .await
            .unwrap()
            .seal(AccountId(1), SECRET)
            .unwrap();
        let vault = Vault::unlock(&config, &key(4)).await.unwrap();
        assert_eq!(*vault.open(AccountId(1), &sealed).unwrap(), SECRET);
        assert!(matches!(
            Vault::unlock(&config, &key(5)).await,
            Err(crate::Error::Credential(CredentialError::WrongKey))
        ));
        config.close().await.unwrap();
    }
}
