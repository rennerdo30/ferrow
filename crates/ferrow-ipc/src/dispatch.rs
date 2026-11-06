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

impl Work {
    fn cancel(&self) {
        // Only the first cancellation owns a wake. Shutdown uses the same
        // transition, so a retained receiver can hold at most one notification.
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            let _ = self.wake.send(());
        }
    }
}

fn reply(writer: &Mutex<Stream>, id: u32, responses: Vec<Response>) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("writer poisoned"))?;
    for response in responses {
        writer.set_deadline(Instant::now() + Duration::from_secs(5))?;
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
            let Some(envelope) = crate::server_read_frame(&mut stream, true)? else {
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
                    work.cancel();
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
        work.cancel();
        work.thread
            .join()
            .map_err(|_| io::Error::other("request panicked"))??;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, Endpoint, Listener, read_message};

    #[test]
    fn redundant_cancel_retains_one_wake_and_shutdown_completes() {
        let endpoint = Endpoint::named(&format!("ferrow-cancel-wake-{}", std::process::id()));
        let listener = Listener::bind(&endpoint).unwrap();
        let (started_tx, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let (observed_tx, observed) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_cancellable(stream, move |context, _| {
                started_tx.send(()).unwrap();
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                observed_tx
                    .send((context.is_cancelled(), context.wake.try_iter().count()))
                    .unwrap();
                vec![Response::Count { n: 0 }]
            })
        });
        let mut client = Client::connect(&endpoint).unwrap();
        let id = client.start_query("held", 1).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..3 {
            client.cancel(id).unwrap();
        }
        let mut stream = client.into_inner();
        // The BadRequest is a barrier: all three Cancels were processed while
        // the original handler still owns its undrained wake receiver.
        write_message(&mut stream, 999, &Message::Request(Request::Status)).unwrap();
        assert_eq!(read_message(&mut stream).unwrap().unwrap().id, 999);
        release.send(()).unwrap();
        let notification = observed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(read_message(&mut stream).unwrap().unwrap().id, id);
        drop(stream);
        server.join().unwrap().unwrap();
        assert_eq!(
            notification,
            (true, 1),
            "redundant cancels queued extra wakes"
        );
    }
}
