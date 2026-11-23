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

use crate::error::{CLOSED, CONNECT_FAILED, Error, PROTOCOL_FAILED, Result, TIMED_OUT, TLS_FAILED};
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

    /// Connects and logs in without sending anything.
    fn check(&self, server: SmtpServer) -> BoxFuture<Result<(), SmtpError>> {
        let _ = server;
        Box::pin(async {
            Err(permanent(
                "this sender cannot test an SMTP server".to_owned(),
            ))
        })
    }
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

/// Like [`smtp`] for an account test: the server's own replies in full,
/// failures below SMTP as a fixed category.
fn coarse(e: lettre::transport::smtp::Error) -> SmtpError {
    if e.is_permanent() || e.is_transient() || e.is_client() {
        return smtp(e);
    }
    let category = if e.is_timeout() {
        TIMED_OUT
    } else if e.is_tls() {
        TLS_FAILED
    } else if e.is_response() {
        PROTOCOL_FAILED
    } else if e.is_transport_shutdown() {
        CLOSED
    } else {
        CONNECT_FAILED
    };
    SmtpError {
        permanent: false,
        message: category.to_owned(),
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

/// Connects with the account's TLS mode and authenticates; failures go
/// through `error`.
async fn login(
    insecure: bool,
    server: SmtpServer,
    error: fn(lettre::transport::smtp::Error) -> SmtpError,
) -> Result<AsyncSmtpConnection, SmtpError> {
    let hello = ClientId::default();
    let tls = || {
        TlsParameters::new_rustls(server.host.clone()).map_err(|e| SmtpError {
            permanent: true,
            ..error(e)
        })
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
            .map_err(error)?;
    if security == Security::StartTls {
        conn.starttls(tls()?, &hello).await.map_err(error)?;
    }
    let credentials = Credentials::new(server.username, server.password.expose().to_owned());
    conn.auth(&[Mechanism::Plain, Mechanism::Login], &credentials)
        .await
        .map_err(error)?;
    Ok(conn)
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
    let mut conn = login(insecure, server, smtp).await?;
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
    fn check(&self, server: SmtpServer) -> BoxFuture<Result<(), SmtpError>> {
        let insecure = self.insecure_plaintext;
        Box::pin(async move {
            let mut conn = login(insecure, server, coarse).await?;
            let _ = conn.quit().await;
            Ok(())
        })
    }

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

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::*;

    fn server(port: u16) -> SmtpServer {
        SmtpServer {
            host: "127.0.0.1".to_owned(),
            port,
            security: Security::Tls,
            username: "u".to_owned(),
            password: Password::new("p"),
        }
    }

    /// Serves one connection: writes each reply after reading a line,
    /// the first one right away.
    fn scripted(replies: &'static [&'static str]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            for (i, reply) in replies.iter().enumerate() {
                if i > 0 {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                }
                let _ = writer.write_all(reply.as_bytes());
            }
        });
        port
    }

    async fn check(port: u16) -> String {
        let sender = LettreSender {
            insecure_plaintext: true,
        };
        sender.check(server(port)).await.unwrap_err().message
    }

    #[tokio::test]
    async fn account_test_shows_categories_below_smtp() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        assert_eq!(check(port).await, CONNECT_FAILED);
        assert_eq!(
            check(scripted(&["SSH-2.0-OpenSSH_9.6 internal-host\r\n"])).await,
            PROTOCOL_FAILED
        );
    }

    #[tokio::test]
    async fn account_test_keeps_the_servers_reply() {
        let port = scripted(&[
            "220 mail ready\r\n",
            "250-mail\r\n250 AUTH PLAIN LOGIN\r\n",
            "535 5.7.8 bad credentials\r\n",
        ]);
        let message = check(port).await;
        assert!(message.contains("bad credentials"), "{message}");
    }
}
