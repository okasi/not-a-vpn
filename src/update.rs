use std::path::{Path, PathBuf};
use std::process::Command;

use async_native_tls::{TlsConnector, TlsStream};
use async_net::TcpStream;
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};

use crate::doh;

/// Default release repository. Override with NOT_A_VPN_REPO=owner/name.
const REPO: &str = "okasi/not-a-vpn";

const MAX_REDIRECTS: usize = 5;
const MAX_BODY: u64 = 128 * 1024 * 1024;

fn tls_connector() -> &'static TlsConnector {
    static C: std::sync::OnceLock<TlsConnector> = std::sync::OnceLock::new();
    C.get_or_init(TlsConnector::new)
}

// ------------------------------------------------------------ menu glue ----

/// Spawn the check on a background thread so the event loop stays responsive.
pub fn spawn_check() {
    let _ =
        std::thread::Builder::new()
            .name("update".into())
            .spawn(|| match smol::block_on(check()) {
                Ok(()) => {}
                Err(e) => notify(&format!("Update check failed:\n{e}"), false),
            });
}

async fn check() -> Result<(), String> {
    let repo = std::env::var("NOT_A_VPN_REPO").unwrap_or_else(|_| REPO.to_string());
    let release_url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = request(&release_url).await?;
    if resp.status == 404 {
        notify("No published release found yet.", false);
        return Ok(());
    }
    ensure_2xx(&resp, "release lookup")?;

    let release: serde_json::Value =
        serde_json::from_slice(&resp.body).map_err(|e| format!("release json: {e}"))?;
    let latest = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    let current = env!("CARGO_PKG_VERSION");
    if latest.is_empty() || cmp_version(current, &latest) != std::cmp::Ordering::Less {
        notify(
            &format!("You are on the latest version (v{current})."),
            false,
        );
        return Ok(());
    }

    let asset_url = pick_asset(&release, &latest)?;
    notify(&format!("Downloading update v{latest}..."), false);
    let zip = request(&asset_url).await?;
    ensure_2xx(&zip, "update download")?;
    if let Err(e) = verify_checksum(&zip.body, &asset_url).await {
        return Err(format!("refusing to install: {e}"));
    }

    install_update(&zip.body, &latest)?;

    notify(&format!("Updated to v{latest}. Restarting..."), false);
    crate::prepare_shutdown();
    let bundle = bundle_dir().expect("bundle disappeared during update");
    let _ = Command::new("open").arg(&bundle).status();
    std::process::exit(0);
}

fn pick_asset(release: &serde_json::Value, version: &str) -> Result<String, String> {
    let host_arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    };
    let mut best: Option<(&str, u8)> = None;
    for asset in release
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
    {
        let name = asset.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let url = asset
            .get("browser_download_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if name.is_empty() || url.is_empty() || !name.ends_with(".app.zip") {
            continue;
        }
        if !name.contains(&format!("-macos-{version}")) {
            continue;
        }
        let score = if name.contains(&format!("-macos-{version}-{host_arch}")) {
            2
        } else if name.contains(&format!("-macos-{version}-universal")) {
            1
        } else {
            continue;
        };
        if best.map(|b| score > b.1).unwrap_or(true) {
            best = Some((url, score));
        }
    }
    best.map(|b| b.0.to_string())
        .ok_or_else(|| "no macOS asset for this release".to_string())
}

// -------------------------------------------------------- install/swap -----

/// Cross-check the downloaded zip against the hex SHA-256 sidecar published
/// next to the asset ("shasum -a 256" format). A missing sidecar is tolerated
/// (same policy as scripts/install.sh); a published-but-mismatched one is a
/// hard stop.
async fn verify_checksum(zip: &[u8], asset_url: &str) -> Result<(), String> {
    let sidecar = match request(&format!("{asset_url}.sha256")).await {
        Ok(r) if (200..300).contains(&r.status) => r,
        _ => return Ok(()),
    };
    let text = String::from_utf8_lossy(&sidecar.body);
    let expected = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("checksum sidecar is malformed".into());
    }
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(zip)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if hex != expected {
        return Err("SHA-256 mismatch - the download is corrupt or tampered with".to_string());
    }
    Ok(())
}

fn bundle_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut ancestors = exe.ancestors();
    ancestors.next()?; // .../Contents/MacOS/not-a-vpn
    ancestors.next()?; // .../Contents
    let app = ancestors.next()?;
    if app.extension().and_then(|e| e.to_str()) == Some("app") {
        Some(app.to_path_buf())
    } else {
        None
    }
}

/// Write the new zip to a temp dir, unpack it with ditto (which carries no
/// quarantine attributes over), swap it in place of the running bundle and
/// relaunch. Fails cleanly, leaving the current install untouched.
fn install_update(zip_bytes: &[u8], version: &str) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("unsupported platform".into());
    }
    let bundle = bundle_dir().ok_or("not running from an .app bundle (development build?)")?;
    let parent = bundle
        .parent()
        .ok_or("bundle has no parent directory")?
        .to_path_buf();
    fs_write_probe(&parent)?;

    let tmp = temp_dir();
    std::fs::create_dir_all(&tmp).map_err(|e| format!("temp dir: {e}"))?;

    let zip_path = tmp.join(format!("update_{version}.zip"));
    std::fs::write(&zip_path, zip_bytes).map_err(|e| format!("write zip: {e}"))?;

    let unpack = tmp.join("unpack");
    std::fs::create_dir_all(&unpack).map_err(|e| format!("unpack dir: {e}"))?;
    let status = Command::new("ditto")
        .arg("-x")
        .arg("-k")
        .arg(&zip_path)
        .arg(&unpack)
        .status()
        .map_err(|e| format!("ditto: {e}"))?;
    if !status.success() {
        return Err("could not unpack the downloaded update".into());
    }
    let new_bundle = find_app(&unpack).ok_or("update archive has no .app bundle")?;
    strip_quarantine(&new_bundle);

    // Stage on the same volume so both renames are atomic.
    let staging = parent.join(".not-a-vpn-update.app");
    let backup = parent.join(".not-a-vpn-previous.app");
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&backup);
    copy_dir(&new_bundle, &staging)?;

    std::fs::rename(&bundle, &backup)
        .inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&staging);
        })
        .map_err(|e| format!("could not move the old app aside: {e}"))?;
    let swapped = std::fs::rename(&staging, &bundle);
    if swapped.is_err() {
        let _ = std::fs::rename(&backup, &bundle);
        let _ = std::fs::remove_dir_all(&staging);
        return Err("could not move the new app into place".into());
    }
    let _ = std::fs::remove_dir_all(&backup);
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

fn find_app(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().and_then(|x| x.to_str()) == Some("app"))
        .map(|e| e.path())
}

fn strip_quarantine(path: &Path) {
    let _ = Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(path)
        .status();
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir: {e}"))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("read {src:?}: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = dst.join(entry.file_name());
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        if ty.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("copy {:?}: {e}", entry.path()))?;
        }
    }
    Ok(())
}

fn fs_write_probe(dir: &Path) -> Result<bool, String> {
    let probe = dir.join(".not-a-vpn-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(true)
        }
        Err(_) => Err(format!(
            "no write access to {} - install the app in a folder you can write to",
            dir.display()
        )),
    }
}

fn temp_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("not-a-vpn-update-{nanos}"))
}

// ------------------------------------------------------------ dialogs ------

fn notify(msg: &str, _sticky: bool) {
    let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display dialog \"{escaped}\" with title \"not-a-vpn\" buttons {{\"OK\"}} default button \"OK\""
    );
    let _ = Command::new("osascript").args(["-e", &script]).status();
}

// ------------------------------------------------------- HTTP plumbing-----

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn ensure_2xx(resp: &Resp, what: &str) -> Result<(), String> {
    if (200..300).contains(&resp.status) {
        Ok(())
    } else {
        Err(format!("{what} failed (HTTP {})", resp.status))
    }
}

async fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let mut last = format!("no address for {host}");
    if let Ok(addrs) = doh::resolve(host).await {
        for ip in addrs {
            match TcpStream::connect(std::net::SocketAddr::new(ip, port)).await {
                Ok(s) => return Ok(s),
                Err(e) => last = format!("{ip}: {e}"),
            }
        }
    }
    match TcpStream::connect((host, port)).await {
        Ok(s) => Ok(s),
        Err(e) => Err(format!("connect {host}: {e} (last: {last})")),
    }
}

async fn tls_connect(host: &str, port: u16) -> Result<TlsStream<TcpStream>, String> {
    let tcp = connect(host, port).await?;
    tls_connector()
        .connect(host.to_string(), tcp)
        .await
        .map_err(|e| format!("tls {host}: {e}"))
}

async fn request(url: &str) -> Result<Resp, String> {
    let mut current = url.to_string();
    for _ in 0..MAX_REDIRECTS {
        let u = parse_url(&current)?;
        let host = u.host.clone();
        let mut tls = tls_connect(&host, u.port).await?;
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: not-a-vpn/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
            u.path,
            host,
            env!("CARGO_PKG_VERSION")
        );
        tls.write_all(req.as_bytes()).await.ok();
        let mut raw = Vec::with_capacity(8192);
        tls.take(MAX_BODY)
            .read_to_end(&mut raw)
            .await
            .map_err(|e| format!("read {host}: {e}"))?;
        let resp = parse_response(&raw)?;
        if (300..400).contains(&resp.status) {
            let loc = resp
                .header("location")
                .ok_or_else(|| format!("redirect without Location (HTTP {})", resp.status))?
                .to_string();
            current = absolutize(&current, &loc)?;
            continue;
        }
        return Ok(resp);
    }
    Err("too many redirects".into())
}

struct Url {
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("unsupported url {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?,
        ),
        None => (authority.to_string(), 443),
    };
    Ok(Url { host, port, path })
}

fn absolutize(base: &str, loc: &str) -> Result<String, String> {
    if loc.starts_with("https://") {
        Ok(loc.to_string())
    } else if loc.starts_with('/') {
        let u = parse_url(base)?;
        Ok(format!("https://{}{}", u.host, loc))
    } else {
        Err(format!("unhandled redirect target {loc}"))
    }
}

fn parse_response(buf: &[u8]) -> Result<Resp, String> {
    let sep = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("bad http response")?;
    let head = std::str::from_utf8(&buf[..sep]).map_err(|_| "bad http headers")?;
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or("bad http status line")?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let resp = Resp {
        status,
        headers,
        body: buf[sep + 4..].to_vec(),
    };
    if resp
        .header("transfer-encoding")
        .map(|v| v.eq_ignore_ascii_case("chunked"))
        .unwrap_or(false)
    {
        return Ok(Resp {
            status,
            headers: resp.headers,
            body: crate::doh::decode_chunked(&resp.body)?,
        });
    }
    Ok(resp)
}

fn cmp_version(a: &str, b: &str) -> std::cmp::Ordering {
    let numeric = |s: &str| -> Vec<u64> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    numeric(a).cmp(&numeric(b))
}
