use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "Montray";
#[cfg(windows)]
const FORMER_APP_NAME: &str = "Salmon Watch";

/// Checks whether autostart is currently enabled for this user.
pub fn is_autostart_enabled(_config_path: &Path) -> Result<bool> {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::*;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run = hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        match run {
            Ok(key) => Ok(windows_autostart_enabled(&key)),
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
        use winreg::RegKey;
        use winreg::enums::*;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu
            .create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .context("open Windows Run registry key")?;

        if enabled {
            let exe = std::env::current_exe().context("locate current executable")?;
            let config_path = absolute_config_path(config_path)?;
            let is_default_config = is_default_config(&config_path);

            let value = if is_default_config {
                format!("\"{}\"", exe.display())
            } else {
                format!(
                    "\"{}\" --config \"{}\"",
                    exe.display(),
                    config_path.display()
                )
            };
            update_windows_autostart(&key, Some(&value))?;
            log::info!("enabled autostart in Windows registry: {value}");
        } else {
            update_windows_autostart(&key, None)?;
            log::info!("disabled autostart in Windows registry");
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
            let config_path = absolute_config_path(config_path)?;
            let content = crate::setup::desktop_entry(&exe, &config_path, false)
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
            let config_path = absolute_config_path(config_path)?;
            let is_default_config = is_default_config(&config_path);
            let executable = plist_xml_text(&exe.to_string_lossy());
            let config = plist_xml_text(&config_path.to_string_lossy());

            let args_xml = if is_default_config {
                format!("        <string>{executable}</string>")
            } else {
                format!(
                    "        <string>{}</string>\n        <string>--config</string>\n        <string>{}</string>",
                    executable, config
                )
            };

            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.dimonomid.montray-ui</string>
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

#[cfg(windows)]
fn windows_autostart_enabled(key: &winreg::RegKey) -> bool {
    [APP_NAME, FORMER_APP_NAME]
        .iter()
        .any(|name| key.get_value::<String, _>(name).is_ok())
}

#[cfg(windows)]
fn update_windows_autostart(key: &winreg::RegKey, command: Option<&str>) -> Result<()> {
    if let Some(command) = command {
        key.set_value(APP_NAME, &command)
            .context("write Windows Run registry value")?;
    } else {
        match key.delete_value(APP_NAME) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("delete Windows Run registry value"),
        }
    }
    // A renamed installation must not leave two clients starting at login.
    match key.delete_value(FORMER_APP_NAME) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("delete former Windows Run registry value"),
    }
    Ok(())
}

fn absolute_config_path(config_path: &Path) -> Result<PathBuf> {
    std::path::absolute(config_path).context("resolve autostart config path")
}

#[cfg(any(windows, target_os = "macos"))]
fn is_default_config(config_path: &Path) -> bool {
    crate::config::runtime_default_path()
        .and_then(|path| absolute_config_path(&path))
        .is_ok_and(|path| path == config_path)
}

#[cfg(target_os = "macos")]
fn plist_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "linux")]
fn linux_autostart_path() -> Result<std::path::PathBuf> {
    let config_dir = dirs::config_dir().context("determine user configuration directory")?;
    Ok(config_dir.join("autostart/montray-ui.desktop"))
}

#[cfg(target_os = "macos")]
fn macos_plist_path() -> Result<std::path::PathBuf> {
    let home_dir = dirs::home_dir().context("determine user home directory")?;
    Ok(home_dir.join("Library/LaunchAgents/com.dimonomid.montray-ui.plist"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn renamed_windows_autostart_replaces_former_entry_and_disables_both() {
        use winreg::RegKey;
        use winreg::enums::*;

        let root = RegKey::predef(HKEY_CURRENT_USER);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = format!(r"Software\MontrayAutostartTests\{unique}");
        let (key, _) = root.create_subkey(&path).unwrap();
        let result = std::panic::catch_unwind(|| {
            key.set_value(FORMER_APP_NAME, &r#""C:\old\salmon-watch.exe""#)
                .unwrap();
            key.set_value("Other app", &"keep").unwrap();
            assert!(windows_autostart_enabled(&key));

            let command = r#""C:\new\montray-ui.exe" --config "C:\config\salmon-watch.yml""#;
            update_windows_autostart(&key, Some(command)).unwrap();
            assert_eq!(key.get_value::<String, _>(APP_NAME).unwrap(), command);
            assert!(key.get_value::<String, _>(FORMER_APP_NAME).is_err());

            key.set_value(FORMER_APP_NAME, &"old").unwrap();
            update_windows_autostart(&key, None).unwrap();
            assert!(!windows_autostart_enabled(&key));
            assert_eq!(key.get_value::<String, _>("Other app").unwrap(), "keep");
            update_windows_autostart(&key, None).unwrap();
        });
        drop(key);
        root.delete_subkey_all(&path).unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    #[test]
    fn relative_config_paths_are_resolved_without_touching_real_autostart() {
        let resolved = absolute_config_path(Path::new("config/salmon-watch.yml")).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("config/salmon-watch.yml"));
    }
}
