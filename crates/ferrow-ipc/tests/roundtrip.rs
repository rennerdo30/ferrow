//! In-process client/server round trips over a per-test
//! unprivileged pipe or socket, plus version-mismatch handling
//! and OS caller identity.

use ferrow_ipc::protocol::{
    Message, PROTOCOL_VERSION, Request, Response, read_message, write_message,
};
use ferrow_ipc::{Client, Endpoint, ErrorCode, Listener, Stream, serve_stream};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn accepted_stream_can_move_to_a_worker() {
    fn assert_send<T: Send>() {}
    assert_send::<Stream>();
}

#[test]
fn expired_connection_budget_refuses_before_connecting() {
    let error =
        match Stream::connect_with_deadline(&test_endpoint("absent"), std::time::Instant::now()) {
            Err(error) => error,
            Ok(_) => panic!("connected after deadline"),
        };
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn healthy_session_survives_idle_then_active() {
    for cancellable in [false, true] {
        let endpoint = test_endpoint("idle");
        let listener = Listener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            if cancellable {
                ferrow_ipc::serve_cancellable(stream, |_, request| handler(request))
            } else {
                serve_stream(stream, handler)
            }
        });
        let mut client = Client::connect(&endpoint).unwrap();
        assert_eq!(client.count("before").unwrap(), 6);
        std::thread::sleep(std::time::Duration::from_millis(5150));
        let result = client.count("after");
        drop(client);
        let served = server.join().unwrap();
        assert_eq!(result.unwrap(), 5, "idle session lost its next operation");
        served.unwrap();
    }
}

#[cfg(windows)]
#[test]
fn bind_reserves_pipe_name_until_listener_drop() {
    let endpoint = test_endpoint("reservation");
    let listener = Listener::bind(&endpoint).unwrap();
    assert!(
        Listener::bind(&endpoint).is_err(),
        "second bind stole pipe name"
    );
    drop(listener);
    assert!(Listener::bind(&endpoint).is_ok());
}

#[test]
fn two_clients_complete_requests_before_either_closes() {
    let endpoint = test_endpoint("two");
    let listener = Listener::bind(&endpoint).unwrap();
    let ep = endpoint.clone();
    let server = std::thread::spawn(move || {
        let (a, _) = listener.accept().unwrap();
        let a = std::thread::spawn(move || serve_stream(a, handler).unwrap());
        let (b, _) = listener.accept().unwrap();
        let b = std::thread::spawn(move || serve_stream(b, handler).unwrap());
        a.join().unwrap();
        b.join().unwrap();
        #[cfg(windows)]
        assert!(
            Listener::bind(&ep).is_err(),
            "last closed session lost name reservation"
        );
        #[cfg(not(windows))]
        let _ = ep;
    });
    let mut a = Client::connect(&endpoint).unwrap();
    let mut b = Client::connect(&endpoint).unwrap();
    assert_eq!(a.count("first").unwrap(), 5);
    assert_eq!(b.count("second").unwrap(), 6);
    drop(a);
    drop(b);
    server.join().unwrap();
}

#[test]
fn incomplete_frames_expire_at_a_total_deadline() {
    use std::io::Write;
    for bytes in [vec![], vec![128], vec![128, 0, 0, 0, 1]] {
        let endpoint = test_endpoint("deadline");
        let listener = Listener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_deadline(std::time::Instant::now() + std::time::Duration::from_millis(60))
                .unwrap();
            let error = read_message(&mut stream).unwrap_err();
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ),
                "{error}"
            );
        });
        let mut stream = Stream::connect(&endpoint).unwrap();
        stream.write_all(&bytes).unwrap();
        server.join().unwrap();
    }
}

#[test]
fn running_request_observes_cancel_with_its_envelope_id() {
    let endpoint = test_endpoint("cancel");
    let listener = Listener::bind(&endpoint).unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        ferrow_ipc::serve_cancellable(stream, move |context, request| {
            if matches!(request, Request::Query { .. }) {
                started_tx.send(context.id).unwrap();
                assert!(context.wait_cancelled(std::time::Duration::from_secs(1)));
            }
            handler(request)
        })
        .unwrap();
    });
    let mut client = Client::connect(&endpoint).unwrap();
    let id = client.start_query("pending", 1).unwrap();
    assert_eq!(
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        id
    );
    client.cancel(id).unwrap();
    assert!(
        client
            .finish_query(id)
            .unwrap_err()
            .to_string()
            .contains("Cancelled")
    );
    assert_eq!(client.count("next").unwrap(), 4);
    drop(client);
    server.join().unwrap();
}

#[cfg(windows)]
#[test]
fn identity_is_captured_only_after_reading_client_data() {
    use std::io::{Read, Write};
    let endpoint = test_endpoint("identity");
    let listener = Listener::bind(&endpoint).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, diagnostic) = listener.accept().unwrap();
        assert!(
            diagnostic.user_sid.is_none(),
            "primary process token was treated as caller identity"
        );
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        let caller = stream.caller().unwrap();
        assert_eq!(
            caller.user_sid,
            ferrow_ipc::CallerIdentity::current().unwrap().user_sid
        );
        assert!(caller.access_token().is_some());
    });
    let mut stream = Stream::connect(&endpoint).unwrap();
    stream.write_all(&[1]).unwrap();
    server.join().unwrap();
}

#[test]
fn refusal_cleanup_does_not_wait_for_a_reader() {
    let endpoint = test_endpoint("nonreader");
    let listener = Listener::bind(&endpoint).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        assert!(serve_stream(stream, handler).is_err());
        done_tx.send(()).unwrap();
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut stream = loop {
        match Stream::connect(&endpoint) {
            Ok(s) => break s,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("connect: {e}"),
        }
    };
    write_message(
        &mut stream,
        0,
        &Message::Request(Request::Hello { version: 99 }),
    )
    .unwrap();
    let finished = done_rx.recv_timeout(std::time::Duration::from_millis(250));
    drop(stream); // release the peer even when testing the broken implementation
    server.join().unwrap();
    assert!(
        finished.is_ok(),
        "server destruction waited for the non-reading peer"
    );
}

fn test_endpoint(tag: &str) -> Endpoint {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    Endpoint::named(&format!(
        "ferrow-ipc-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn handler(request: Request) -> Vec<Response> {
    match request {
        Request::Query { text, .. } => vec![
            Response::Results {
                paths: vec![format!("/hits/{text}/a.txt").into_bytes()],
                more: true,
            },
            Response::Results {
                paths: vec![format!("/hits/{text}/b.txt").into_bytes()],
                more: false,
            },
        ],
        Request::Count { text } => vec![Response::Count {
            n: text.len() as u64,
        }],
        Request::Status => vec![Response::Status {
            entries: 42,
            mode: "test".into(),
            report: "test backend".into(),
        }],
        Request::Cancel { .. } => vec![Response::Count { n: 0 }],
        Request::Hello { .. } | Request::Page { .. } | Request::Watch { .. } => vec![],
    }
}

#[test]
fn client_server_round_trip_and_caller_identity() {
    let endpoint = test_endpoint("rt");
    let listener = Listener::bind(&endpoint).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, caller) = listener.accept().unwrap();
        // Same process on the other end: the OS must say so where
        // it reports a pid (Windows, Linux).
        if cfg!(any(windows, target_os = "linux")) {
            assert_eq!(
                caller.pid,
                Some(std::process::id()),
                "caller identity: {caller:?}"
            );
        }
        if cfg!(unix) {
            assert!(caller.uid.is_some(), "caller identity: {caller:?}");
        }
        serve_stream(stream, handler).unwrap();
    });

    let mut client = loop {
        match Client::connect(&endpoint) {
            Ok(client) => break client,
            // The pipe instance/socket may not exist yet on a
            // cold start; the accept thread creates it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("connect: {e}"),
        }
    };
    let hits = client.query("report", 0).unwrap();
    assert_eq!(
        hits,
        vec![
            b"/hits/report/a.txt".to_vec(),
            b"/hits/report/b.txt".to_vec()
        ],
        "both Results pages, in order"
    );
    assert_eq!(client.count("abcd").unwrap(), 4);
    client.cancel(7).unwrap();
    assert_eq!(client.count("after cancel").unwrap(), 12);
    assert_eq!(
        client.status().unwrap(),
        (42, "test".to_string(), "test backend".to_string())
    );
    drop(client);
    server.join().unwrap();
}

#[test]
fn version_mismatch_gets_a_typed_error() {
    let endpoint = test_endpoint("ver");
    let listener = Listener::bind(&endpoint).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        // Handshake must refuse the bogus version.
        assert!(serve_stream(stream, handler).is_err());
    });

    let mut stream = loop {
        match Stream::connect(&endpoint) {
            Ok(s) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("connect: {e}"),
        }
    };
    write_message(
        &mut stream,
        0,
        &Message::Request(Request::Hello {
            version: PROTOCOL_VERSION + 100,
        }),
    )
    .unwrap();
    let reply = read_message(&mut stream).unwrap().expect("a refusal reply");
    match reply.message {
        Message::Response(Response::Error { code, msg }) => {
            assert_eq!(code, ErrorCode::VersionMismatch);
            assert!(msg.contains("not supported"), "{msg}");
        }
        other => panic!("expected VersionMismatch error, got {other:?}"),
    }
    drop(stream);
    server.join().unwrap();
}

#[test]
fn default_and_user_endpoints_are_named_as_documented() {
    let service = Endpoint::default_service().display();
    if cfg!(windows) {
        assert_eq!(service, r"\\.\pipe\ferrowd");
    } else if cfg!(target_os = "linux") {
        assert_eq!(service, "/run/ferrow/ferrowd.sock");
    } else if cfg!(target_os = "macos") {
        assert_eq!(service, "/private/var/run/ferrow/ferrowd.sock");
    }
    assert!(Endpoint::user_service().display().contains("ferrowd-user-"));
}
