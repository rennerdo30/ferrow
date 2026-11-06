//! One in-flight operation per connection, with a separate bounded reader so
//! cancellation is received while the operation runs. No detached workers.
use crate::{CallerIdentity, ErrorCode, Message, Request, Response, Stream, write_message};
use std::io;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

pub struct RequestContext {
    pub id: u32,
    pub caller: CallerIdentity,
    cancelled: Arc<AtomicBool>,
    wake: mpsc::Receiver<()>,
}

impl RequestContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub fn wait_cancelled(&self, timeout: Duration) -> bool {
        self.is_cancelled() || self.wake.recv_timeout(timeout).is_ok()
    }
}

struct Work {
    id: u32,
    cancelled: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
    done: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<io::Result<()>>,
}

fn reply(writer: &Mutex<Stream>, id: u32, responses: Vec<Response>) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("writer poisoned"))?;
    writer.set_deadline(Instant::now() + Duration::from_secs(5))?;
    for response in responses {
        write_message(&mut *writer, id, &Message::Response(response))?;
    }
    Ok(())
}

fn error(code: ErrorCode, msg: &str) -> Vec<Response> {
    vec![Response::Error {
        code,
        msg: msg.into(),
    }]
}

pub fn serve_cancellable<F>(mut stream: Stream, handler: F) -> io::Result<()>
where
    F: Fn(RequestContext, Request) -> Vec<Response> + Send + Sync + 'static,
{
    crate::server_handshake(&mut stream)?;
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let handler = Arc::new(handler);
    let mut active: Option<Work> = None;
    let result = (|| {
        loop {
            stream.set_deadline(Instant::now() + Duration::from_secs(5))?;
            let Some(envelope) = crate::server_read(&mut stream)? else {
                return Ok(());
            };
            let Message::Request(request) = envelope.message else {
                reply(
                    &writer,
                    envelope.id,
                    error(ErrorCode::BadRequest, "expected request"),
                )?;
                continue;
            };
            if let Request::Cancel { target_id } = request {
                if let Some(work) = &active
                    && work.id == target_id
                {
                    work.cancelled.store(true, Ordering::Release);
                    let _ = work.wake.send(());
                }
                continue; // no reply under Cancel's own id
            }
            if active
                .as_ref()
                .is_some_and(|w| w.done.load(Ordering::Acquire))
            {
                active
                    .take()
                    .unwrap()
                    .thread
                    .join()
                    .map_err(|_| io::Error::other("request panicked"))??;
            }
            if active.is_some() || matches!(request, Request::Hello { .. }) {
                reply(
                    &writer,
                    envelope.id,
                    error(
                        ErrorCode::BadRequest,
                        "request already active or repeated Hello",
                    ),
                )?;
                continue;
            }
            let id = envelope.id;
            let caller = stream.caller().cloned().ok_or_else(|| {
                io::Error::new(io::ErrorKind::PermissionDenied, "caller unavailable")
            })?;
            let cancelled = Arc::new(AtomicBool::new(false));
            let done = Arc::new(AtomicBool::new(false));
            let (wake_tx, wake) = mpsc::channel();
            let context = RequestContext {
                id,
                caller,
                cancelled: cancelled.clone(),
                wake,
            };
            let h = handler.clone();
            let w = writer.clone();
            let c = cancelled.clone();
            let d = done.clone();
            let thread = std::thread::Builder::new()
                .name("ipc-request".into())
                .spawn(move || {
                    let mut responses = h(context, request);
                    if c.load(Ordering::Acquire) {
                        responses = error(ErrorCode::Cancelled, "request cancelled");
                    } else if responses.is_empty() {
                        responses = error(ErrorCode::Internal, "handler produced no response");
                    }
                    d.store(true, Ordering::Release);
                    reply(&w, id, responses)
                })?;
            active = Some(Work {
                id,
                cancelled,
                done,
                wake: wake_tx,
                thread,
            });
        }
    })();
    if let Some(work) = active {
        work.cancelled.store(true, Ordering::Release);
        let _ = work.wake.send(());
        work.thread
            .join()
            .map_err(|_| io::Error::other("request panicked"))??;
    }
    result
}
