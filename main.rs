// wasmer-probe: minimal WASIX probe for Wasmer Edge.
// Purpose: verify two gateway facts, nothing else. No VLESS, no secrets.
//   GET /                          -> 200 "ok"
//   GET /ws                        -> WebSocket upgrade, echo frames back
//   GET /dial?host=H&port=P        -> outbound TcpStream::connect + minimal HTTP GET, plain-text report
//
// std-only networking on purpose: no axum/tokio, so it builds cleanly for
// wasm32-wasmer-wasi. Sequential accept loop (one connection at a time) is
// fine for a probe.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use sha1::{Digest, Sha1};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(80);
    let listener = TcpListener::bind(("0.0.0.0", port)).expect("bind listener");
    eprintln!("wasmer-probe listening on 0.0.0.0:{}", port);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
                handle(stream);
            }
            Err(e) => eprintln!("accept error: {}", e),
        }
    }
}

/// Buffered connection: keeps bytes already read past the HTTP headers so the
/// WebSocket frame parser sees them.
struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
    pos: usize,
}

impl Conn {
    fn new(stream: TcpStream) -> Self {
        Conn { stream, buf: Vec::new(), pos: 0 }
    }

    /// Fill the buffer with more bytes from the socket. Returns Ok(false) on EOF.
    fn fill(&mut self) -> std::io::Result<bool> {
        let mut chunk = [0u8; 4096];
        let n = self.stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(false);
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(true)
    }

    /// Read exactly `out.len()` bytes, from buffer first, then socket.
    fn read_exact(&mut self, out: &mut [u8]) -> std::io::Result<()> {
        let mut done = 0;
        while done < out.len() {
            if self.pos < self.buf.len() {
                let avail = self.buf.len() - self.pos;
                let take = avail.min(out.len() - done);
                out[done..done + take].copy_from_slice(&self.buf[self.pos..self.pos + take]);
                self.pos += take;
                done += take;
            } else {
                self.buf.clear();
                self.pos = 0;
                if !self.fill()? {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "eof while reading",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Read HTTP request head (up to \r\n\r\n). Returns the raw head bytes.
    fn read_head(&mut self) -> std::io::Result<Vec<u8>> {
        loop {
            if let Some(idx) = find_subslice(&self.buf[self.pos..], b"\r\n\r\n") {
                let end = self.pos + idx + 4;
                let head = self.buf[self.pos..end].to_vec();
                self.pos = end;
                return Ok(head);
            }
            if self.buf.len() - self.pos > MAX_HEADER_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request head too large",
                ));
            }
            if !self.fill()? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof before end of headers",
                ));
            }
        }
    }

    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(data)
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn handle(stream: TcpStream) {
    let mut conn = Conn::new(stream);
    let head = match conn.read_head() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("read head error: {}", e);
            return;
        }
    };
    let head_str = String::from_utf8_lossy(&head);
    let mut lines = head_str.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    if method != "GET" {
        respond(&mut conn, 405, "method not allowed\n");
        return;
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    match path {
        "/" => respond(&mut conn, 200, "ok\n"),
        "/ws" => handle_ws(&mut conn, &headers),
        "/dial" => handle_dial(&mut conn, query),
        _ => respond(&mut conn, 404, "not found\n"),
    }
}

fn respond(conn: &mut Conn, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        body.len(),
        body
    );
    let _ = conn.write_all(resp.as_bytes());
}

fn handle_ws(conn: &mut Conn, headers: &HashMap<String, String>) {
    let upgrade = headers.get("upgrade").map(|s| s.to_ascii_lowercase());
    let key = headers.get("sec-websocket-key").cloned();
    let version = headers.get("sec-websocket-version").cloned();
    if upgrade.as_deref() != Some("websocket") {
        respond(conn, 400, "missing Upgrade: websocket header\n");
        return;
    }
    let key = match key {
        Some(k) if !k.is_empty() => k,
        _ => {
            respond(conn, 400, "missing Sec-WebSocket-Key header\n");
            return;
        }
    };
    if version.as_deref() != Some("13") {
        respond(conn, 400, "unsupported Sec-WebSocket-Version (need 13)\n");
        return;
    }
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = STANDARD.encode(hasher.finalize());
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept
    );
    if conn.write_all(resp.as_bytes()).is_err() {
        return;
    }
    eprintln!("ws: handshake ok, echoing frames");
    ws_echo_loop(conn);
}

/// WebSocket frame echo loop. Accumulates fragmented messages, echoes text(1)
/// and binary(2) back with the same opcode, answers ping(9) with pong(10),
/// replies to close(8) and stops.
fn ws_echo_loop(conn: &mut Conn) {
    let mut msg: Vec<u8> = Vec::new();
    let mut msg_opcode: u8 = 0;
    loop {
        let mut hdr = [0u8; 2];
        if conn.read_exact(&mut hdr).is_err() {
            return;
        }
        let fin = hdr[0] & 0x80 != 0;
        let opcode = hdr[0] & 0x0f;
        let masked = hdr[1] & 0x80 != 0;
        let mut len = (hdr[1] & 0x7f) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            if conn.read_exact(&mut b).is_err() {
                return;
            }
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            if conn.read_exact(&mut b).is_err() {
                return;
            }
            len = u64::from_be_bytes(b);
        }
        if len > 16 * 1024 * 1024 {
            return; // refuse absurd frames
        }
        let mask_key = if masked {
            let mut k = [0u8; 4];
            if conn.read_exact(&mut k).is_err() {
                return;
            }
            Some(k)
        } else {
            None
        };
        let mut payload = vec![0u8; len as usize];
        if conn.read_exact(&mut payload).is_err() {
            return;
        }
        if let Some(k) = mask_key {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= k[i % 4];
            }
        }
        match opcode {
            8 => {
                // close: echo close frame and stop
                let _ = send_frame(conn, true, 8, &payload);
                return;
            }
            9 => {
                // ping -> pong
                if send_frame(conn, true, 10, &payload).is_err() {
                    return;
                }
            }
            10 => { /* pong: ignore */ }
            1 | 2 => {
                if msg.is_empty() {
                    msg_opcode = opcode;
                }
                msg.extend_from_slice(&payload);
                if fin {
                    if send_frame(conn, true, msg_opcode, &msg).is_err() {
                        return;
                    }
                    msg.clear();
                }
            }
            0 => {
                // continuation
                msg.extend_from_slice(&payload);
                if fin {
                    if send_frame(conn, true, msg_opcode, &msg).is_err() {
                        return;
                    }
                    msg.clear();
                }
            }
            _ => {
                // unknown opcode: stop
                return;
            }
        }
    }
}

fn send_frame(conn: &mut Conn, fin: bool, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push((if fin { 0x80 } else { 0x00 }) | (opcode & 0x0f));
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= 0xffff {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    conn.write_all(&out)
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    // domain name or IPv4
    host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !host.starts_with('.')
        && !host.starts_with('-')
        && !host.ends_with('.')
        && !host.ends_with('-')
        && !host.contains("..")
}

fn handle_dial(conn: &mut Conn, query: &str) {
    let mut params: HashMap<String, String> = HashMap::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            params.insert(url_decode(k), url_decode(v));
        }
    }
    let host = params.get("host").cloned().unwrap_or_default();
    let port_s = params.get("port").cloned().unwrap_or_default();
    let port: u16 = match port_s.parse() {
        Ok(p) if p >= 1 => p,
        _ => {
            respond(conn, 400, "bad port: must be 1-65535\n");
            return;
        }
    };
    if !valid_host(&host) {
        respond(conn, 400, "bad host: only domain names or IPv4 allowed\n");
        return;
    }
    let mut report = format!("host: {}\nport: {}\n", host, port);
    let target = format!("{}:{}", host, port);
    let addrs: Vec<std::net::SocketAddr> = match target.to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => {
            report.push_str(&format!("dial: error\nerror: dns resolve failed: {}\n", e));
            respond(conn, 200, &report);
            return;
        }
    };
    if addrs.is_empty() {
        report.push_str("dial: error\nerror: dns resolve returned no address\n");
        respond(conn, 200, &report);
        return;
    }
    let addr = addrs[0];
    report.push_str(&format!("resolved: {}\n", addr));
    let start = Instant::now();
    match TcpStream::connect_timeout(&addr, DIAL_TIMEOUT) {
        Ok(mut stream) => {
            let ms = start.elapsed().as_millis();
            report.push_str(&format!("dial: ok\nconnect_ms: {}\n", ms));
            let _ = stream.set_read_timeout(Some(DIAL_TIMEOUT));
            let _ = stream.set_write_timeout(Some(DIAL_TIMEOUT));
            let req = format!(
                "GET / HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
                host
            );
            match stream.write_all(req.as_bytes()) {
                Ok(()) => {
                    let mut buf = [0u8; 2048];
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            let text = String::from_utf8_lossy(&buf[..n]);
                            let first = text.lines().next().unwrap_or("").to_string();
                            report.push_str(&format!(
                                "http_read_bytes: {}\nhttp_first_line: {}\n",
                                n, first
                            ));
                        }
                        Ok(_) => report.push_str("http_read_bytes: 0\n"),
                        Err(e) => report.push_str(&format!("http_read_error: {}\n", e)),
                    }
                }
                Err(e) => report.push_str(&format!("http_write_error: {}\n", e)),
            }
            respond(conn, 200, &report);
        }
        Err(e) => {
            let ms = start.elapsed().as_millis();
            report.push_str(&format!(
                "dial: error\nconnect_ms: {}\nerror: {}\n",
                ms, e
            ));
            respond(conn, 200, &report);
        }
    }
}
