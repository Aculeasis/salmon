use std::path::Path;
use anyhow::{Context, Result};

pub const APP_NAME: &str = "Salmon Watch";

/// Checks whether autostart is currently enabled for this user.
pub fn is_autostart_enabled(_config_path: &Path) -> Result<bool> {
    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run = hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        match run {
            Ok(key) => {
                let val: Result<String, _> = key.get_value(APP_NAME);
                Ok(val.is_ok())
            }
            Err(_) => Ok(false),
        }
    }

    #[cfg(target_os = "linux")]
    {
        let path = linux_autostart_path()?;
        Ok(path.exists())
    }

    #[cfg(target_os = "macos")]
    {
        let path = macos_plist_path()?;
        Ok(path.exists())
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Ok(false)
    }
}

/// Enables or disables autostart for the current user.
pub fn set_autostart(enabled: bool, config_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu
            .create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .context("open Windows Run registry key")?;

        if enabled {
            let exe = std::env::current_exe().context("locate current executable")?;
            let is_default_config = crate::config::default_path()
                .ok()
                .as_deref() == Some(config_path);

            let value = if is_default_config {
                format!("\"{}\"", exe.display())
            } else {
                format!("\"{}\" --config \"{}\"", exe.display(), config_path.display())
            };
            key.set_value(APP_NAME, &value)
                .context("write Windows Run registry value")?;
            log::info!("enabled autostart in Windows registry: {value}");
        } else {
            match key.delete_value(APP_NAME) {
                Ok(_) => log::info!("disabled autostart in Windows registry"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("delete Windows Run registry value"),
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        let path = linux_autostart_path()?;
        if enabled {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).context("create autostart directory")?;
            }
            let exe = std::env::current_exe().context("locate current executable")?;
            let content = crate::setup::desktop_entry(&exe, config_path, false)
                .context("generate desktop entry")?;
            std::fs::write(&path, content.as_bytes()).context("write autostart desktop entry")?;
            log::info!("enabled Linux autostart at {}", path.display());
        } else {
            if path.exists() {
                std::fs::remove_file(&path).context("remove autostart desktop entry")?;
                log::info!("disabled Linux autostart at {}", path.display());
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        let path = macos_plist_path()?;
        if enabled {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).context("create LaunchAgents directory")?;
            }
            let exe = std::env::current_exe().context("locate current executable")?;
            let is_default_config = crate::config::default_path()
                .ok()
                .as_deref() == Some(config_path);

            let args_xml = if is_default_config {
                format!(
                    "        <string>{}</string>",
                    exe.display()
                )
            } else {
                format!(
                    "        <string>{}</string>\n        <string>--config</string>\n        <string>{}</string>",
                    exe.display(),
                    config_path.display()
                )
            };

            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.dimonomid.salmon-watch</string>
    <key>ProgramArguments</key>
    <array>
{args_xml}
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
            );
            std::fs::write(&path, plist.as_bytes()).context("write LaunchAgents plist")?;
            log::info!("enabled macOS autostart at {}", path.display());
        } else {
            if path.exists() {
                std::fs::remove_file(&path).context("remove LaunchAgents plist")?;
                log::info!("disabled macOS autostart at {}", path.display());
            }
        }
        Ok(())
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = enabled;
        let _ = config_path;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn linux_autostart_path() -> Result<std::path::PathBuf> {
    let config_dir = dirs::config_dir().context("determine user configuration directory")?;
    Ok(config_dir.join("autostart/salmon-watch.desktop"))
}

#[cfg(target_os = "macos")]
fn macos_plist_path() -> Result<std::path::PathBuf> {
    let home_dir = dirs::home_dir().context("determine user home directory")?;
    Ok(home_dir.join("Library/LaunchAgents/com.dimonomid.salmon-watch.plist"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_autostart_toggle() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("salmon-watch.yml");
        std::fs::write(
            &config_path,
            b"wsClient:\n  servers:\n    - id: test\n      addr: 127.0.0.1:41990\n",
        )
        .unwrap();

        let initial = is_autostart_enabled(&config_path).unwrap();

        // Enable autostart
        set_autostart(true, &config_path).unwrap();
        assert!(is_autostart_enabled(&config_path).unwrap());

        // Disable autostart
        set_autostart(false, &config_path).unwrap();
        assert!(!is_autostart_enabled(&config_path).unwrap());

        // Restore initial state
        if initial {
            let _ = set_autostart(true, &config_path);
        }
    }
}

