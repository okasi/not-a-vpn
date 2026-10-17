use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use async_native_tls::{TlsConnector, TlsStream};
use async_net::TcpStream;
use futures_lite::future;

use crate::state;

struct CacheEntry {
    host: String,
    addrs: Vec<IpAddr>,
    at: std::time::Instant,
}

static CACHE: OnceLock<Mutex<Vec<CacheEntry>>> = OnceLock::new();
static TLS_CONNECTOR: OnceLock<TlsConnector> = OnceLock::new();

const CACHE_CAP: usize = 512;
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(120);
/// Per-attempt TCP connect limit; an unreachable address must not stall the
/// resolve pipeline for the OS default (~75s).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Whole-query budget (bootstrap + TLS + HTTP + DNS parse).
const QUERY_TIMEOUT: Duration = Duration::from_secs(6);

fn cache() -> &'static Mutex<Vec<CacheEntry>> {
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

fn connector() -> &'static TlsConnector {
    TLS_CONNECTOR.get_or_init(TlsConnector::new)
}

fn encode_qname(host: &str, out: &mut Vec<u8>) -> Result<(), String> {
    for label in host.trim_matches('.').split('.') {
        if label.is_empty() {
            continue;
        }
        let bytes = label.as_bytes();
        if bytes.len() > 63 {
            return Err("dns label too long".into());
        }
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
    }
    out.push(0);
    Ok(())
}

fn build_query(host: &str, qtype: u16) -> Result<Vec<u8>, String> {
    let mut msg = Vec::with_capacity(64);
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis() as u16)
        .unwrap_or(0x1234);
    msg.extend_from_slice(&id.to_be_bytes());
    msg.extend_from_slice(&0x0100_u16.to_be_bytes()); // RD
    msg.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0]); // QDCOUNT=1, rest 0
    encode_qname(host, &mut msg)?;
    msg.extend_from_slice(&qtype.to_be_bytes());
    msg.extend_from_slice(&[0, 1]); // IN
    Ok(msg)
}

fn get_u16(b: &[u8], p: usize) -> Option<u16> {
    if p + 2 > b.len() {
        return None;
    }
    Some(u16::from_be_bytes([b[p], b[p + 1]]))
}

fn skip_name(b: &[u8], mut p: usize) -> Option<usize> {
    let mut hops = 0u8;
    loop {
        let len = *b.get(p)?;
        if len & 0xC0 == 0xC0 {
            return Some(p + 2);
        }
        if len == 0 {
            return Some(p + 1);
        }
        p += 1 + len as usize;
        hops += 1;
        if hops > 32 {
            return None;
        }
    }
}

fn parse_addresses(msg: &[u8]) -> Option<Vec<IpAddr>> {
    if msg.len() < 12 {
        return None;
    }
    let qd = get_u16(msg, 4)? as usize;
    let an = get_u16(msg, 6)? as usize;
    let mut p = 12usize;
    for _ in 0..qd {
        p = skip_name(msg, p)?;
        p += 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        p = skip_name(msg, p)?;
        let rtype = get_u16(msg, p)?;
        p += 2; // type
        p += 2; // class
        p += 4; // ttl
        let rdlen = get_u16(msg, p)? as usize;
        p += 2;
        if p + rdlen > msg.len() {
            break;
        }
        match (rtype, rdlen) {
            (1, 4) => {
                if let Ok(oct) = <[u8; 4]>::try_from(&msg[p..p + 4]) {
                    out.push(IpAddr::from(oct));
                }
            }
            (28, 16) => {
                if let Ok(oct) = <[u8; 16]>::try_from(&msg[p..p + 16]) {
                    out.push(IpAddr::from(oct));
                }
            }
            _ => {}
        }
        p += rdlen;
    }
    Some(out)
}

/// Resolve `host` via the currently selected DoH provider, bypassing the OS
/// resolver entirely (bootstrap directly to the provider IP). Cached briefly.
pub async fn resolve(host: &str) -> Result<Vec<IpAddr>, String> {
    if let Ok(ref mut c) = cache().lock() {
        if let Some(idx) = c
            .iter()
            .position(|e| e.host == host && e.at.elapsed() < CACHE_TTL)
        {
            return Ok(c[idx].addrs.clone());
        }
        c.retain(|e| e.at.elapsed() < CACHE_TTL);
        while c.len() >= CACHE_CAP {
            c.remove(0);
        }
    }

    let addrs = query(host).await?;
    if addrs.is_empty() {
        return Err(format!("DoH: no records for {host}"));
    }
    if let Ok(ref mut c) = cache().lock() {
        c.push(CacheEntry {
            host: host.to_string(),
            addrs: addrs.clone(),
            at: std::time::Instant::now(),
        });
    }
    Ok(addrs)
}

async fn query(host: &str) -> Result<Vec<IpAddr>, String> {
    let mut addrs = query_once(host, 1).await?;
    if addrs.is_empty() {
        addrs = query_once(host, 28).await?;
    }
    Ok(addrs)
}

async fn query_once(host: &str, qtype: u16) -> Result<Vec<IpAddr>, String> {
    let fut = async {
        let prov = state::live_provider();
        let tcp = connect_bootstrap(prov).await?;
        let stream: TlsStream<TcpStream> = connector()
            .connect(prov.host, tcp)
            .await
            .map_err(|e| format!("DoH tls {}: {e}", prov.host))?;

        let body = build_query(host, qtype)?;
        let req = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/dns-message\r\nAccept: application/dns-message\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            prov.path,
            prov.host,
            body.len()
        );

        use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
        let mut tls = stream;
        tls.write_all(req.as_bytes()).await.ok();
        tls.write_all(&body).await.ok();
        let mut resp = Vec::with_capacity(2048);
        tls.take(64 * 1024)
            .read_to_end(&mut resp)
            .await
            .map_err(|e| format!("DoH read: {e}"))?;

        let sep = resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| "DoH: bad http response".to_string())?;
        let lower = String::from_utf8_lossy(&resp[..sep]).to_ascii_lowercase();
        let dns_blob;
        let mut dns_msg: &[u8] = &resp[sep + 4..];
        if lower.contains("transfer-encoding: chunked") {
            dns_blob = decode_chunked(dns_msg)?;
            dns_msg = &dns_blob;
        }
        if dns_msg.len() >= 12 {
            if dns_msg[..2] != body[..2] {
                return Err("DoH: transaction id mismatch".into());
            }
            if dns_msg[3] & 0x0F != 0 {
                return Err(format!("DoH: dns rcode {}", dns_msg[3] & 0x0F));
            }
        }
        parse_addresses(dns_msg).ok_or_else(|| "DoH: malformed dns response".to_string())
    };
    // A hung provider (open TCP but no reply, black-holed TLS handshake...)
    // must not leave a connection stalled forever behind no timeout.
    future::race(fut, async {
        smol::Timer::after(QUERY_TIMEOUT).await;
        Err(String::from("DoH: timed out"))
    })
    .await
}

/// Decode an HTTP/1.1 chunked body. Shared with the updater's HTTP client.
pub(crate) fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("bad chunked encoding")?;
        let size = std::str::from_utf8(&body[..line_end])
            .map_err(|_| "bad chunk size")?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let len = usize::from_str_radix(size, 16).map_err(|_| format!("bad chunk size {size}"))?;
        body = &body[line_end + 2..];
        if len == 0 {
            return Ok(out);
        }
        if body.len() < len + 2 {
            return Err("truncated chunked body".into());
        }
        out.extend_from_slice(&body[..len]);
        body = &body[len + 2..];
    }
}

/// Connect straight to the provider's static IP so no system DNS is ever
/// consulted for the DNS channel itself. IPv4 entries come first.
async fn connect_bootstrap(prov: &state::Provider) -> Result<TcpStream, String> {
    let mut last_err = String::from("no bootstrap address");
    for a in prov.addrs {
        let Ok(ip) = a.parse::<IpAddr>() else {
            continue;
        };
        let target: SocketAddr = SocketAddr::new(ip, 443);
        let attempt = async {
            TcpStream::connect(target)
                .await
                .map_err(|e| format!("{a}: {e}"))
        };
        let timeout = async {
            smol::Timer::after(CONNECT_TIMEOUT).await;
            Err(String::from("timed out"))
        };
        match future::race(attempt, timeout).await {
            Ok(s) => return Ok(s),
            Err(e) => last_err = format!("{a}: {e}"),
        }
    }
    Err(format!("DoH bootstrap failed: {last_err}"))
}
