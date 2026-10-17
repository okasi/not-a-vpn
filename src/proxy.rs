use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use futures_lite::future;
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};

use crate::doh;
use crate::state;

const MAX_HEAD: usize = 16 * 1024;
const FIRST_CHUNK: usize = 16 * 1024;

pub struct Handle {
    stop: smol::channel::Sender<()>,
    port: u16,
    th: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn stop(mut self) {
        let _ = self.stop.try_send(());
        if let Some(th) = self.th.take() {
            let _ = th.join();
        }
    }
}

/// Bind a local HTTP proxy on an ephemeral port and spawn its executor thread.
pub fn start() -> Result<Handle, String> {
    let (stop_tx, stop_rx) = smol::channel::bounded::<()>(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<u16>();
    let th = std::thread::Builder::new()
        .name("proxy".into())
        .spawn(move || run(stop_rx, ready_tx))
        .map_err(|e| e.to_string())?;
    let port = ready_rx
        .recv()
        .map_err(|_| "proxy thread died before binding".to_string())?;
    Ok(Handle {
        stop: stop_tx,
        port,
        th: Some(th),
    })
}

fn run(stop_rx: smol::channel::Receiver<()>, ready_tx: std::sync::mpsc::Sender<u16>) {
    let ex = smol::Executor::new();
    smol::block_on(async move {
        let listener = match async_net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(l) => l,
            Err(_) => return,
        };
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let _ = ready_tx.send(port);

        let ex_ref = &ex;
        let accept_loop = async move {
            loop {
                match listener.accept().await {
                    Ok((client, _)) => {
                        ex_ref.spawn(handle_client(client)).detach();
                    }
                    Err(_) => {
                        // A transient accept error (e.g. fd exhaustion) must
                        // not become a 100% CPU spin.
                        smol::Timer::after(Duration::from_millis(200)).await;
                    }
                }
            }
        };
        let shutdown = async {
            let _ = stop_rx.recv().await;
        };
        // Stop wakes the race at once; remaining tasks drop with the executor
        // and their sockets close on drop.
        future::race(ex.run(accept_loop), shutdown).await;
    });
}

async fn handle_client(mut client: async_net::TcpStream) {
    let _ = client.set_nodelay(true);
    let mut head = Vec::with_capacity(2048);
    loop {
        let mut tmp = [0u8; 4096];
        match client.read(&mut tmp).await {
            Ok(0) => return,
            Ok(n) => {
                head.extend_from_slice(&tmp[..n]);
                if has_double_crlf(&head) || head.len() > MAX_HEAD {
                    break;
                }
            }
            Err(_) => return,
        }
    }

    let first_line_end = head
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(head.len());
    let line = String::from_utf8_lossy(&head[..first_line_end]);
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("").to_string();

    match method.as_str() {
        "CONNECT" => {
            if let Err(e) = serve_connect(client, &target).await {
                eprintln!("proxy: CONNECT {target}: {e}");
            }
        }
        "GET" | "POST" | "PUT" | "DELETE" | "HEAD" | "OPTIONS" | "PATCH" => {
            if let Err(e) = serve_http(client, &target, head).await {
                eprintln!("proxy: {method} {target}: {e}");
            }
        }
        _ => (),
    }
}

fn has_double_crlf(b: &[u8]) -> bool {
    b.windows(4).any(|w| w == b"\r\n\r\n")
}

fn split_host_port(s: &str) -> Result<(String, u16), String> {
    if let Some(rest) = s.strip_prefix('[') {
        let close = rest.find(']').ok_or("bad authority")?;
        let host = rest[..close].to_string();
        let port = rest[close + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(80);
        return Ok((host, port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), p.parse().map_err(|_| "bad port")?)),
        None => Ok((s.to_string(), 80)),
    }
}

fn host_from_headers(head: &[u8]) -> Option<String> {
    // Only match at a line start so a request line or a header value that
    // happens to contain "host:" cannot feed a bogus destination.
    let mut i = 0;
    while i + 5 < head.len() {
        if i > 0 && head[i - 1] != b'\n' {
            i += 1;
            continue;
        }
        if head[i..i + 5].eq_ignore_ascii_case(b"host:") {
            let start = i + 5;
            let end = head[start..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .map(|p| start + p)
                .unwrap_or(head.len());
            let v = String::from_utf8_lossy(&head[start..end]);
            return Some(v.trim().to_string());
        }
        i += 1;
    }
    None
}

async fn send_502(mut client: async_net::TcpStream) {
    let _ = client
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await;
}

async fn dial(host: &str, port: u16) -> Result<async_net::TcpStream, String> {
    // IP-literal destinations skip DNS entirely: a DoH query for "1.2.3.4"
    // would just come back NXDOMAIN and fail a perfectly valid target.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return async_net::TcpStream::connect(SocketAddr::new(ip, port))
            .await
            .map_err(|e| format!("connect {host}: {e}"))
            .inspect(|s| {
                let _ = s.set_nodelay(true);
            });
    }
    let addrs = doh::resolve(host).await?;
    let mut last_err = String::from("no address");
    for ip in &addrs {
        let stream = async_net::TcpStream::connect(SocketAddr::new(*ip, port)).await;
        match stream {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last_err = format!("{ip}: {e}"),
        }
    }
    Err(format!("connect {host}: {last_err}"))
}

async fn serve_connect(mut client: async_net::TcpStream, authority: &str) -> Result<(), String> {
    let (host, port) = split_host_port(authority)?;
    let target = dial(&host, port).await;

    let upstream = match target {
        Ok(s) => s,
        Err(e) => {
            send_502(client).await;
            return Err(e);
        }
    };

    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .map_err(|e| e.to_string())?;

    // First bytes the client sends inside the tunnel (normally the TLS
    // ClientHello). Fragment them per the selected evasion mode, then splice.
    let mut first = [0u8; FIRST_CHUNK];
    let n = client.read(&mut first).await.map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(());
    }
    let slice = &first[..n];

    let (cr, cw) = futures_lite::io::split(client);
    let (ur, mut uw) = futures_lite::io::split(upstream);

    write_fragmented(&mut uw, slice).await?;
    let c2u = futures_lite::io::copy(cr, uw);
    let u2c = futures_lite::io::copy(ur, cw);
    let _ = future::zip(c2u, u2c).await;
    Ok(())
}

async fn serve_http(
    client: async_net::TcpStream,
    target: &str,
    head: Vec<u8>,
) -> Result<(), String> {
    // Origin-form requests carry the destination in the Host header; some
    // clients send absolute-form.
    let (host, port) = if let Some(rest) = target.strip_prefix("http://") {
        split_host_port(rest.split('/').next().unwrap_or(""))?
    } else {
        let h = host_from_headers(&head).ok_or("no host header")?;
        split_host_port(&h)?
    };
    let upstream = match dial(&host, port).await {
        Ok(s) => s,
        Err(e) => {
            send_502(client).await;
            return Err(e);
        }
    };

    // Fragment the request head per the selected mode; body bytes follow as-is.
    let (cr, cw) = futures_lite::io::split(client);
    let (ur, mut uw) = futures_lite::io::split(upstream);
    let head_len = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(head.len());
    write_fragmented(&mut uw, &head[..head_len]).await?;
    uw.write_all(&head[head_len..])
        .await
        .map_err(|e| e.to_string())?;

    let c2u = futures_lite::io::copy(cr, uw);
    let u2c = futures_lite::io::copy(ur, cw);
    let _ = future::zip(c2u, u2c).await;
    Ok(())
}

// ---- DPI evasion ----

fn get16(b: &[u8], p: usize) -> Option<u16> {
    if p + 2 > b.len() {
        return None;
    }
    Some(u16::from_be_bytes([b[p], b[p + 1]]))
}

/// Locate the SNI bytes inside a TLS ClientHello: byte range of the host name.
fn find_sni(buf: &[u8]) -> Option<(usize, usize)> {
    if buf.len() < 6 || buf[0] != 0x16 || buf[5] != 0x01 {
        return None;
    }
    let end = 5 + get16(buf, 3)? as usize;
    if end > buf.len() {
        return None;
    }
    let mut p = 5 + 1 + 3 + 2 + 32; // handshake header + version + random
    p += *buf.get(p)? as usize + 1; // session id
    p += get16(buf, p)? as usize * 2 + 2; // cipher suites
    p += *buf.get(p)? as usize + 1; // compression
    if p + 2 > end {
        return None;
    }
    let ext_total = get16(buf, p)? as usize;
    p += 2;
    let ext_end = (p + ext_total).min(end);
    while p + 4 <= ext_end {
        let etype = get16(buf, p)?;
        let elen = get16(buf, p + 2)? as usize;
        let body = p + 4;
        if etype == 0 && body + elen <= ext_end {
            // server_name list: u16 list length, then entry: u8 type, u16 len
            let name_len = get16(buf, body + 3)? as usize;
            let start = body + 5;
            let stop = start + name_len;
            if stop <= min(body + elen, buf.len()) {
                return Some((start, stop));
            }
            return None;
        }
        p = body + elen;
    }
    None
}

fn min(a: usize, b: usize) -> usize {
    if a < b { a } else { b }
}

/// Where the first sensitive bytes start: either right before the SNI in a
/// ClientHello, or before the Host header of a plain HTTP request.
fn sensitive_start(data: &[u8]) -> usize {
    if data.first() == Some(&0x16)
        && let Some((s, _)) = find_sni(data)
    {
        return s;
    }
    let mut i = 0;
    while i + 5 < data.len() {
        if data[i] == b'\n' {
            i += 1;
            continue;
        }
        if data[i..].len() >= 5 && data[i..i + 5].eq_ignore_ascii_case(b"host:") {
            return i;
        }
        i += 1;
    }
    data.len() / 2
}

/// Write `data` in DPI-defeating TCP segments per the active evasion mode.
async fn write_fragmented<W>(stream: &mut W, data: &[u8]) -> Result<(), String>
where
    W: AsyncWriteExt + Unpin,
{
    if data.is_empty() {
        return Ok(());
    }
    match state::live_evasion() {
        0 => {
            // SNI split: prefix, sensitive section, remainder.
            let cut = sensitive_start(data);
            stream.write_all(&data[..cut]).await.ok();
            if cut < data.len() {
                stream.write_all(&data[cut..]).await.ok();
            }
        }
        1 => {
            // Small random split near the start of the payload.
            if data.len() > 4 {
                let cut = 1 + fastrand::usize(1..=3);
                let cut = min(cut, data.len() - 1);
                stream.write_all(&data[..cut]).await.ok();
                stream.write_all(&data[cut..]).await.ok();
            } else {
                stream.write_all(data).await.ok();
            }
        }
        2 => {
            // Multi-chunk dribble: several 1-2 byte segments with delays.
            stream.write_all(&data[..1]).await.ok();
            let mut sent = 1usize;
            for chunk in data[1..].chunks(2) {
                smol::Timer::after(Duration::from_millis(6)).await;
                stream.write_all(chunk).await.ok();
                sent += 2;
                if sent >= 16 {
                    break;
                }
            }
            if sent < data.len() {
                stream.write_all(&data[min(sent, data.len())..]).await.ok();
            }
        }
        _ => {
            stream.write_all(data).await.ok();
        }
    }
    stream.flush().await.map_err(|e| e.to_string())
}
