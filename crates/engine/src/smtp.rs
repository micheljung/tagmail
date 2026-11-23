//! SMTP submission with lettre over rustls (ring, Mozilla roots): implicit
//! TLS or STARTTLS as the account says; plaintext only with the server's
//! development flag that also turns IMAP TLS off.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use lettre::Address;
use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
use lettre::transport::smtp::commands::{Data, Mail, Rcpt};
use lettre::transport::smtp::extension::{ClientId, Extension, MailBodyParameter, MailParameter};
use tagmail_api_core::BoxFuture;
use tagmail_core::AccountId;
use tagmail_imap::{Password, Security};
use tagmail_store::Sealed;
use tagmail_store::rusqlite::OptionalExtension;

use crate::error::{Error, Result};
use crate::supervisor::Stores;

/// Timeout of a submission, plus [`MIN_RATE`] for the message itself.
pub const SMTP_TIMEOUT: Duration = Duration::from_secs(120);
/// Slowest upload the timeout allows for, in bytes per second.
const MIN_RATE: u64 = 32 * 1024;
/// Message bytes handed to the connection at a time.
const CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct SmtpServer {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
    pub password: Password,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpError {
    /// The server rejected the message for good (5xx); retrying will not
    /// help.
    pub permanent: bool,
    pub message: String,
}

/// Submits one message, read from a file; tests substitute a recorder.
pub trait SmtpSender: Send + Sync + 'static {
    fn send(
        &self,
        server: SmtpServer,
        from: String,
        recipients: Vec<String>,
        message: PathBuf,
    ) -> BoxFuture<Result<(), SmtpError>>;
}

/// Submits with lettre's connection API so the message streams from its
/// file to DATA in chunks instead of being loaded whole.
#[derive(Debug, Clone, Default)]
pub struct LettreSender {
    /// Submit without TLS whatever the account says; for local test
    /// servers only.
    pub insecure_plaintext: bool,
}

fn permanent(message: String) -> SmtpError {
    SmtpError {
        permanent: true,
        message,
    }
}

fn smtp(e: lettre::transport::smtp::Error) -> SmtpError {
    SmtpError {
        permanent: e.is_permanent(),
        message: e.to_string(),
    }
}

/// The message file in chunks; a read error ends it early and is kept.
struct Chunks {
    file: File,
    error: Arc<Mutex<Option<io::Error>>>,
}

impl Iterator for Chunks {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; CHUNK];
        match self.file.read(&mut buf) {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some(buf)
            }
            Err(e) => {
                *self.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
                None
            }
        }
    }
}

/// Whether the file has bytes outside ASCII. Blocking.
fn has_8bit(path: &PathBuf) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(false);
        }
        if !buf[..n].is_ascii() {
            return Ok(true);
        }
    }
}

async fn submit(
    insecure: bool,
    server: SmtpServer,
    envelope: Envelope,
    path: PathBuf,
) -> Result<(), SmtpError> {
    let read = |e: io::Error| permanent(format!("reading the queued message: {e}"));
    let eight_bit = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || has_8bit(&path))
            .await
            .map_err(|e| permanent(e.to_string()))?
            .map_err(read)?
    };
    let file = File::open(&path).map_err(read)?;
    let hello = ClientId::default();
    let tls = || {
        TlsParameters::new_rustls(server.host.clone())
            .map_err(|e| permanent(format!("TLS setup: {e}")))
    };
    let security = if insecure {
        Security::Plain
    } else {
        server.security
    };
    let wrapper = match security {
        Security::Tls => Some(tls()?),
        _ => None,
    };
    let address = (server.host.as_str(), server.port);
    let mut conn =
        AsyncSmtpConnection::connect_tokio1(address, Some(SMTP_TIMEOUT), &hello, wrapper, None)
            .await
            .map_err(smtp)?;
    if security == Security::StartTls {
        conn.starttls(tls()?, &hello).await.map_err(smtp)?;
    }
    let credentials = Credentials::new(server.username, server.password.expose().to_owned());
    conn.auth(&[Mechanism::Plain, Mechanism::Login], &credentials)
        .await
        .map_err(smtp)?;
    let mut parameters = Vec::new();
    let non_ascii = envelope
        .from()
        .into_iter()
        .chain(envelope.to())
        .any(|a| !AsRef::<str>::as_ref(a).is_ascii());
    if non_ascii {
        if !conn.server_info().supports_feature(Extension::SmtpUtfEight) {
            return Err(permanent(
                "non-ASCII addresses need SMTPUTF8, which the server lacks".to_owned(),
            ));
        }
        parameters.push(MailParameter::SmtpUtfEight);
    }
    if eight_bit {
        if !conn.server_info().supports_feature(Extension::EightBitMime) {
            return Err(permanent(
                "the message needs 8BITMIME, which the server lacks".to_owned(),
            ));
        }
        parameters.push(MailParameter::Body(MailBodyParameter::EightBitMime));
    }
    conn.command(Mail::new(envelope.from().cloned(), parameters))
        .await
        .map_err(smtp)?;
    for to in envelope.to() {
        conn.command(Rcpt::new(to.clone(), Vec::new()))
            .await
            .map_err(smtp)?;
    }
    conn.command(Data).await.map_err(smtp)?;
    let error = Arc::new(Mutex::new(None));
    let chunks = Chunks {
        file,
        error: error.clone(),
    };
    let accepted = conn.message_iter(chunks).await;
    let failed = error.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(e) = failed {
        conn.abort().await;
        return Err(permanent(format!(
            "reading the queued message failed while sending, so it may have arrived \
             truncated: {e}"
        )));
    }
    accepted.map_err(smtp)?;
    let _ = conn.quit().await;
    Ok(())
}

impl SmtpSender for LettreSender {
    fn send(
        &self,
        server: SmtpServer,
        from: String,
        recipients: Vec<String>,
        message: PathBuf,
    ) -> BoxFuture<Result<(), SmtpError>> {
        let insecure = self.insecure_plaintext;
        Box::pin(async move {
            let parse = |a: &str| {
                a.parse::<Address>()
                    .map_err(|e| permanent(format!("address {a:?}: {e}")))
            };
            let to = recipients
                .iter()
                .map(|r| parse(r))
                .collect::<Result<Vec<_>, _>>()?;
            let envelope = Envelope::new(Some(parse(&from)?), to)
                .map_err(|e| permanent(format!("envelope: {e}")))?;
            let size = std::fs::metadata(&message).map(|m| m.len()).unwrap_or(0);
            let limit = SMTP_TIMEOUT + Duration::from_secs(size / MIN_RATE);
            tokio::time::timeout(limit, submit(insecure, server, envelope, message))
                .await
                .unwrap_or_else(|_| {
                    Err(SmtpError {
                        permanent: false,
                        message: "the SMTP server timed out".to_owned(),
                    })
                })
        })
    }
}

/// An account's SMTP settings with the decrypted password.
pub async fn load_server(stores: &Stores, id: AccountId) -> Result<SmtpServer> {
    let row = stores
        .config
        .read(move |c| {
            Ok(c.query_row(
                "SELECT smtp_host, smtp_port, smtp_tls, username, secret_ct, secret_nonce \
                 FROM account WHERE id = ?1",
                [id.0],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u16>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        Sealed {
                            ciphertext: r.get(4)?,
                            nonce: r.get(5)?,
                        },
                    ))
                },
            )
            .optional()?)
        })
        .await?;
    let (host, port, tls, username, sealed) = row.ok_or(Error::NoAccount(id.0))?;
    let security = match tls.as_str() {
        "tls" => Security::Tls,
        "starttls" => Security::StartTls,
        other => return Err(Error::InvalidAccount(format!("smtp_tls {other:?}"))),
    };
    let password = stores
        .vault
        .open(id, &sealed)
        .map_err(tagmail_store::Error::from)?;
    Ok(SmtpServer {
        host,
        port,
        security,
        username,
        password: Password::new(password.as_str()),
    })
}
