#[cfg(target_os = "windows")]
mod imp {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "not-a-vpn";

    pub fn set(on: bool) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu
            .open_subkey_with_flags(RUN_KEY, winreg::enums::KEY_SET_VALUE)
            .map_err(|e| e.to_string())?;
        if on {
            let exe = std::env::current_exe()
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned();
            key.set_value(VALUE, &exe).map_err(|e| e.to_string())?;
        } else {
            let _ = key.delete_value(VALUE);
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::Command;

    const LABEL: &str = "com.notavpn.proxy";

    fn plist_path() -> Option<std::path::PathBuf> {
        let home = std::env::var("HOME").ok()?;
        Some(
            std::path::PathBuf::from(home)
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
        )
    }

    fn exe_quoted() -> Result<String, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        Ok(exe.to_string_lossy().into_owned())
    }

    fn launchctl(args: &[&str]) {
        let _ = Command::new("/bin/launchctl").args(args).output();
    }

    pub fn set(on: bool) -> Result<(), String> {
        let path = plist_path().ok_or("no HOME")?;
        if on {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let exe = exe_quoted()?;
            let plist = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{exe}</string>\n  </array>\n  <key>RunAtLoad</key><true/>\n</dict>\n</plist>\n"
            );
            std::fs::write(&path, plist).map_err(|e| e.to_string())?;
            launchctl(&["load", "-w", &path.to_string_lossy()]);
            Ok(())
        } else {
            launchctl(&["unload", &path.to_string_lossy()]);
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

pub fn set(on: bool) -> Result<(), String> {
    imp::set(on)
}
