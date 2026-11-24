//! Message encryption for Web Push (RFC 8291, `aes128gcm` of RFC 8188):
//! one record, the ephemeral public key as `keyid`, and the plaintext
//! padded to [`PAD_BUCKET`] so the ciphertext length hides the subject
//! length.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Key, Nonce};
use hkdf::Hkdf;
use p256::elliptic_curve::Generate;
use p256::{PublicKey, SecretKey};
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

pub const SALT_LEN: usize = 16;
pub const AUTH_LEN: usize = 16;
/// Uncompressed SEC1 P-256 point.
pub const PUBLIC_KEY_LEN: usize = 65;
pub const RECORD_SIZE: u32 = 4096;
/// Padded plaintexts are a multiple of this.
pub const PAD_BUCKET: usize = 256;
/// Larger plaintexts are a bug in the payload builder.
pub const MAX_PLAINTEXT: usize = 3000;
/// salt ‖ rs ‖ idlen ‖ keyid.
pub const HEADER_LEN: usize = SALT_LEN + 4 + 1 + PUBLIC_KEY_LEN;
const TAG_LEN: usize = 16;
const CEK_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const IKM_LEN: usize = 32;
const LAST_RECORD: u8 = 2;
const KEY_INFO: &[u8] = b"WebPush: info\0";
const CEK_INFO: &[u8] = b"Content-Encoding: aes128gcm\0";
const NONCE_INFO: &[u8] = b"Content-Encoding: nonce\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EceError {
    #[error("plaintext larger than {MAX_PLAINTEXT} bytes")]
    TooLarge,
    #[error("invalid subscription public key")]
    BadPublicKey,
    #[error("malformed or undecryptable message")]
    Malformed,
}

/// Content key and nonce for one message.
fn derive(
    salt: &[u8],
    ecdh_secret: &[u8],
    auth: &[u8; AUTH_LEN],
    ua_public: &[u8],
    as_public: &[u8],
) -> (Zeroizing<[u8; CEK_LEN]>, [u8; NONCE_LEN]) {
    let info = [KEY_INFO, ua_public, as_public].concat();
    let mut ikm = Zeroizing::new([0u8; IKM_LEN]);
    Hkdf::<Sha256>::new(Some(auth), ecdh_secret)
        .expand(&info, ikm.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 length");
    let prk = Hkdf::<Sha256>::new(Some(salt), ikm.as_ref());
    let mut cek = Zeroizing::new([0u8; CEK_LEN]);
    let mut nonce = [0u8; NONCE_LEN];
    prk.expand(CEK_INFO, cek.as_mut())
        .expect("16 bytes is a valid HKDF-SHA256 length");
    prk.expand(NONCE_INFO, &mut nonce)
        .expect("12 bytes is a valid HKDF-SHA256 length");
    (cek, nonce)
}

fn cipher(cek: &[u8; CEK_LEN]) -> Aes128Gcm {
    Aes128Gcm::new(&Key::<Aes128Gcm>::from(*cek))
}

/// Encrypts `plaintext` for a subscription's `p256dh` key and `auth`
/// secret, with a fresh salt and ephemeral key, padded to [`PAD_BUCKET`].
pub fn encrypt(
    ua_public: &[u8],
    auth: &[u8; AUTH_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, EceError> {
    let ua_public = PublicKey::from_sec1_bytes(ua_public).map_err(|_| EceError::BadPublicKey)?;
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).expect("the OS random source is available");
    encrypt_with(
        &salt,
        &SecretKey::generate(),
        &ua_public,
        auth,
        plaintext,
        Some(PAD_BUCKET),
    )
}

/// [`encrypt`] with given randomness; `pad_to: None` adds only the
/// delimiter (the RFC 8291 test vector).
pub(crate) fn encrypt_with(
    salt: &[u8; SALT_LEN],
    as_secret: &SecretKey,
    ua_public: &PublicKey,
    auth: &[u8; AUTH_LEN],
    plaintext: &[u8],
    pad_to: Option<usize>,
) -> Result<Vec<u8>, EceError> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(EceError::TooLarge);
    }
    let as_public = as_secret.public_key().to_sec1_bytes();
    let ua_bytes = ua_public.to_sec1_bytes();
    let shared = as_secret.diffie_hellman(ua_public);
    let (cek, nonce) = derive(salt, shared.raw_secret_bytes(), auth, &ua_bytes, &as_public);
    let padded_len = match pad_to {
        Some(bucket) => (plaintext.len() + 1).div_ceil(bucket) * bucket,
        None => plaintext.len() + 1,
    };
    let mut record = Zeroizing::new(Vec::with_capacity(padded_len));
    record.extend_from_slice(plaintext);
    record.push(LAST_RECORD);
    record.resize(padded_len, 0);
    let sealed = cipher(&cek)
        .encrypt(&Nonce::from(nonce), record.as_slice())
        .map_err(|_| EceError::Malformed)?;
    let id_len = u8::try_from(as_public.len()).expect("a SEC1 point fits in a byte");
    let mut body = Vec::with_capacity(HEADER_LEN + sealed.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(id_len);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&sealed);
    Ok(body)
}

/// The user agent's side: decrypts a single-record message. For tests and
/// for checking our own output.
pub fn decrypt(
    body: &[u8],
    ua_secret: &SecretKey,
    auth: &[u8; AUTH_LEN],
) -> Result<Vec<u8>, EceError> {
    if body.len() < HEADER_LEN + TAG_LEN {
        return Err(EceError::Malformed);
    }
    let (salt, rest) = body.split_at(SALT_LEN);
    let (_rs, rest) = rest.split_at(4);
    let (&id_len, rest) = rest.split_first().ok_or(EceError::Malformed)?;
    if usize::from(id_len) != PUBLIC_KEY_LEN {
        return Err(EceError::Malformed);
    }
    let (as_public, sealed) = rest.split_at(PUBLIC_KEY_LEN);
    let as_key = PublicKey::from_sec1_bytes(as_public).map_err(|_| EceError::Malformed)?;
    let shared = ua_secret.diffie_hellman(&as_key);
    let ua_public = ua_secret.public_key().to_sec1_bytes();
    let (cek, nonce) = derive(salt, shared.raw_secret_bytes(), auth, &ua_public, as_public);
    let mut record = cipher(&cek)
        .decrypt(&Nonce::from(nonce), sealed)
        .map_err(|_| EceError::Malformed)?;
    let end = record
        .iter()
        .rposition(|&b| b != 0)
        .ok_or(EceError::Malformed)?;
    if record[end] != LAST_RECORD {
        return Err(EceError::Malformed);
    }
    record.truncate(end);
    Ok(record)
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    fn b64(s: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(s).unwrap()
    }

    /// RFC 8291 §5 / Appendix A.
    struct Vector {
        plaintext: &'static [u8],
        as_private: SecretKey,
        ua_private: SecretKey,
        ua_public: Vec<u8>,
        auth: [u8; AUTH_LEN],
        salt: [u8; SALT_LEN],
        body: Vec<u8>,
    }

    fn vector() -> Vector {
        Vector {
            plaintext: b"When I grow up, I want to be a watermelon",
            as_private: SecretKey::from_slice(&b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"))
                .unwrap(),
            ua_private: SecretKey::from_slice(&b64("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94"))
                .unwrap(),
            ua_public: b64(
                "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            ),
            auth: b64("BTBZMqHH6r4Tts7J_aSIgg").try_into().unwrap(),
            salt: b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap(),
            body: b64(
                "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
            ),
        }
    }

    #[test]
    fn rfc8291_vector_encrypts_byte_for_byte() {
        let v = vector();
        let ua_public = PublicKey::from_sec1_bytes(&v.ua_public).unwrap();
        assert_eq!(
            *ua_public.to_sec1_bytes(),
            *v.ua_private.public_key().to_sec1_bytes()
        );
        let body = encrypt_with(
            &v.salt,
            &v.as_private,
            &ua_public,
            &v.auth,
            v.plaintext,
            None,
        )
        .unwrap();
        assert_eq!(body, v.body);
    }

    #[test]
    fn rfc8291_vector_decrypts() {
        let v = vector();
        assert_eq!(
            decrypt(&v.body, &v.ua_private, &v.auth).unwrap(),
            v.plaintext
        );
        let mut tampered = v.body.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(
            decrypt(&tampered, &v.ua_private, &v.auth),
            Err(EceError::Malformed)
        );
        assert_eq!(
            decrypt(&v.body, &v.ua_private, &[0; AUTH_LEN]),
            Err(EceError::Malformed)
        );
    }

    #[test]
    fn padded_messages_land_in_a_bucket_and_round_trip() {
        let ua = SecretKey::generate();
        let ua_public = ua.public_key().to_sec1_bytes();
        let auth = [9u8; AUTH_LEN];
        for len in [0, 1, PAD_BUCKET - 1, PAD_BUCKET, MAX_PLAINTEXT] {
            let plaintext = vec![b'x'; len];
            let body = encrypt(&ua_public, &auth, &plaintext).unwrap();
            let record = body.len() - HEADER_LEN - TAG_LEN;
            assert_eq!(record % PAD_BUCKET, 0, "plaintext of {len}");
            assert!(record > len);
            assert_eq!(decrypt(&body, &ua, &auth).unwrap(), plaintext);
        }
        let short = encrypt(&ua_public, &auth, b"Hi").unwrap();
        let long = encrypt(&ua_public, &auth, &[b'y'; 200]).unwrap();
        assert_eq!(short.len(), long.len(), "subject length leaks");
    }

    #[test]
    fn oversized_plaintext_and_bad_keys_are_refused() {
        let ua_public = SecretKey::generate().public_key().to_sec1_bytes();
        assert_eq!(
            encrypt(&ua_public, &[0; AUTH_LEN], &[0; MAX_PLAINTEXT + 1]),
            Err(EceError::TooLarge)
        );
        let mut not_on_curve = ua_public.to_vec();
        not_on_curve[PUBLIC_KEY_LEN - 1] ^= 1;
        assert_eq!(
            encrypt(&not_on_curve, &[0; AUTH_LEN], b"x"),
            Err(EceError::BadPublicKey)
        );
    }
}
