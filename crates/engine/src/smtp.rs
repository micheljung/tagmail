//! SMTP submission with lettre over rustls (ring, Mozilla roots): implicit
//! TLS or STARTTLS as the account says; plaintext only with the server's
//! development flag that also turns IMAP TLS off.

use std::sync::Arc;
use std::time::Duration;

use lettre::address::Envelope;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use tagmail_api_core::BoxFuture;
use tagmail_core::AccountId;
use tagmail_imap::{Password, Security};
use tagmail_store::Sealed;
use tagmail_store::rusqlite::OptionalExtension;

use crate::error::{Error, Result};
use crate::supervisor::Stores;

/// Whole-conversation timeout of one submission.
pub const SMTP_TIMEOUT: Duration = Duration::from_secs(120);

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

/// Submits one message; tests substitute a recorder.
pub trait SmtpSender: Send + Sync + 'static {
    fn send(
        &self,
        server: SmtpServer,
        from: String,
        recipients: Vec<String>,
        message: Arc<Vec<u8>>,
    ) -> BoxFuture<Result<(), SmtpError>>;
}

#[derive(Debug, Clone, Default)]
pub struct LettreSender {
    /// Submit without TLS whatever the account says; for local test
    /// servers only.
    pub insecure_plaintext: bool,
}

fn invalid(message: String) -> SmtpError {
    SmtpError {
        permanent: true,
        message,
    }
}

impl SmtpSender for LettreSender {
    fn send(
        &self,
        server: SmtpServer,
        from: String,
        recipients: Vec<String>,
        message: Arc<Vec<u8>>,
    ) -> BoxFuture<Result<(), SmtpError>> {
        let insecure = self.insecure_plaintext;
        Box::pin(async move {
            let parse = |a: &str| {
                a.parse::<Address>()
                    .map_err(|e| invalid(format!("address {a:?}: {e}")))
            };
            let to = recipients
                .iter()
                .map(|r| parse(r))
                .collect::<Result<Vec<_>, _>>()?;
            let envelope = Envelope::new(Some(parse(&from)?), to)
                .map_err(|e| invalid(format!("envelope: {e}")))?;
            let tls = |host: &str| {
                TlsParameters::new_rustls(host.to_owned())
                    .map_err(|e| invalid(format!("TLS setup: {e}")))
            };
            let tls = match (insecure, server.security) {
                (true, _) | (false, Security::Plain) => Tls::None,
                (false, Security::Tls) => Tls::Wrapper(tls(&server.host)?),
                (false, Security::StartTls) => Tls::Required(tls(&server.host)?),
            };
            let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&server.host)
                .port(server.port)
                .tls(tls)
                .credentials(Credentials::new(
                    server.username,
                    server.password.expose().to_owned(),
                ))
                .timeout(Some(SMTP_TIMEOUT))
                .build();
            transport
                .send_raw(&envelope, &message)
                .await
                .map(|_| ())
                .map_err(|e| SmtpError {
                    permanent: e.is_permanent(),
                    message: e.to_string(),
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
