use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Imap(#[from] tagmail_imap::Error),
    #[error(transparent)]
    Store(#[from] tagmail_store::Error),
    #[error("account {0} not found")]
    NoAccount(i64),
    #[error("invalid account settings: {0}")]
    InvalidAccount(String),
    /// The account's IMAP connection cannot take work now.
    #[error("{0}")]
    Unavailable(String),
    #[error("stopped")]
    Stopped,
}

impl Error {
    /// The IMAP connection is unusable after this error.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Error::Imap(e) if e.is_fatal())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What an account test shows for failures below the mail protocol, so
/// it does not tell apart why an arbitrary host or port did not answer.
pub(crate) const CONNECT_FAILED: &str = "cannot connect to the server";
pub(crate) const TLS_FAILED: &str = "the secure connection to the server failed";
pub(crate) const CLOSED: &str = "the server closed the connection";
pub(crate) const PROTOCOL_FAILED: &str = "the server's answer was not understood";
pub(crate) const TIMED_OUT: &str = "the server did not answer in time";

/// `e` as an account test shows it: the server's own NO, BAD or login
/// reply in full, anything below that as a fixed category.
pub(crate) fn test_message(e: &tagmail_imap::Error) -> String {
    use tagmail_imap::Error as E;
    match e {
        E::Io(_) => CONNECT_FAILED,
        E::Tls(_) | E::InvalidServerName(_) => TLS_FAILED,
        E::Closed { .. } => CLOSED,
        E::Timeout => TIMED_OUT,
        E::Protocol(_) | E::ResponseTooLong { .. } => PROTOCOL_FAILED,
        _ => return e.to_string(),
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_tests_show_server_replies_and_categories() {
        use tagmail_imap::Error as E;
        let refused = E::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused (os error 111) 10.0.0.7:25",
        ));
        assert_eq!(test_message(&refused), CONNECT_FAILED);
        assert_eq!(
            test_message(&E::Closed {
                bye: Some("internal-host.lan going away".into())
            }),
            CLOSED
        );
        assert_eq!(
            test_message(&E::Protocol("SSH-2.0-OpenSSH_9.6".into())),
            PROTOCOL_FAILED
        );
        assert_eq!(
            test_message(&E::InvalidServerName("x y".into())),
            TLS_FAILED
        );
        assert_eq!(test_message(&E::Timeout), TIMED_OUT);
        for kept in [
            E::Auth("[AUTHENTICATIONFAILED] wrong password".into()),
            E::No {
                code: None,
                text: "LIST failed".into(),
            },
            E::Bad("unknown command".into()),
        ] {
            assert_eq!(test_message(&kept), kept.to_string());
        }
    }
}
