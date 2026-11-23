//! The server's SSE stream as a doorbell (§4): a `changes` event means
//! "pull now", `reset` means "bootstrap again". The event data is ignored;
//! the pull reads the same changes with their rows.

use reqwest::Method;
use tagmail_core::ChangeCursor;

use crate::client::ServerClient;
use crate::error::{Error, Result};
use crate::import::RecordReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doorbell {
    Changes,
    Reset,
}

pub struct Doorbells {
    lines: LineReader,
    client: ServerClient,
}

/// Opens `/api/events` after `since`; a 410 is [`crate::ClientError::Gone`].
pub async fn connect(client: &ServerClient, since: ChangeCursor) -> Result<Doorbells> {
    let builder = client
        .request(Method::GET, &format!("api/events?since={since}"))?
        .header(reqwest::header::ACCEPT, "text/event-stream");
    let resp = client.send(builder).await?;
    Ok(Doorbells {
        lines: LineReader::new(resp.bytes_stream()),
        client: client.clone(),
    })
}

impl Doorbells {
    /// The next ring; `None` when the server closed the stream.
    pub async fn next(&mut self) -> Result<Option<Doorbell>> {
        let mut event = None;
        loop {
            let Some(line) = self.lines.next().await? else {
                return Ok(None);
            };
            self.client.link().mark_ok();
            if line.is_empty() {
                match event.take() {
                    Some("changes") => return Ok(Some(Doorbell::Changes)),
                    Some("reset") => return Ok(Some(Doorbell::Reset)),
                    _ => continue,
                }
            }
            if let Some(name) = line.strip_prefix("event:") {
                event = match name.trim() {
                    "changes" => Some("changes"),
                    "reset" => Some("reset"),
                    _ => None,
                };
            }
        }
    }
}

/// SSE lines without their terminator; heartbeats are comment lines.
struct LineReader {
    inner: RecordReader<'static>,
}

impl LineReader {
    fn new<S, B, E>(stream: S) -> LineReader
    where
        S: futures_util::Stream<Item = std::result::Result<B, E>> + Send + 'static,
        B: AsRef<[u8]>,
        E: std::fmt::Display,
    {
        LineReader {
            inner: RecordReader::new(stream),
        }
    }

    async fn next(&mut self) -> Result<Option<String>> {
        match self.inner.raw_line().await {
            Ok(Some(line)) => Ok(Some(
                String::from_utf8_lossy(&line)
                    .trim_end_matches('\r')
                    .to_owned(),
            )),
            Ok(None) => Ok(None),
            Err(Error::Incomplete(e)) => Err(Error::Client(crate::ClientError::Offline(e))),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::stream;
    use url::Url;

    use super::*;

    fn bells(body: &'static str) -> Doorbells {
        let chunks: Vec<Result<Vec<u8>, String>> =
            body.as_bytes().chunks(7).map(|c| Ok(c.to_vec())).collect();
        Doorbells {
            lines: LineReader::new(stream::iter(chunks)),
            client: ServerClient::new(&Url::parse("http://127.0.0.1:1/").unwrap(), None).unwrap(),
        }
    }

    #[tokio::test]
    async fn rings_on_changes_and_reset_and_ignores_heartbeats() {
        let mut b = bells(
            ": heartbeat\n\nevent: changes\nid: 1:2\ndata: {}\n\n:\n\nevent: reset\r\ndata: {}\r\n\r\n",
        );
        assert_eq!(b.next().await.unwrap(), Some(Doorbell::Changes));
        assert!(b.client.is_online());
        assert_eq!(b.next().await.unwrap(), Some(Doorbell::Reset));
        assert_eq!(b.next().await.unwrap(), None);
    }
}
