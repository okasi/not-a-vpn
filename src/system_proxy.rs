#[cfg(target_os = "windows")]
mod imp {
    const INTERNET_OPTION_REFRESH: u32 = 37;
    const INTERNET_OPTION_SETTINGS_CHANGED: u32 = 39;

    fn set_registry(enable: bool, port: u16) -> Result<(), String> {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let settings = hkcu
            .open_subkey_with_flags(
                r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                winreg::enums::KEY_SET_VALUE,
            )
            .map_err(|e| e.to_string())?;
        settings
            .set_value("ProxyEnable", &(if enable { 1u32 } else { 0u32 }))
            .map_err(|e| e.to_string())?;
        if enable {
            settings
                .set_value("ProxyServer", &format!("127.0.0.1:{port}"))
                .map_err(|e| e.to_string())?;
            settings
                .set_value("ProxyOverride", &"<local>;localhost;127.0.0.1")
                .map_err(|e| e.to_string())?;
        }
        // Ask WinInet to notice the change without a reboot (no admin needed).
        unsafe {
            windows_sys::Win32::Networking::WinInet::InternetSetOptionA(
                std::ptr::null_mut(),
                INTERNET_OPTION_SETTINGS_CHANGED,
                std::ptr::null_mut(),
                0,
            );
            windows_sys::Win32::Networking::WinInet::InternetSetOptionA(
                std::ptr::null_mut(),
                INTERNET_OPTION_REFRESH,
                std::ptr::null_mut(),
                0,
            );
        }
        Ok(())
    }

    pub fn enable(port: u16) -> Result<(), String> {
        set_registry(true, port)
    }

    pub fn disable() -> Result<(), String> {
        set_registry(false, 0)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};

    const DIR: &str = "/Library/Application Support/Not A VPN";
    const COMMAND: &str = "/Library/Application Support/Not A VPN/command";
    const HELPER: &str = "/Library/Application Support/Not A VPN/helper.sh";
    const LABEL: &str = "io.github.okasi.not-a-vpn.helper";
    const PLIST: &str = "/Library/LaunchDaemons/io.github.okasi.not-a-vpn.helper.plist";

    /// Runs as root (via LaunchDaemon) whenever the command file is touched.
    /// Reads "enable <port>" or "disable" and configures every network service.
    const HELPER_SH: &str = r#"#!/bin/bash
# not-a-vpn privileged helper; do not edit.
set -u
CMD_FILE="/Library/Application Support/Not A VPN/command"
ACTION="$(head -c 256 "$CMD_FILE" 2>/dev/null || true)"
PORT=""
case "$ACTION" in enable\ *) PORT="$(printf '%s' "$ACTION" | awk '{print $2}')" ;; esac
export PATH=/usr/sbin:/usr/bin:/bin:/sbin
networksetup -listallnetworkservices 2>/dev/null | tail -n +2 | while IFS= read -r SVC; do
  SVC="$(printf '%s' "$SVC" | sed -e 's/^[[:space:]]*//')"
  [ -z "$SVC" ] && continue
  case "$SVC" in \**) continue ;; esac
  if [ -n "$PORT" ]; then
    networksetup -setwebproxy "$SVC" 127.0.0.1 "$PORT"
    networksetup -setsecurewebproxy "$SVC" 127.0.0.1 "$PORT"
  else
    networksetup -setwebproxystate "$SVC" off
    networksetup -setsecurewebproxystate "$SVC" off
  fi
done
exit 0
"#;

    const HELPER_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.github.okasi.not-a-vpn.helper</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>/Library/Application Support/Not A VPN/helper.sh</string>
  </array>
  <key>WatchPaths</key>
  <array>
    <string>/Library/Application Support/Not A VPN/command</string>
  </array>
  <key>RunAtLoad</key>
  <false/>
</dict>
</plist>
"#;

    fn b64(data: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let n = ((chunk[0] as u32) << 16)
                | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
                | (chunk.get(2).copied().unwrap_or(0) as u32);
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                T[((n >> 6) & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                T[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    fn elevated_shell(cmd: &str) -> Result<(), String> {
        let q = format!("\"{}\"", cmd.replace('\\', "\\\\").replace('"', "\\\""));
        let script = format!("do shell script {q} with administrator privileges");
        let status = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(&script)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else if let Some(code) = status.code() {
            if code == -600 {
                Err("canceled".into())
            } else {
                Err(format!("osascript exit {code}"))
            }
        } else {
            Err("osascript failed".into())
        }
    }

    fn installed() -> bool {
        std::fs::metadata(HELPER)
            .map(|m| m.is_file())
            .unwrap_or(false)
            && std::fs::metadata(PLIST)
                .map(|m| m.is_file())
                .unwrap_or(false)
    }

    /// One-time admin prompt: installs the root LaunchDaemon + writable command
    /// file so every later toggle happens without any password.
    fn install() -> Result<(), String> {
        let user = std::env::var("USER").unwrap_or_else(|_| {
            let out = Command::new("/usr/bin/id").arg("-un").output();
            String::from_utf8_lossy(&out.map(|o| o.stdout).unwrap_or_default())
                .trim()
                .to_string()
        });
        let cmd = format!(
            "mkdir -p '{dir}' && chmod 755 '{dir}' \
             && chown {user} '{dir}' \
             && printf '%s' {hb} | openssl base64 -d -A > '{helper}' \
             && chown root:wheel '{helper}' && chmod 755 '{helper}' \
             && touch '{command}' && chown {user} '{command}' && chmod 644 '{command}' \
             && printf '%s' {pb} | openssl base64 -d -A > '{plist}' \
             && chown root:wheel '{plist}' && chmod 644 '{plist}' \
             && launchctl bootout system/{label} > /dev/null 2>&1 || true; \
             launchctl bootstrap system '{plist}'",
            dir = DIR,
            hb = b64(HELPER_SH.as_bytes()),
            helper = HELPER,
            command = COMMAND,
            user = user,
            pb = b64(HELPER_PLIST.as_bytes()),
            plist = PLIST,
            label = LABEL,
        );
        elevated_shell(&cmd)
    }

    /// Atomically hands the helper a new order. The LaunchDaemon's WatchPaths
    /// fires the root script seconds later. Writes go to a temp file first and
    /// are renamed so the helper never reads a half-written command.
    fn write_command(text: &str) -> Result<(), String> {
        let mut tmp = PathBuf::from(COMMAND);
        tmp.set_file_name(".command.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, COMMAND).map_err(|e| e.to_string())
    }

    fn scutil_proxy() -> String {
        Command::new("/usr/sbin/scutil")
            .arg("--proxy")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    fn wait_for(enable: bool, port: u16) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(6);
        let port_line = format!("HTTPPort : {port}");
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(150));
            let text = scutil_proxy();
            if enable {
                if text.contains("HTTPProxy : 127.0.0.1")
                    && text.lines().map(str::trim).any(|l| l == port_line)
                {
                    return Ok(());
                }
            } else if !text.contains("HTTPEnable : 1") {
                return Ok(());
            }
        }
        Err(String::from(
            "the system proxy helper did not apply the change in time",
        ))
    }

    fn apply(order: &str, enable: bool, port: u16) -> Result<(), String> {
        match write_command(order) {
            Ok(()) => wait_for(enable, port),
            Err(_) => {
                // Command file not writable/missing (or first run): install the
                // helper, which is the only time a password is ever needed.
                install()?;
                write_command(order)?;
                wait_for(enable, port)
            }
        }
    }

    pub fn enable(port: u16) -> Result<(), String> {
        apply(&format!("enable {port}\n"), true, port)
    }

    pub fn disable() -> Result<(), String> {
        if !installed() && std::fs::metadata(COMMAND).is_err() {
            // Nothing was ever set through the helper.
            return Ok(());
        }
        apply("disable\n", false, 0)
    }
}

pub fn enable(port: u16) -> Result<(), String> {
    imp::enable(port)
}

pub fn disable() -> Result<(), String> {
    imp::disable()
}
