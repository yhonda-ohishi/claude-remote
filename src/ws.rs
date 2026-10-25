//! Transport over a single WebSocket, so everything fits through Cloudflare Tunnel
//! (which carries HTTP/TCP only).
//!
//! The server answers `GET /` with the viewer page and upgrades `GET /ws` to a WebSocket:
//! - host → viewer: binary video messages (see [`pack`]);
//! - viewer → host: text JSON, either input events ([`InputMsg`]) or `{"t":"keyframe"}`.
//!
//! One viewer at a time; a new one replaces the previous (closed with code 4000).
//! The server has no authentication of its own. It is meant to listen on 127.0.0.1 only, behind
//! `cloudflared` with Cloudflare Access in front.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;
use tungstenite::{Message, WebSocket};

use crate::input::{InputMsg, Injector};

pub struct EncodedFrame {
    /// Annex B H.264 access unit.
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Capture time as Unix milliseconds, so the viewer can measure capture-to-display latency.
    pub captured_unix_ms: f64,
}

/// Connects the encoder to whichever viewer is current.
#[derive(Default)]
pub struct Hub {
    current: Mutex<Option<(u64, Sender<Arc<EncodedFrame>>)>>,
    next_id: AtomicU64,
    keyframe: AtomicBool,
}

impl Hub {
    pub fn has_subscriber(&self) -> bool {
        self.current.lock().unwrap().is_some()
    }

    pub fn publish(&self, frame: EncodedFrame) {
        if let Some((_, tx)) = self.current.lock().unwrap().as_ref() {
            let _ = tx.send(Arc::new(frame));
        }
    }

    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn keyframe_pending(&self) -> bool {
        self.keyframe.load(Ordering::Relaxed)
    }

    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe.swap(false, Ordering::Relaxed)
    }

    fn subscribe(&self, tx: Sender<Arc<EncodedFrame>>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        *self.current.lock().unwrap() = Some((id, tx));
        id
    }

    fn is_current(&self, id: u64) -> bool {
        matches!(self.current.lock().unwrap().as_ref(), Some((current, _)) if *current == id)
    }

    fn unsubscribe(&self, id: u64) {
        let mut current = self.current.lock().unwrap();
        if matches!(current.as_ref(), Some((c, _)) if *c == id) {
            *current = None;
        }
    }
}

/// Accept connections forever, one thread each.
pub fn serve(listener: TcpListener, hub: Arc<Hub>, injector: Arc<Injector>, index_html: &'static str) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (hub, injector) = (hub.clone(), injector.clone());
        thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &hub, &injector, index_html) {
                eprintln!("connection error: {e}");
            }
        });
    }
}

fn handle_connection(
    mut stream: TcpStream,
    hub: &Hub,
    injector: &Injector,
    index_html: &str,
) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let (head_len, path, headers) = peek_request(&stream)?;
    let is_upgrade = headers.get("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    if path == "/ws" && is_upgrade {
        // cloudflared adds Cf-Connecting-Ip; without it the peer is a browser on this PC.
        let remote = headers.get("cf-connecting-ip").cloned();
        let same_pc = remote.is_none();
        let who = remote.unwrap_or_else(|| stream.peer_addr().map(|a| a.to_string()).unwrap_or_default());
        // The handshake is still unread (we only peeked), so tungstenite can parse it itself.
        match tungstenite::accept(stream) {
            Ok(ws) => run_viewer(ws, hub, injector, same_pc, &who),
            Err(e) => eprintln!("websocket handshake failed: {e}"),
        }
        return Ok(());
    }

    let mut head = vec![0u8; head_len];
    stream.read_exact(&mut head)?;
    let (status, content_type, body) = match path.as_str() {
        "/" => ("200 OK", "text/html; charset=utf-8", index_html.as_bytes()),
        _ => ("404 Not Found", "text/plain", &b"not found"[..]),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

/// Read the request line and headers without consuming them from the socket.
/// Returns (length of the head in bytes, path, lower-cased headers).
fn peek_request(stream: &TcpStream) -> std::io::Result<(usize, String, HashMap<String, String>)> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = vec![0u8; 8192];
    for _ in 0..50 {
        let n = stream.peek(&mut buf)?;
        if n == 0 {
            return Err(ErrorKind::UnexpectedEof.into());
        }
        if let Some(end) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]);
            let mut lines = head.split("\r\n");
            let path = lines.next().and_then(|l| l.split(' ').nth(1)).unwrap_or("/").to_string();
            let path = path.split('?').next().unwrap_or("/").to_string();
            let headers = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            stream.set_read_timeout(None)?;
            return Ok((end + 4, path, headers));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(ErrorKind::InvalidData.into())
}

fn run_viewer(mut ws: WebSocket<TcpStream>, hub: &Hub, injector: &Injector, same_pc: bool, who: &str) {
    let (tx, rx) = mpsc::channel();
    let id = hub.subscribe(tx);
    hub.request_keyframe();
    println!("[viewer {id}] connected from {who}{}", if same_pc { " (this PC)" } else { "" });
    // Short read timeout: the same thread alternates between reading input and sending video.
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(2)));

    let mut waiting_for_keyframe = true;
    let reason = loop {
        if !hub.is_current(id) {
            let _ = ws.close(Some(CloseFrame { code: CloseCode::Library(4000), reason: "replaced".into() }));
            let _ = ws.flush();
            break "replaced by a newer viewer";
        }

        match ws.read() {
            Ok(Message::Text(text)) => handle_text(hub, injector, text.as_str(), same_pc, id),
            Ok(Message::Close(_)) => break "closed by viewer",
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break "connection lost",
        }

        let mut frames: Vec<_> = rx.try_iter().collect();
        if frames.len() > 3 {
            // The link can't keep up. Over TCP every queued frame would still be delivered, late;
            // drop them and restart from the next keyframe instead.
            frames.clear();
            waiting_for_keyframe = true;
            hub.request_keyframe();
        }
        let mut failed = false;
        for frame in frames {
            if waiting_for_keyframe && !frame.keyframe {
                continue;
            }
            waiting_for_keyframe = false;
            if ws.send(Message::Binary(pack(&frame).into())).is_err() {
                failed = true;
                break;
            }
        }
        if failed {
            break "send failed";
        }
    };
    hub.unsubscribe(id);
    println!("[viewer {id}] {reason}");
}

/// Video message: `[1u8][keyframe u8][captured_unix_ms f64 LE][Annex B data]`.
fn pack(frame: &EncodedFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + frame.data.len());
    out.push(1);
    out.push(frame.keyframe as u8);
    out.extend_from_slice(&frame.captured_unix_ms.to_le_bytes());
    out.extend_from_slice(&frame.data);
    out
}

fn handle_text(hub: &Hub, injector: &Injector, text: &str, same_pc: bool, id: u64) {
    if text.contains("\"t\":\"keyframe\"") {
        hub.request_keyframe();
        return;
    }
    let msg: InputMsg = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(e) => {
            eprintln!("[viewer {id}] bad message: {e}");
            return;
        }
    };
    // A viewer on this PC: its hover moves would drag the local cursor onto the Claude window.
    if same_pc && matches!(msg, InputMsg::Move { .. }) {
        return;
    }
    // Moves are too frequent to log; typed text is the user's prompt, so log only its length.
    let summary = match &msg {
        InputMsg::Move { .. } => None,
        InputMsg::Text { s } => Some(format!("Text ({} chars)", s.chars().count())),
        other => Some(format!("{other:?}")),
    };
    let result = if same_pc { injector.handle_local(msg) } else { injector.handle(msg) };
    match (result, summary) {
        (Ok(()), Some(s)) => println!("[viewer {id}] input {s}"),
        (Err(why), Some(s)) => eprintln!("[viewer {id}] input dropped ({why}): {s}"),
        _ => {}
    }
}
