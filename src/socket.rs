//! The socket: the only way commands come down and reports go up. A socket
//! that is down loses nothing -- the service pushes every pending command again
//! when it opens, and the host keeps what it has not sent -- it only makes the
//! host slower to hear and be heard until it is back. So this is one thread
//! that keeps a socket open whenever it can, hands down every command it reads,
//! and writes what it is given, saying whether it went.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tungstenite::client::ClientRequestBuilder;
use tungstenite::http::Uri;
use tungstenite::protocol::frame::coding::{Data, OpCode};
use tungstenite::protocol::frame::Frame;
use tungstenite::{Message, WebSocket};

use crate::protocol::{Command, Down, Up};

/// API Gateway closes a socket it has seen nothing on for ten minutes.
const PING: Duration = Duration::from_secs(300);

/// How long one read waits before the thread looks for something to send.
const POLL: Duration = Duration::from_millis(100);

/// How long connecting, the TLS and WebSocket handshakes, and a write that
/// cannot go out may take before the socket is given up on.
const PATIENCE: Duration = Duration::from_secs(15);

/// Between one failed connection and the next try, doubling from the first
/// to the last; reset once a socket has stayed open for `STEADY`. API Gateway
/// ends every socket after two hours, so a reconnect is routine.
const BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));
const STEADY: Duration = Duration::from_secs(60);

/// API Gateway takes a frame of 32 KB at most, and closes the socket on a
/// longer one; a message, up to 128 KB, may come in as many frames as it
/// needs. tungstenite writes a message as one frame, so `write` splits it,
/// leaving room for each frame's header.
const FRAME: usize = 31 << 10;

/// A message to write, and where to say whether it was written.
type Outgoing = (String, Sender<bool>);

/// The host's side of the socket, shared with its thread.
struct Shared {
    url: Mutex<Option<String>>,
    up: AtomicBool,
    /// Counts the sockets opened, so the host can tell one reopened, and
    /// send a whole report on it, from one that stayed open.
    opened: AtomicU64,
}

pub struct Socket {
    shared: Arc<Shared>,
    outbox: Sender<Outgoing>,
    pub commands: Receiver<Command>,
}

impl Socket {
    /// Starts the thread, which waits for the service to name a socket url.
    pub fn spawn(host_id: &str, token: &str) -> Socket {
        let shared = Arc::new(Shared {
            url: Mutex::new(None),
            up: AtomicBool::new(false),
            opened: AtomicU64::new(0),
        });
        let (outbox, outgoing) = mpsc::channel();
        let (incoming, commands) = mpsc::channel();
        let link = Link {
            shared: shared.clone(),
            host_id: host_id.into(),
            authorization: format!("Bearer {token}"),
            outgoing,
            incoming,
        };
        thread::spawn(move || link.run());
        Socket {
            shared,
            outbox,
            commands,
        }
    }

    /// The url the service names in each heartbeat's reply. A new one is
    /// taken at the next connection.
    pub fn point_at(&self, url: Option<&str>) {
        let mut held = self.shared.url.lock().unwrap();
        if held.as_deref() != url {
            *held = url.map(String::from);
        }
    }

    pub fn is_up(&self) -> bool {
        self.shared.up.load(Ordering::SeqCst)
    }

    pub fn opened(&self) -> u64 {
        self.shared.opened.load(Ordering::SeqCst)
    }

    /// Writes `up` on the socket and says whether it was written: false at
    /// once while there is no socket, and false for a write that failed.
    pub fn send(&self, up: &Up) -> bool {
        if !self.is_up() {
            return false;
        }
        let Ok(text) = serde_json::to_string(up) else {
            return false;
        };
        let (done, written) = mpsc::channel();
        // A write is given up on after `PATIENCE`, and one read before it.
        self.outbox.send((text, done)).is_ok()
            && written.recv_timeout(PATIENCE * 2 + POLL).unwrap_or(false)
    }
}

/// The socket's url with this host named, as the service's `$connect` reads it.
pub fn connect_url(url: &str, host_id: &str) -> String {
    let encoded: String = host_id
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    let joiner = if url.contains('?') { '&' } else { '?' };
    format!("{url}{joiner}hostId={encoded}")
}

/// A socket's stream: TLS for `wss`, as the service is reached, and plain for
/// `ws`, as a stand-in on this machine is.
enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => &s.sock,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// The same roots and provider ureq uses, so the host trusts one set of
/// certificates whichever way it reaches the service.
fn tls() -> Result<Arc<rustls::ClientConfig>, String> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Opens the socket at `url`, with the host's bearer in a header: never in
/// the url, which ends up in logs.
fn open(url: &str, authorization: &str) -> Result<WebSocket<Stream>, String> {
    let uri: Uri = url.parse().map_err(|e| format!("{url}: {e}"))?;
    let host = uri.host().ok_or_else(|| format!("{url}: no host"))?;
    let secure = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") => false,
        _ => return Err(format!("{url}: not a ws:// or wss:// url")),
    };
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let address = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host}: no address"))?;
    let tcp = TcpStream::connect_timeout(&address, PATIENCE).map_err(|e| format!("{host}: {e}"))?;
    tcp.set_read_timeout(Some(PATIENCE))
        .map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(PATIENCE))
        .map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let stream = if secure {
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| format!("{host}: {e}"))?;
        let connection =
            rustls::ClientConnection::new(tls()?, name).map_err(|e| format!("{host}: {e}"))?;
        Stream::Tls(Box::new(rustls::StreamOwned::new(connection, tcp)))
    } else {
        Stream::Plain(tcp)
    };
    let request = ClientRequestBuilder::new(uri).with_header("Authorization", authorization);
    let (socket, _) = tungstenite::client(request, stream).map_err(|e| match e {
        tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response)) => format!(
            "refused ({}): {}",
            response.status(),
            String::from_utf8_lossy(response.body().as_deref().unwrap_or_default())
        ),
        e => e.to_string(),
    })?;
    socket
        .get_ref()
        .tcp()
        .set_read_timeout(Some(POLL))
        .map_err(|e| e.to_string())?;
    Ok(socket)
}

/// Whether an error is only a read that found nothing before its timeout.
fn quiet(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(e)
        if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

/// Writes `text` as one message in frames of at most `FRAME` bytes, each
/// ending on a character, so no frame is text that is not whole.
fn write<S: Read + Write>(socket: &mut WebSocket<S>, text: &str) -> tungstenite::Result<()> {
    let (mut rest, mut opcode) = (text, OpCode::Data(Data::Text));
    loop {
        let mut end = rest.len().min(FRAME);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (part, after) = rest.split_at(end);
        let last = after.is_empty();
        socket.write(Message::Frame(Frame::message(
            part.to_owned(),
            opcode,
            last,
        )))?;
        if last {
            return socket.flush();
        }
        (rest, opcode) = (after, OpCode::Data(Data::Continue));
    }
}

/// The thread's side.
struct Link {
    shared: Arc<Shared>,
    host_id: String,
    authorization: String,
    outgoing: Receiver<Outgoing>,
    incoming: Sender<Command>,
}

impl Link {
    fn run(self) {
        let mut wait = BACKOFF.0;
        loop {
            let url = self.shared.url.lock().unwrap().clone();
            let Some(url) = url else {
                self.drain();
                thread::sleep(Duration::from_secs(1));
                continue;
            };
            let opened = Instant::now();
            match open(&connect_url(&url, &self.host_id), &self.authorization) {
                Ok(socket) => {
                    self.drain();
                    self.shared.opened.fetch_add(1, Ordering::SeqCst);
                    self.shared.up.store(true, Ordering::SeqCst);
                    crate::log!("socket open: {url}");
                    let why = self.carry(socket, &url);
                    self.shared.up.store(false, Ordering::SeqCst);
                    self.drain();
                    crate::log!("socket closed: {why}");
                }
                Err(e) => crate::log!("socket could not open: {e}"),
            }
            if self.host_gone() {
                return;
            }
            wait = if opened.elapsed() >= STEADY {
                BACKOFF.0
            } else {
                (wait * 2).min(BACKOFF.1)
            };
            thread::sleep(wait);
        }
    }

    /// Answers what was given to send while there was no socket: not sent,
    /// so the host keeps it for the next.
    fn drain(&self) {
        while let Ok((_, done)) = self.outgoing.try_recv() {
            let _ = done.send(false);
        }
    }

    fn host_gone(&self) -> bool {
        // The host has stopped listening: nothing more to do.
        matches!(self.outgoing.try_recv(), Err(TryRecvError::Disconnected))
    }

    /// Carries messages both ways until the socket ends, and says why it did.
    fn carry(&self, mut socket: WebSocket<Stream>, url: &str) -> String {
        let mut said = Instant::now();
        loop {
            if self.shared.url.lock().unwrap().as_deref() != Some(url) {
                let _ = socket.close(None);
                return "the service named another url".into();
            }
            loop {
                match self.outgoing.try_recv() {
                    Ok((text, done)) => {
                        let sent = write(&mut socket, &text);
                        let _ = done.send(sent.is_ok());
                        if let Err(e) = sent {
                            return e.to_string();
                        }
                        said = Instant::now();
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        let _ = socket.close(None);
                        return "the host is done".into();
                    }
                }
            }
            if said.elapsed() >= PING {
                let ping = serde_json::to_string(&Up::Ping).unwrap_or_default();
                if let Err(e) = socket.send(Message::text(ping)) {
                    return e.to_string();
                }
                said = Instant::now();
            }
            match socket.read() {
                Ok(Message::Text(text)) => match serde_json::from_str::<Down>(&text) {
                    Ok(Down::Command { command }) => {
                        if self.incoming.send(*command).is_err() {
                            return "the host is done".into();
                        }
                    }
                    Ok(Down::Other) => {}
                    Err(e) => crate::log!("socket: unreadable message ({e}): {text}"),
                },
                Ok(Message::Close(frame)) => {
                    return frame.map_or("closed by the service".into(), |f| {
                        format!("closed by the service ({}): {}", f.code, f.reason)
                    })
                }
                Ok(_) => {}
                Err(e) if quiet(&e) => {}
                Err(e) => return e.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CommandReport, WorkerReport};
    use std::net::TcpListener;
    use tungstenite::handshake::server::{Request, Response};

    #[test]
    fn the_url_names_the_host_and_keeps_any_query_it_had() {
        assert_eq!(
            connect_url("wss://s.example/beta", "studio"),
            "wss://s.example/beta?hostId=studio"
        );
        assert_eq!(
            connect_url("wss://s.example/beta?v=1", "my mac/2"),
            "wss://s.example/beta?v=1&hostId=my%20mac%2F2"
        );
    }

    /// API Gateway's limits on what a socket sends it.
    fn api_gateway() -> tungstenite::protocol::WebSocketConfig {
        tungstenite::protocol::WebSocketConfig::default()
            .max_frame_size(Some(32 << 10))
            .max_message_size(Some(128 << 10))
    }

    /// A stand-in for the service's socket, which takes what API Gateway
    /// does: checks how the host connects, pushes `pushed`, and hands back
    /// the first text the host sends.
    // tungstenite's callback answers a refusal with a whole HTTP response.
    #[allow(clippy::result_large_err)]
    fn stand_in(pushed: &'static str) -> (String, thread::JoinHandle<(String, String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/beta", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let (mut path, mut authorization) = (String::new(), String::new());
            let mut socket = tungstenite::accept_hdr_with_config(
                stream,
                |req: &Request, res: Response| {
                    path = req.uri().to_string();
                    authorization = req.headers()["authorization"].to_str().unwrap().into();
                    Ok(res)
                },
                Some(api_gateway()),
            )
            .unwrap();
            socket.send(Message::text(pushed)).unwrap();
            let sent = loop {
                if let Message::Text(text) = socket.read().unwrap() {
                    break text.to_string();
                }
            };
            (path, authorization, sent)
        });
        (url, handle)
    }

    #[test]
    fn a_command_pushed_is_handed_down_and_a_report_goes_up() {
        let (url, service) = stand_in(
            r#"{"type":"command","command":{"id":"c1","kind":"interrupt","workerId":"w1"}}"#,
        );
        let socket = Socket::spawn("studio", "the-token");
        assert!(
            !socket.send(&Up::Ping),
            "nothing goes while there is no socket"
        );
        socket.point_at(Some(&url));
        let command = socket
            .commands
            .recv_timeout(Duration::from_secs(10))
            .expect("the pushed command");
        assert_eq!(
            (command.id.as_str(), command.kind.as_str()),
            ("c1", "interrupt")
        );
        assert!(socket.is_up());
        assert_eq!(socket.opened(), 1);

        let workers = [WorkerReport::new(
            "w1".into(),
            crate::protocol::State::Waiting,
            Some("s1".into()),
        )];
        let results = [CommandReport::new("c1".into(), None)];
        assert!(socket.send(&Up::Report {
            workers: &workers,
            lines: vec![],
            command_results: &results,
        }));
        let (path, authorization, sent) = service.join().unwrap();
        assert_eq!(path, "/beta?hostId=studio");
        assert_eq!(authorization, "Bearer the-token");
        let sent: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(sent["type"], "report");
        assert_eq!(sent["commandResults"][0]["commandId"], "c1");
        assert_eq!(sent["workers"][0]["state"], "waiting");
        assert_eq!(sent["workers"][0]["turn"], "ended");
    }

    #[test]
    fn a_report_longer_than_a_frame_goes_up_whole_in_frames_api_gateway_takes() {
        let (url, service) = stand_in(r#"{"type":"hello"}"#);
        let socket = Socket::spawn("studio", "t");
        socket.point_at(Some(&url));
        let start = Instant::now();
        while !socket.is_up() && start.elapsed() < Duration::from_secs(10) {
            thread::sleep(POLL);
        }
        // Several frames' worth, with characters of every width across them.
        let text = "A reply, é and 日本 and 🦀.\n".repeat(2_500);
        let lines = [(
            "w1".to_string(),
            vec![crate::protocol::Line {
                seq: 0,
                at: "2026-10-28T00:00:00.000Z".into(),
                session_id: "s1".into(),
                from: "agent",
                text: text.clone(),
            }],
        )];
        assert!(socket.send(&Up::Report {
            workers: &[],
            lines: crate::protocol::line_reports(&lines),
            command_results: &[],
        }));
        let (_, _, sent) = service.join().expect("the report, taken whole");
        assert!(sent.len() > 2 * FRAME);
        let sent: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(sent["lines"][0]["text"], text);
    }

    #[test]
    fn a_socket_that_drops_is_opened_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/", listener.local_addr().unwrap());
        let service = thread::spawn(move || {
            for n in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                let text = format!(
                    r#"{{"type":"command","command":{{"id":"c{n}","kind":"stop","workerId":"w1"}}}}"#
                );
                socket.send(Message::text(text)).unwrap();
                // Dropped without a close, as a network that goes away does.
                thread::sleep(Duration::from_millis(200));
            }
        });
        let socket = Socket::spawn("studio", "t");
        socket.point_at(Some(&url));
        for n in 0..2 {
            let command = socket
                .commands
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            assert_eq!(command.id, format!("c{n}"));
        }
        service.join().unwrap();
        assert!(socket.opened() >= 2);
    }

    #[test]
    fn a_refused_connection_says_why() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"message":"Forbidden"}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        });
        let err = open(&url, "Bearer t").err().unwrap();
        assert!(err.contains("403") && err.contains("Forbidden"), "{err}");
    }

    #[test]
    fn tls_is_rustls_with_ring_and_the_webpki_roots() {
        assert!(tls().is_ok());
        let err = open("https://x.example/", "Bearer t").err().unwrap();
        assert!(err.contains("not a ws:// or wss:// url"), "{err}");
    }
}
