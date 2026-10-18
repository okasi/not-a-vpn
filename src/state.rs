use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    pub host: &'static str,
    pub path: &'static str,
    pub addrs: &'static [&'static str],
}

pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "adguard",
        name: "AdGuard DNS",
        host: "dns.adguard-dns.com",
        path: "/dns-query",
        addrs: &[
            "94.140.14.14",
            "94.140.15.15",
            "2a10:50c0::ad1:ff",
            "2a10:50c0::ad2:ff",
        ],
    },
    Provider {
        id: "controld",
        name: "Control D",
        host: "freedns.controld.com",
        path: "/p2",
        addrs: &["76.76.2.2", "76.76.10.2", "2606:1a40::2", "2606:1a40:1::2"],
    },
    Provider {
        id: "cloudflare",
        name: "Cloudflare Security",
        host: "security.cloudflare-dns.com",
        path: "/dns-query",
        addrs: &[
            "1.1.1.2",
            "1.0.0.2",
            "2606:4700:4700::1112",
            "2606:4700:4700::1002",
        ],
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvasionMode {
    Sni,
    Random,
    Multi,
    Off,
}

impl EvasionMode {
    pub fn label(self) -> &'static str {
        match self {
            EvasionMode::Sni => "Fragment ClientHello (SNI split)",
            EvasionMode::Random => "Small random split",
            EvasionMode::Multi => "Multi-chunk",
            EvasionMode::Off => "Off",
        }
    }

    pub fn all() -> [EvasionMode; 4] {
        [
            EvasionMode::Sni,
            EvasionMode::Random,
            EvasionMode::Multi,
            EvasionMode::Off,
        ]
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Settings {
    pub provider: String,
    #[serde(default = "default_evasion")]
    pub evasion: EvasionMode,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_evasion() -> EvasionMode {
    EvasionMode::Sni
}

fn default_enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            provider: String::from("adguard"),
            evasion: EvasionMode::Sni,
            autostart: false,
            enabled: true,
        }
    }
}

// Live runtime state shared between the tray thread and the proxy thread.
static PROVIDER_IDX: AtomicUsize = AtomicUsize::new(0);
static EVASION_RAW: AtomicU8 = AtomicU8::new(0);

pub fn set_live(provider_id: &str, evasion: EvasionMode) {
    let idx = PROVIDERS
        .iter()
        .position(|p| p.id == provider_id)
        .unwrap_or(0);
    let raw = match evasion {
        EvasionMode::Sni => 0,
        EvasionMode::Random => 1,
        EvasionMode::Multi => 2,
        EvasionMode::Off => 3,
    };
    PROVIDER_IDX.store(idx, Ordering::Relaxed);
    EVASION_RAW.store(raw, Ordering::Relaxed);
}

pub fn live_provider() -> &'static Provider {
    let idx = PROVIDER_IDX.load(Ordering::Relaxed);
    &PROVIDERS[idx.min(PROVIDERS.len() - 1)]
}

pub fn live_evasion() -> u8 {
    EVASION_RAW.load(Ordering::Relaxed)
}

pub fn config_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .or_else(|_| {
            #[cfg(target_os = "macos")]
            {
                std::env::var("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
            }
            #[cfg(not(target_os = "macos"))]
            {
                std::env::var("HOME").map(|h| PathBuf::from(h).join(".config"))
            }
        })
        .unwrap_or_else(|_| PathBuf::from("."));
    let p = base.join("not-a-vpn");
    std::fs::create_dir_all(&p).ok();
    p.join("config.json")
}

pub fn load() -> Settings {
    if let Ok(text) = std::fs::read_to_string(config_path())
        && let Ok(s) = serde_json::from_str::<Settings>(&text)
    {
        return s;
    }
    Settings::default()
}

pub fn save(settings: &Settings) {
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(config_path(), text);
    }
}
