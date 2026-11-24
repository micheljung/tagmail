//! VAPID (RFC 8292): the server's P-256 key identifies it to push services,
//! which bind subscriptions to it. The key is created on first use and kept
//! sealed by the credential vault in `config.db` meta.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::SecretKey;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::Generate;
use reqwest::Url;
use serde::Serialize;
use tagmail_store::credentials::NONCE_LEN;
use tagmail_store::rusqlite::{OptionalExtension, params};
use tagmail_store::{ConfigDb, Sealed, Vault};
use tokio::sync::OnceCell;
use zeroize::Zeroizing;

/// `config.db` meta key of the sealed private scalar (hex of nonce ‖ ct).
pub const VAPID_META: &str = "vapid_key";
const VAPID_AAD: &[u8] = b"vapid";
/// `exp` of a JWT, from when it is made.
pub const JWT_LIFETIME_SECS: i64 = 12 * 60 * 60;
/// A JWT is reused for the same push service this long.
pub const JWT_REUSE_SECS: i64 = 60 * 60;
pub const JWT_CACHE_ENTRIES: usize = 8;
/// `sub` when the server has no https public origin.
pub const DEFAULT_SUBJECT: &str = "https://github.com/micheljung/tagmail";
const JWT_HEADER: &str = r#"{"typ":"JWT","alg":"ES256"}"#;

pub struct VapidKey {
    signing: SigningKey,
    public: Box<[u8]>,
}

impl fmt::Debug for VapidKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VapidKey(<redacted>)")
    }
}

impl VapidKey {
    pub fn generate() -> VapidKey {
        VapidKey::from_secret(SecretKey::generate())
    }

    pub fn from_scalar(scalar: &[u8]) -> anyhow::Result<VapidKey> {
        let secret =
            SecretKey::from_slice(scalar).map_err(|_| anyhow::anyhow!("invalid VAPID key"))?;
        Ok(VapidKey::from_secret(secret))
    }

    fn from_secret(secret: SecretKey) -> VapidKey {
        VapidKey {
            public: secret.public_key().to_sec1_bytes(),
            signing: SigningKey::from(secret),
        }
    }

    fn scalar(&self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(self.signing.to_bytes().to_vec())
    }

    /// The 65-byte uncompressed point.
    pub fn public_key(&self) -> &[u8] {
        &self.public
    }

    /// The browser's `applicationServerKey`.
    pub fn public_key_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.public)
    }
}

#[derive(Serialize)]
struct Claims<'a> {
    aud: &'a str,
    exp: i64,
    sub: &'a str,
}

struct CachedJwt {
    aud: String,
    issued: i64,
    jwt: String,
}

/// Signs VAPID JWTs, reusing one per push service for a while.
#[derive(Debug)]
pub struct Vapid {
    key: VapidKey,
    sub: String,
    cache: Mutex<VecDeque<CachedJwt>>,
}

impl fmt::Debug for CachedJwt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedJwt")
            .field("aud", &self.aud)
            .field("issued", &self.issued)
            .finish_non_exhaustive()
    }
}

/// `sub`: the public origin when it is https, else the project page.
pub fn subject(public_origin: Option<&Url>) -> String {
    match public_origin {
        Some(origin) if origin.scheme() == "https" => origin.origin().ascii_serialization(),
        _ => DEFAULT_SUBJECT.to_owned(),
    }
}

impl Vapid {
    pub fn new(key: VapidKey, sub: String) -> Vapid {
        Vapid {
            key,
            sub,
            cache: Mutex::new(VecDeque::new()),
        }
    }

    pub fn key(&self) -> &VapidKey {
        &self.key
    }

    /// The `Authorization` header for a push to `endpoint` at unix time
    /// `now`.
    pub fn authorization(&self, endpoint: &Url, now: i64) -> String {
        let aud = endpoint.origin().ascii_serialization();
        format!(
            "vapid t={}, k={}",
            self.jwt(&aud, now),
            self.key.public_key_b64()
        )
    }

    fn jwt(&self, aud: &str, now: i64) -> String {
        let mut cache = self.cache.lock().expect("JWT cache lock");
        cache.retain(|c| now - c.issued < JWT_REUSE_SECS && now >= c.issued);
        if let Some(cached) = cache.iter().find(|c| c.aud == aud) {
            return cached.jwt.clone();
        }
        let claims = Claims {
            aud,
            exp: now + JWT_LIFETIME_SECS,
            sub: &self.sub,
        };
        let claims = serde_json::to_vec(&claims).expect("claims serialize");
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(JWT_HEADER),
            URL_SAFE_NO_PAD.encode(claims)
        );
        let signature: Signature = self.key.signing.sign(input.as_bytes());
        let jwt = format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()));
        if cache.len() == JWT_CACHE_ENTRIES {
            cache.pop_front();
        }
        cache.push_back(CachedJwt {
            aud: aud.to_owned(),
            issued: now,
            jwt: jwt.clone(),
        });
        jwt
    }
}

/// Loads or creates the key on first use, once.
pub struct VapidStore {
    config: Arc<ConfigDb>,
    vault: Arc<Vault>,
    sub: String,
    vapid: OnceCell<Arc<Vapid>>,
}

impl fmt::Debug for VapidStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VapidStore")
            .field("sub", &self.sub)
            .finish_non_exhaustive()
    }
}

impl VapidStore {
    pub fn new(config: Arc<ConfigDb>, vault: Arc<Vault>, sub: String) -> VapidStore {
        VapidStore {
            config,
            vault,
            sub,
            vapid: OnceCell::new(),
        }
    }

    pub async fn get(&self) -> anyhow::Result<Arc<Vapid>> {
        self.vapid
            .get_or_try_init(|| async {
                let key = self.load_or_create().await?;
                Ok(Arc::new(Vapid::new(key, self.sub.clone())))
            })
            .await
            .cloned()
    }

    async fn load_or_create(&self) -> anyhow::Result<VapidKey> {
        if let Some(stored) = read_meta(&self.config, VAPID_META).await? {
            let bytes = hex::decode(stored)?;
            if bytes.len() <= NONCE_LEN {
                anyhow::bail!("corrupt {VAPID_META}");
            }
            let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
            let sealed = Sealed {
                ciphertext: ciphertext.to_vec(),
                nonce: nonce.to_vec(),
            };
            let scalar = self.vault.open_bytes(VAPID_AAD, &sealed)?;
            return VapidKey::from_scalar(&scalar);
        }
        let key = VapidKey::generate();
        let sealed = self.vault.seal_bytes(VAPID_AAD, &key.scalar())?;
        let value = hex::encode([sealed.nonce, sealed.ciphertext].concat());
        write_meta(&self.config, VAPID_META, value).await?;
        tracing::info!("created the Web Push (VAPID) key");
        Ok(key)
    }
}

async fn read_meta(config: &ConfigDb, key: &'static str) -> anyhow::Result<Option<String>> {
    Ok(config
        .read(move |conn| {
            Ok(conn
                .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                .optional()?)
        })
        .await?)
}

async fn write_meta(config: &ConfigDb, key: &'static str, value: String) -> anyhow::Result<()> {
    config
        .write(move |tx| {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;
    use serde_json::Value;

    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn parts(header: &str) -> (String, String) {
        let rest = header.strip_prefix("vapid t=").unwrap();
        let (jwt, k) = rest.split_once(", k=").unwrap();
        (jwt.to_owned(), k.to_owned())
    }

    fn json(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn jwt_verifies_and_carries_the_claims() {
        let vapid = Vapid::new(VapidKey::generate(), "https://mail.example".into());
        let endpoint = Url::parse("https://fcm.googleapis.com/fcm/send/abc?x=1").unwrap();
        let (jwt, k) = parts(&vapid.authorization(&endpoint, NOW));

        let k = URL_SAFE_NO_PAD.decode(k).unwrap();
        assert_eq!(k.len(), 65);
        assert_eq!(k, vapid.key().public_key());

        let segments: Vec<&str> = jwt.split('.').collect();
        let [header, claims, signature] = segments[..] else {
            panic!("{jwt}")
        };
        assert_eq!(
            json(header),
            serde_json::json!({"typ": "JWT", "alg": "ES256"})
        );
        let claims = json(claims);
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "https://mail.example");
        let ttl = claims["exp"].as_i64().unwrap() - NOW;
        assert!(ttl > 0 && ttl <= 24 * 60 * 60, "{ttl}");

        let signature = URL_SAFE_NO_PAD.decode(signature).unwrap();
        assert_eq!(signature.len(), 64);
        let signature = Signature::from_slice(&signature).unwrap();
        let verifier = VerifyingKey::from_sec1_bytes(&k).unwrap();
        let signed = format!("{header}.{}", segments[1]);
        verifier.verify(signed.as_bytes(), &signature).unwrap();
        assert!(verifier.verify(b"other", &signature).is_err());
    }

    #[test]
    fn jwts_are_cached_per_audience_for_an_hour() {
        let vapid = Vapid::new(VapidKey::generate(), DEFAULT_SUBJECT.into());
        let a = Url::parse("https://a.example/x").unwrap();
        let a2 = Url::parse("https://a.example/y").unwrap();
        let b = Url::parse("https://b.example/x").unwrap();
        let first = vapid.authorization(&a, NOW);
        assert_eq!(vapid.authorization(&a2, NOW + 10), first);
        assert_ne!(vapid.authorization(&b, NOW), first);
        assert_ne!(vapid.authorization(&a, NOW + JWT_REUSE_SECS), first);
        for i in 0..20 {
            let url = Url::parse(&format!("https://h{i}.example/")).unwrap();
            vapid.authorization(&url, NOW);
        }
        assert!(vapid.cache.lock().unwrap().len() <= JWT_CACHE_ENTRIES);
    }

    #[test]
    fn subject_needs_an_https_origin() {
        let https = Url::parse("https://mail.example/app").unwrap();
        let http = Url::parse("http://localhost:7878").unwrap();
        assert_eq!(subject(Some(&https)), "https://mail.example");
        assert_eq!(subject(Some(&http)), DEFAULT_SUBJECT);
        assert_eq!(subject(None), DEFAULT_SUBJECT);
    }

    #[test]
    fn debug_is_redacted() {
        let key = VapidKey::generate();
        assert_eq!(format!("{key:?}"), "VapidKey(<redacted>)");
        let vapid = Vapid::new(key, DEFAULT_SUBJECT.into());
        assert!(!format!("{vapid:?}").contains(&hex::encode(vapid.key().scalar().as_slice())));
    }

    #[tokio::test]
    async fn key_is_created_once_sealed_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.db");
        let master = tagmail_store::MasterKey::from_bytes(Zeroizing::new([3; 32]));
        let config = Arc::new(ConfigDb::open(&path).unwrap());
        let vault = Arc::new(Vault::unlock(&config, &master).await.unwrap());
        let store = VapidStore::new(config.clone(), vault.clone(), DEFAULT_SUBJECT.into());
        let (a, b) = tokio::join!(store.get(), store.get());
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(Arc::ptr_eq(&a, &b));
        let public = a.key().public_key().to_vec();

        let stored = read_meta(&config, VAPID_META).await.unwrap().unwrap();
        let scalar = hex::encode(a.key().scalar().as_slice());
        assert!(!stored.contains(&scalar), "raw scalar stored");

        let again = VapidStore::new(config.clone(), vault, DEFAULT_SUBJECT.into());
        assert_eq!(again.get().await.unwrap().key().public_key(), public);

        let (other_vault, _) = Vault::create(&master).unwrap();
        let wrong = VapidStore::new(config, Arc::new(other_vault), DEFAULT_SUBJECT.into());
        assert!(wrong.get().await.is_err());
    }
}
