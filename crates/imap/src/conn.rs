//! Drives the imap-next client state machine over any async byte stream.
//!
//! imap-next's own `Stream` only wraps a `TcpStream`; owning the I/O here
//! lets the same code run over TLS, STARTTLS-upgraded sockets and in-memory
//! test pipes, and lets raw commands (for attributes imap-codec cannot
//! encode) share the connection.

use std::time::Duration;

use imap_next::client::{Client, Event, Options};
use imap_next::imap_types::command::{Command, CommandBody};
use imap_next::imap_types::core::Tag;
use imap_next::imap_types::response::{Code, Data, Greeting, Status, StatusBody, StatusKind};
use imap_next::{Interrupt, Io as NextIo, State};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::rawfetch;
use crate::types::MessageMeta;

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

const READ_CHUNK: usize = 16 * 1024;

/// One low-level event, with codec failures already classified.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "short-lived, moved once per response"
)]
pub(crate) enum ConnEvent {
    Next(Event),
    /// A FETCH line imap-codec rejected, recovered by [`rawfetch`].
    RawFetch(Box<MessageMeta>),
    /// A line nobody could parse; logged and otherwise ignored.
    Unparsed(Vec<u8>),
    TooLong,
}

/// Everything received while one command ran.
#[derive(Debug, Default)]
pub(crate) struct Collected {
    pub data: Vec<Data<'static>>,
    pub raw_fetches: Vec<MessageMeta>,
    pub untagged: Vec<StatusBody<'static>>,
    pub too_long: bool,
}

impl Collected {
    pub fn codes(&self) -> impl Iterator<Item = &Code<'static>> {
        self.untagged.iter().filter_map(|s| s.code.as_ref())
    }
}

/// The tagged completion of a command.
#[derive(Debug)]
pub(crate) struct Done {
    pub kind: StatusKind,
    pub code: Option<Code<'static>>,
    pub text: String,
}

pub(crate) struct Conn {
    io: Box<dyn Io>,
    client: Client,
    write_buf: Vec<u8>,
    next_tag: u32,
    pub timeout: Duration,
    pub max_response_size: u32,
    pub bye: Option<String>,
    pub broken: bool,
}

impl Conn {
    pub fn new(
        io: Box<dyn Io>,
        greeting_done: bool,
        max_response_size: u32,
        timeout: Duration,
    ) -> Self {
        let mut options = Options::default();
        options.crlf_relaxed = true;
        options.max_response_size = max_response_size;
        options.discard_greeting = greeting_done;
        Self {
            io,
            client: Client::new(options),
            write_buf: Vec::new(),
            next_tag: 0,
            timeout,
            max_response_size,
            bye: None,
            broken: false,
        }
    }

    pub fn tag(&mut self) -> Tag<'static> {
        self.next_tag += 1;
        Tag::try_from(format!("T{}", self.next_tag)).expect("valid tag")
    }

    pub fn closed(&self) -> Error {
        Error::Closed {
            bye: self.bye.clone(),
        }
    }

    /// Cancel-safe: written bytes are dropped from `write_buf` as they go
    /// out, and read bytes are handed to the client immediately.
    pub async fn next_event(&mut self) -> Result<ConnEvent> {
        loop {
            match self.client.next() {
                Ok(event) => return Ok(ConnEvent::Next(event)),
                Err(Interrupt::Io(NextIo::Output(bytes))) => self.write_buf.extend(bytes),
                Err(Interrupt::Io(NextIo::NeedMoreInput)) => self.io_step().await?,
                Err(Interrupt::Error(e)) => return Ok(classify(e)),
            }
        }
    }

    async fn io_step(&mut self) -> Result<()> {
        use std::io::ErrorKind;
        self.io_step_inner().await.map_err(|e| match e {
            Error::Io(io)
                if matches!(
                    io.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof
                ) =>
            {
                self.closed()
            }
            other => other,
        })
    }

    async fn io_step_inner(&mut self) -> Result<()> {
        while !self.write_buf.is_empty() {
            let n = self.io.write(&self.write_buf).await?;
            if n == 0 {
                return Err(self.closed());
            }
            self.write_buf.drain(..n);
        }
        self.io.flush().await?;
        let mut buf = [0u8; READ_CHUNK];
        let n = self.io.read(&mut buf).await?;
        if n == 0 {
            return Err(self.closed());
        }
        self.client.enqueue_input(&buf[..n]);
        Ok(())
    }

    pub async fn greeting(&mut self) -> Result<Greeting<'static>> {
        match self.next_event().await? {
            ConnEvent::Next(Event::GreetingReceived { greeting }) => Ok(greeting),
            other => Err(Error::Protocol(format!("expected greeting, got {other:?}"))),
        }
    }

    /// Sends `body` and collects responses until its tagged completion.
    pub async fn run(&mut self, body: CommandBody<'static>) -> Result<(Collected, Done)> {
        let tag = self.tag();
        self.client.enqueue_command(Command {
            tag: tag.clone(),
            body,
        });
        self.finish(tag.as_ref()).await
    }

    /// Sends a command line imap-codec cannot encode. Only for commands
    /// without literals; no typed command may be in flight.
    pub async fn run_raw(&mut self, line: &str) -> Result<(Collected, Done)> {
        if line.bytes().any(|c| c == b'\r' || c == b'\n') {
            return Err(Error::InvalidArgument(
                "raw command contains a line break".into(),
            ));
        }
        let tag = self.tag();
        self.write_buf
            .extend_from_slice(format!("{} {line}\r\n", tag.as_ref()).as_bytes());
        self.finish(tag.as_ref()).await
    }

    async fn finish(&mut self, tag: &str) -> Result<(Collected, Done)> {
        let timeout = self.timeout;
        match tokio::time::timeout(timeout, self.collect(tag)).await {
            Ok(Ok(out)) => Ok(out),
            Ok(Err(e)) => {
                if e.is_fatal() {
                    self.broken = true;
                }
                Err(e)
            }
            Err(_) => {
                self.broken = true;
                Err(Error::Timeout)
            }
        }
    }

    async fn collect(&mut self, tag: &str) -> Result<(Collected, Done)> {
        let mut out = Collected::default();
        loop {
            let event = self.next_event().await?;
            if let Some(done) = self.absorb(event, tag, &mut out)? {
                return Ok((out, done));
            }
        }
    }

    /// Records one event; returns the completion once `tag` is done.
    pub fn absorb(
        &mut self,
        event: ConnEvent,
        tag: &str,
        out: &mut Collected,
    ) -> Result<Option<Done>> {
        match event {
            ConnEvent::Next(Event::DataReceived { data }) => out.data.push(data),
            ConnEvent::Next(Event::StatusReceived { status }) => match status {
                Status::Tagged(t) if t.tag.as_ref() == tag => return Ok(Some(done(t.body))),
                Status::Tagged(t) => {
                    tracing::warn!(tag = t.tag.as_ref(), "ignoring status for unknown tag");
                }
                Status::Untagged(body) => out.untagged.push(body),
                Status::Bye(bye) => self.bye = Some(bye.text.as_ref().to_owned()),
            },
            ConnEvent::Next(Event::CommandRejected { status, .. })
            | ConnEvent::Next(Event::IdleRejected { status, .. }) => {
                let body = match status {
                    Status::Tagged(t) => t.body,
                    Status::Untagged(b) => b,
                    Status::Bye(b) => StatusBody {
                        kind: StatusKind::Bad,
                        code: b.code,
                        text: b.text,
                    },
                };
                return Ok(Some(done(body)));
            }
            ConnEvent::Next(Event::AuthenticateStatusReceived { status, .. }) => {
                if let Status::Tagged(t) = status {
                    return Ok(Some(done(t.body)));
                }
            }
            ConnEvent::Next(Event::AuthenticateContinuationRequestReceived { .. }) => {
                return Err(Error::Protocol("unexpected SASL challenge".into()));
            }
            ConnEvent::Next(Event::GreetingReceived { .. }) => {
                return Err(Error::Protocol("unexpected greeting".into()));
            }
            ConnEvent::Next(
                Event::CommandSent { .. }
                | Event::AuthenticateStarted { .. }
                | Event::IdleCommandSent { .. }
                | Event::IdleAccepted { .. }
                | Event::IdleDoneSent { .. }
                | Event::ContinuationRequestReceived { .. },
            ) => {}
            ConnEvent::RawFetch(meta) => out.raw_fetches.push(*meta),
            ConnEvent::TooLong => out.too_long = true,
            ConnEvent::Unparsed(bytes) => {
                if let Some(kind) = rawfetch::parse_tagged_status(&bytes, tag) {
                    let kind = match kind.to_ascii_uppercase().as_slice() {
                        b"OK" => StatusKind::Ok,
                        b"NO" => StatusKind::No,
                        _ => StatusKind::Bad,
                    };
                    return Ok(Some(Done {
                        kind,
                        code: None,
                        text: String::from_utf8_lossy(&bytes).trim_end().to_owned(),
                    }));
                }
                tracing::warn!(
                    line = %String::from_utf8_lossy(&bytes[..bytes.len().min(200)]),
                    "ignoring unparseable response"
                );
            }
        }
        Ok(None)
    }

    pub fn enqueue(&mut self, body: CommandBody<'static>) -> Tag<'static> {
        let tag = self.tag();
        self.client.enqueue_command(Command {
            tag: tag.clone(),
            body,
        });
        tag
    }

    pub fn set_idle_done(&mut self) -> bool {
        self.client.set_idle_done().is_some()
    }

    pub fn set_authenticate_data(&mut self, data: Vec<u8>) -> Result<()> {
        use imap_next::imap_types::auth::AuthenticateData;
        use imap_next::imap_types::secret::Secret;
        self.client
            .set_authenticate_data(AuthenticateData::Continue(Secret::new(data.into())))
            .map(|_| ())
            .map_err(|_| Error::Protocol("no authentication in progress".into()))
    }
}

fn done(body: StatusBody<'static>) -> Done {
    Done {
        kind: body.kind,
        code: body.code,
        text: body.text.as_ref().to_owned(),
    }
}

fn classify(e: imap_next::client::Error) -> ConnEvent {
    use imap_next::client::Error as E;
    match e {
        E::MalformedMessage { discarded_bytes } | E::ExpectedCrlfGotLf { discarded_bytes } => {
            let bytes = discarded_bytes.declassify().to_vec();
            match rawfetch::parse_fetch(&bytes) {
                Some(meta) => ConnEvent::RawFetch(Box::new(meta)),
                None => ConnEvent::Unparsed(bytes),
            }
        }
        E::ResponseTooLong { .. } => ConnEvent::TooLong,
    }
}
