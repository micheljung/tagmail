//! A scripted IMAP server over an in-memory pipe for transcript tests.

use std::time::Duration;

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream,
};
use tokio::task::JoinHandle;

use crate::session::{Password, ServerConfig, TokioSession};
use crate::tls::Security;

pub(crate) enum Step {
    /// Reads one command, asserts it (without the tag) starts with the
    /// expected text, case-insensitively, then sends the lines with `{tag}`
    /// replaced. A line without a tag (`DONE`, SASL data) answers with the tag
    /// of the preceding command.
    Cmd(&'static str, Vec<String>),
    /// Sends lines without waiting for a command.
    Send(Vec<String>),
    Sleep(Duration),
}

pub(crate) fn cmd<S: AsRef<str>>(expect: &'static str, lines: &[S]) -> Step {
    Step::Cmd(
        expect,
        lines.iter().map(|l| l.as_ref().to_owned()).collect(),
    )
}

pub(crate) fn send(lines: &[&str]) -> Step {
    Step::Send(lines.iter().map(|l| (*l).to_owned()).collect())
}

/// Greeting, CAPABILITY, LOGIN, CAPABILITY and (if `caps` has ENABLE) ENABLE.
pub(crate) fn login(caps: &str) -> Vec<Step> {
    let cap_line = format!("* CAPABILITY IMAP4rev1 {caps}");
    let upper = caps.to_ascii_uppercase();
    let mut steps = vec![
        send(&["* OK ready"]),
        cmd("CAPABILITY", &[&cap_line, "{tag} OK done"]),
    ];
    if !upper.contains("AUTH=PLAIN") {
        steps.push(cmd("LOGIN", &["{tag} OK logged in"]));
    } else if upper.contains("SASL-IR") {
        steps.push(cmd(
            "AUTHENTICATE PLAIN AHVzZXIAcGFzcw==",
            &["{tag} OK logged in"],
        ));
    } else {
        steps.push(cmd("AUTHENTICATE PLAIN", &["+ "]));
        steps.push(cmd("AHVzZXIAcGFzcw==", &["{tag} OK logged in"]));
    }
    steps.push(cmd("CAPABILITY", &[&cap_line, "{tag} OK done"]));
    if upper.contains("ENABLE") {
        if upper.contains("QRESYNC") {
            steps.push(cmd(
                "ENABLE QRESYNC",
                &["* ENABLED QRESYNC", "{tag} OK enabled"],
            ));
        } else if upper.contains("CONDSTORE") {
            steps.push(cmd(
                "ENABLE CONDSTORE",
                &["* ENABLED CONDSTORE", "{tag} OK enabled"],
            ));
        }
    }
    steps
}

pub(crate) fn config() -> ServerConfig {
    ServerConfig::new("test", 0, Security::Plain, "user", Password::new("pass"))
}

async fn read_command<S: AsyncRead + AsyncWrite + Unpin>(r: &mut BufReader<S>) -> Option<String> {
    let mut command = Vec::new();
    loop {
        let mut line = Vec::new();
        if r.read_until(b'\n', &mut line).await.ok()? == 0 {
            return None;
        }
        command.extend_from_slice(&line);
        let trimmed = line.strip_suffix(b"\r\n").unwrap_or(&line);
        let Some(open) = trimmed.strip_suffix(b"}").and_then(|t| {
            let i = t.iter().rposition(|&c| c == b'{')?;
            Some(&t[i + 1..])
        }) else {
            return Some(String::from_utf8_lossy(&command).into_owned());
        };
        let non_sync = open.ends_with(b"+");
        let n: usize = std::str::from_utf8(open.strip_suffix(b"+").unwrap_or(open))
            .ok()?
            .parse()
            .ok()?;
        if !non_sync {
            r.get_mut().write_all(b"+ go ahead\r\n").await.ok()?;
        }
        let mut literal = vec![0; n];
        r.read_exact(&mut literal).await.ok()?;
        command.extend_from_slice(&literal);
    }
}

/// Runs `steps` on the server end; the join handle surfaces assertion
/// failures. Returns the client end.
pub(crate) fn serve(steps: Vec<Step>) -> (DuplexStream, JoinHandle<Vec<String>>) {
    let (client, server) = tokio::io::duplex(4 << 20);
    (client, serve_on(server, steps))
}

/// Runs `steps` on any server-side stream.
pub(crate) fn serve_on<S>(server: S, steps: Vec<Step>) -> JoinHandle<Vec<String>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut r = BufReader::new(server);
        let mut seen = Vec::new();
        let mut last_tag = String::new();
        for step in steps {
            match step {
                Step::Send(lines) => {
                    for l in lines {
                        r.get_mut()
                            .write_all(format!("{l}\r\n").as_bytes())
                            .await
                            .unwrap();
                    }
                }
                Step::Sleep(d) => tokio::time::sleep(d).await,
                Step::Cmd(expect, lines) => {
                    let command = read_command(&mut r)
                        .await
                        .unwrap_or_else(|| panic!("connection closed, expected {expect}"));
                    let (tag, rest) = if !command.trim_end().contains(' ') {
                        (last_tag.clone(), command.trim_end().to_owned())
                    } else {
                        let (t, rest) = command.split_once(' ').expect("tagged command");
                        (t.to_owned(), rest.trim_end().to_owned())
                    };
                    assert!(
                        rest.to_ascii_uppercase()
                            .starts_with(&expect.to_ascii_uppercase()),
                        "expected {expect:?}, got {rest:?}"
                    );
                    seen.push(rest);
                    last_tag.clone_from(&tag);
                    for l in lines {
                        let l = l.replace("{tag}", &tag);
                        r.get_mut()
                            .write_all(format!("{l}\r\n").as_bytes())
                            .await
                            .unwrap();
                    }
                }
            }
        }
        r.get_mut().flush().await.unwrap();
        seen
    })
}

/// A logged-in session against `steps`, which must start with [`login`].
pub(crate) async fn session(
    steps: Vec<Step>,
    config: &ServerConfig,
) -> (TokioSession, JoinHandle<Vec<String>>) {
    let (io, handle) = serve(steps);
    let session = TokioSession::start(Box::new(io), false, config)
        .await
        .expect("login");
    (session, handle)
}
