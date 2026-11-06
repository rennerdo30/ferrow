//! Local IPC between Ferrow clients (CLI, GUI, launchers) and the
//! `ferrowd` index service (M2a; spec: `docs/milestones/M2-service.md`).
//!
//! - [`protocol`]: the versioned, length-prefixed wire format.
//!   Requests are `Hello{version}`, `Query{text, limit, sort,
//!   flags}`, `Count{text}`, `Status` and `Cancel{id}`; responses
//!   are `Results{paths…}` streamed in pages, `Count{n}`,
//!   `Status{entries, mode, report}` and `Error{code, msg}`.
//! - [`transport`]: a Windows named pipe with an explicit security
//!   descriptor and remote clients rejected, or a Unix domain
//!   socket. Caller identity always comes from the OS, never from
//!   a claim inside a message.
//! - [`Client`] and [`serve_stream`]: the handshake and
//!   request/response plumbing over any stream.

mod dispatch;
pub mod protocol;
pub mod transport;
pub use dispatch::{RequestContext, serve_cancellable};

use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// A stream whose deadline can be renewed at protocol boundaries. Embedders
/// must enforce this deadline across partial reads and writes, without extending
/// it on byte progress. In-memory streams may implement this as a no-op.
pub trait DeadlineIo: Read + Write {
    fn set_deadline(&mut self, deadline: Instant) -> std::io::Result<()>;
}

impl DeadlineIo for Stream {
    fn set_deadline(&mut self, deadline: Instant) -> std::io::Result<()> {
        Stream::set_deadline(self, deadline)
    }
}

fn operation<S: DeadlineIo>(stream: &mut S) -> std::io::Result<()> {
    stream.set_deadline(Instant::now() + Duration::from_secs(5))
}

pub use protocol::{
    Envelope, ErrorCode, MAX_FRAME_BYTES, Message, PROTOCOL_VERSION, ProtocolError, Request,
    Response, Sort, read_message, write_message,
};
pub use transport::{CallerIdentity, Endpoint, Listener, Stream};

fn io_invalid(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

fn version_mismatch(peer: u16) -> Message {
    Message::Response(Response::Error {
        code: ErrorCode::VersionMismatch,
        msg: format!("protocol version {peer} not supported; this build speaks {PROTOCOL_VERSION}"),
    })
}

fn server_read<S: DeadlineIo>(stream: &mut S) -> std::io::Result<Option<Envelope>> {
    server_read_frame(stream, false)
}

fn server_read_frame<S: DeadlineIo>(
    stream: &mut S,
    idle: bool,
) -> std::io::Result<Option<Envelope>> {
    // Idle is a separate five-minute resource limit. Once the first byte
    // arrives, the whole remaining frame has one five-second budget.
    let result = if idle {
        stream.set_deadline(Instant::now() + Duration::from_secs(300))?;
        let mut first = [0];
        match stream.read(&mut first)? {
            0 => return Ok(None),
            _ => {
                operation(stream)?;
                read_message(&mut first.as_slice().chain(&mut *stream))
            }
        }
    } else {
        read_message(stream)
    };
    match result {
        Err(error) => {
            if let Some(ProtocolError::VersionMismatch { version, id }) = error
                .get_ref()
                .and_then(|e| e.downcast_ref::<ProtocolError>())
            {
                operation(stream)?;
                write_message(stream, *id, &version_mismatch(u16::from(*version)))?;
            }
            Err(error)
        }
        result => result,
    }
}

/// Server side of the `Hello` handshake: the first message must be
/// `Hello` with a supported version. On a version mismatch an
/// `Error{VersionMismatch}` reply is written before failing, so
/// the client can say why it was refused.
pub fn server_handshake<S: DeadlineIo>(stream: &mut S) -> std::io::Result<()> {
    operation(stream)?;
    let Some(envelope) = server_read(stream)? else {
        return Err(io_invalid("connection closed before Hello".into()));
    };
    match envelope.message {
        Message::Request(Request::Hello { version }) if version == PROTOCOL_VERSION => {
            write_message(
                stream,
                envelope.id,
                &Message::Response(Response::HelloOk {
                    version: PROTOCOL_VERSION,
                }),
            )
        }
        Message::Request(Request::Hello { version }) => {
            let _ = write_message(stream, envelope.id, &version_mismatch(version));
            Err(io_invalid(format!(
                "unsupported protocol version {version}"
            )))
        }
        _ => Err(io_invalid("first message was not Hello".into())),
    }
}

/// Serve one connection until the peer closes it: handshake, then
/// each request is handed to `handler`, whose responses are
/// written back under the request's id. A handler that returns no
/// responses gets a typed `Error{Internal}` reply instead of
/// silence. Blocking; run one thread per connection.
pub fn serve_stream<S, F>(mut stream: S, mut handler: F) -> std::io::Result<()>
where
    S: DeadlineIo,
    F: FnMut(Request) -> Vec<Response>,
{
    server_handshake(&mut stream)?;
    while let Some(envelope) = server_read_frame(&mut stream, true)? {
        operation(&mut stream)?;
        let Message::Request(request) = envelope.message else {
            write_message(
                &mut stream,
                envelope.id,
                &Message::Response(Response::Error {
                    code: ErrorCode::BadRequest,
                    msg: "expected a request".into(),
                }),
            )?;
            continue;
        };
        if matches!(request, Request::Hello { .. }) {
            write_message(
                &mut stream,
                envelope.id,
                &Message::Response(Response::Error {
                    code: ErrorCode::BadRequest,
                    msg: "Hello is only valid as the first message".into(),
                }),
            )?;
            continue;
        }
        // Cancel is one-way. Its target request owns any cancellation reply.
        let cancel = matches!(request, Request::Cancel { .. });
        let mut responses = handler(request);
        if cancel {
            continue;
        }
        if responses.is_empty() {
            responses.push(Response::Error {
                code: ErrorCode::Internal,
                msg: "handler produced no response".into(),
            });
        }
        for response in responses {
            operation(&mut stream)?;
            write_message(&mut stream, envelope.id, &Message::Response(response))?;
        }
    }
    Ok(())
}

/// A connected client. Generic over the stream so tests and
/// embedders can use in-process pipes; [`Client::connect`] uses
/// the platform [`Stream`].
pub struct Client<S> {
    stream: S,
    next_id: u32,
    deadline_limit: Option<Instant>,
}

impl Client<Stream> {
    pub fn connect_deadline(
        endpoint: &Endpoint,
        deadline: std::time::Instant,
    ) -> std::io::Result<Client<Stream>> {
        Client::new_limited(
            Stream::connect_deadline(endpoint, deadline)?,
            Some(deadline),
        )
    }

    pub fn set_deadline(&mut self, deadline: std::time::Instant) -> std::io::Result<()> {
        self.deadline_limit = Some(deadline);
        self.stream.set_deadline(deadline)
    }

    /// End an explicit overall budget (for example service discovery). Normal
    /// requests then receive fresh per-operation deadlines.
    pub fn clear_deadline(&mut self) {
        self.deadline_limit = None;
    }

    /// Connect to a service endpoint and perform the handshake.
    pub fn connect(endpoint: &Endpoint) -> std::io::Result<Client<Stream>> {
        Client::new(Stream::connect(endpoint)?)
    }
}

impl<S: DeadlineIo> Client<S> {
    /// Wrap a connected stream and perform the `Hello` handshake.
    /// A server `Error{VersionMismatch}` surfaces as `InvalidData`
    /// carrying the server's message.
    pub fn new(stream: S) -> std::io::Result<Client<S>> {
        Self::new_limited(stream, None)
    }

    fn new_limited(mut stream: S, deadline_limit: Option<Instant>) -> std::io::Result<Client<S>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        stream.set_deadline(deadline_limit.map_or(deadline, |limit| limit.min(deadline)))?;
        write_message(
            &mut stream,
            0,
            &Message::Request(Request::Hello {
                version: PROTOCOL_VERSION,
            }),
        )?;
        match read_message(&mut stream)? {
            Some(Envelope {
                message: Message::Response(Response::HelloOk { version }),
                ..
            }) if version == PROTOCOL_VERSION => Ok(Client {
                stream,
                next_id: 1,
                deadline_limit,
            }),
            Some(Envelope {
                message: Message::Response(Response::Error { code, msg }),
                ..
            }) => Err(io_invalid(format!(
                "server refused Hello ({code:?}): {msg}"
            ))),
            Some(_) => Err(io_invalid("unexpected reply to Hello".into())),
            None => Err(io_invalid("connection closed during Hello".into())),
        }
    }

    fn send(&mut self, request: Request) -> std::io::Result<u32> {
        self.begin_operation()?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        write_message(&mut self.stream, id, &Message::Request(request))?;
        Ok(id)
    }

    fn expect(&mut self, id: u32) -> std::io::Result<Response> {
        self.begin_operation()?;
        match read_message(&mut self.stream)? {
            Some(envelope) if envelope.id == id => match envelope.message {
                Message::Response(response) => Ok(response),
                Message::Request(_) => Err(io_invalid("server sent a request".into())),
            },
            Some(envelope) => Err(io_invalid(format!(
                "response id {} does not match request id {id}",
                envelope.id
            ))),
            None => Err(io_invalid(
                "connection closed waiting for a response".into(),
            )),
        }
    }

    fn check(response: Response) -> std::io::Result<Response> {
        match response {
            Response::Error { code, msg } => {
                Err(io_invalid(format!("server error ({code:?}): {msg}")))
            }
            other => Ok(other),
        }
    }

    fn begin_operation(&mut self) -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        self.stream.set_deadline(
            self.deadline_limit
                .map_or(deadline, |limit| limit.min(deadline)),
        )
    }

    /// Run a query, collecting every `Results` page. `limit = 0`
    /// means no limit.
    pub fn query(&mut self, text: &str, limit: u32) -> std::io::Result<Vec<Vec<u8>>> {
        let id = self.start_query(text, limit)?;
        self.finish_query(id)
    }

    /// Start a query without borrowing the client until completion; its id can
    /// be passed to `cancel` before `finish_query` receives its final response.
    pub fn start_query(&mut self, text: &str, limit: u32) -> std::io::Result<u32> {
        self.send(Request::Query {
            text: text.to_owned(),
            limit,
            sort: Sort::Name,
            flags: 0,
        })
    }

    pub fn finish_query(&mut self, id: u32) -> std::io::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        loop {
            match Self::check(self.expect(id)?)? {
                Response::Results { paths, more } => {
                    out.extend(paths);
                    if !more {
                        return Ok(out);
                    }
                }
                _ => return Err(io_invalid("expected Results".into())),
            }
        }
    }

    /// Count matches for a query text.
    pub fn count(&mut self, text: &str) -> std::io::Result<u64> {
        let id = self.send(Request::Count {
            text: text.to_owned(),
        })?;
        match Self::check(self.expect(id)?)? {
            Response::Count { n } => Ok(n),
            _ => Err(io_invalid("expected Count".into())),
        }
    }

    /// Ask for service status: `(entries, mode, backend report)`.
    pub fn status(&mut self) -> std::io::Result<(u64, String, String)> {
        let id = self.send(Request::Status)?;
        match Self::check(self.expect(id)?)? {
            Response::Status {
                entries,
                mode,
                report,
            } => Ok((entries, mode, report)),
            _ => Err(io_invalid("expected Status".into())),
        }
    }

    /// Send a `Cancel` for an in-flight request id. Fire and
    /// forget: the cancelled request itself reports how it ended.
    pub fn cancel(&mut self, target_id: u32) -> std::io::Result<()> {
        self.send(Request::Cancel { target_id })?;
        Ok(())
    }

    /// The underlying stream (for example to shut it down).
    pub fn into_inner(self) -> S {
        self.stream
    }
}
