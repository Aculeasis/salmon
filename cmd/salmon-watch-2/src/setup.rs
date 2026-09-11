use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tempfile::NamedTempFile;

use crate::cli::SetupOperation;
use crate::config::Config;

const DEFAULT_CONFIG: &[u8] = include_bytes!("../assets/setup/salmon-watch.yml");
const APPLICATION_ICON: &[u8] = include_bytes!("../assets/app-icon.svg");
const DESKTOP_ENTRY_TEMPLATE: &str = include_str!("../assets/setup/salmon-watch.desktop.tpl");
const DESKTOP_EXEC_PLACEHOLDER: &str = "{{EXEC}}";

#[derive(Clone, Debug)]
struct InstallPaths {
    autostart: PathBuf,
    launcher: PathBuf,
    icon: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileResult {
    Created,
    Preserved,
    Reinstalled,
}

pub fn execute(
    output: &mut dyn Write,
    config_filename: &Path,
    operation: SetupOperation,
    reinstall: bool,
) -> Result<()> {
    validate_platform(std::env::consts::OS)?;
    if operation == SetupOperation::CreateConfig {
        let result = install_file(config_filename, DEFAULT_CONFIG, false)?;
        report(output, "configuration", config_filename, result)?;
        return Ok(());
    }
    let needs_config_home = matches!(
        operation,
        SetupOperation::Complete | SetupOperation::InstallAutostart
    );
    let needs_data_home = matches!(
        operation,
        SetupOperation::Complete | SetupOperation::InstallLauncher
    );
    let config_home = if needs_config_home {
        dirs::config_dir().context("determine user configuration directory")?
    } else {
        PathBuf::new()
    };
    let data_home = if needs_data_home {
        dirs::data_dir().context("determine user data directory")?
    } else {
        PathBuf::new()
    };
    let paths = InstallPaths {
        autostart: config_home.join("autostart/salmon-watch.desktop"),
        launcher: data_home.join("applications/salmon-watch.desktop"),
        icon: data_home.join("icons/hicolor/scalable/apps/salmon-watch.svg"),
    };
    let executable = executable_path()?;
    execute_with(
        output,
        config_filename,
        operation,
        reinstall,
        &paths,
        &executable,
        &std::env::temp_dir(),
    )?;
    Ok(())
}

fn execute_with(
    output: &mut dyn Write,
    config_filename: &Path,
    operation: SetupOperation,
    reinstall: bool,
    paths: &InstallPaths,
    executable: &Path,
    backup_parent: &Path,
) -> Result<Option<PathBuf>> {
    let create_config = matches!(
        operation,
        SetupOperation::Complete | SetupOperation::CreateConfig
    );
    let install_autostart = matches!(
        operation,
        SetupOperation::Complete | SetupOperation::InstallAutostart
    );
    let install_launcher = matches!(
        operation,
        SetupOperation::Complete | SetupOperation::InstallLauncher
    );

    if create_config {
        let result = install_file(config_filename, DEFAULT_CONFIG, false)?;
        report(output, "configuration", config_filename, result)?;
    }
    if !install_autostart && !install_launcher {
        return Ok(None);
    }

    let config_filename = std::path::absolute(config_filename).context("resolve config path")?;
    Config::load(&config_filename)
        .with_context(|| format!("validate config at {}", config_filename.display()))?;
    let executable = validate_executable_path(executable, &std::env::temp_dir())?;
    let autostart_entry = desktop_entry(&executable, &config_filename, true)?;
    let launcher_entry = desktop_entry(&executable, &config_filename, false)?;

    let backup_targets = if reinstall {
        let mut targets = Vec::new();
        if install_autostart {
            targets.push((
                &paths.autostart,
                Path::new("autostart/salmon-watch.desktop"),
            ));
        }
        if install_launcher {
            targets.push((
                &paths.launcher,
                Path::new("applications/salmon-watch.desktop"),
            ));
            targets.push((&paths.icon, Path::new("icons/salmon-watch.svg")));
        }
        targets
    } else {
        Vec::new()
    };
    let backup = backup_existing_files(&backup_targets, backup_parent)?;
    if let Some(directory) = &backup {
        writeln!(
            output,
            "Backed up files that will be replaced to {}",
            directory.display()
        )?;
    }

    if install_autostart {
        let result = install_file(&paths.autostart, autostart_entry.as_bytes(), reinstall)?;
        report(output, "desktop autostart entry", &paths.autostart, result)?;
    }
    if install_launcher {
        let icon_result = install_file(&paths.icon, APPLICATION_ICON, reinstall)?;
        report(output, "application icon", &paths.icon, icon_result)?;
        let launcher_result = install_file(&paths.launcher, launcher_entry.as_bytes(), reinstall)?;
        report(
            output,
            "application launcher",
            &paths.launcher,
            launcher_result,
        )?;
    }
    if operation == SetupOperation::Complete {
        writeln!(
            output,
            "\nSalmon Watch is configured and installed for desktop autostart and application-menu launch. To start it now, run:\n\n    {}",
            start_command(&executable, config_filename.as_path())?
        )?;
    }
    Ok(backup)
}

fn validate_platform(os: &str) -> Result<()> {
    if os != "linux" {
        bail!("salmon-watch setup is not implemented on this platform ({os})");
    }
    Ok(())
}

fn executable_path() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locate executable")?;
    executable.canonicalize().context("resolve executable")
}

fn validate_executable_path(executable: &Path, temporary_directory: &Path) -> Result<PathBuf> {
    let executable = std::path::absolute(executable).context("resolve executable")?;
    let temporary_directory =
        std::path::absolute(temporary_directory).context("resolve temporary directory")?;
    if executable.starts_with(&temporary_directory) {
        bail!(
            "refusing to install from temporary executable {}",
            executable.display()
        );
    }
    Ok(executable)
}

fn desktop_entry(executable: &Path, config_filename: &Path, start_hidden: bool) -> Result<String> {
    let executable = executable
        .to_str()
        .context("executable path must be valid UTF-8 for a desktop entry")?;
    let config_filename = config_filename
        .to_str()
        .context("config path must be valid UTF-8 for a desktop entry")?;
    let hidden = if start_hidden { " --start-hidden" } else { "" };
    let command = format!(
        "{} --config {}{}",
        desktop_exec_argument(executable),
        desktop_exec_argument(config_filename),
        hidden,
    );
    render_desktop_entry_template(DESKTOP_ENTRY_TEMPLATE, &command)
}

fn render_desktop_entry_template(template: &str, command: &str) -> Result<String> {
    let mut parts = template.split(DESKTOP_EXEC_PLACEHOLDER);
    let before = parts.next().unwrap_or_default();
    let after = parts
        .next()
        .context("desktop entry template is missing {{EXEC}}")?;
    if parts.next().is_some() {
        bail!("desktop entry template contains {{EXEC}} more than once");
    }
    Ok(format!("{before}{command}{after}"))
}

fn desktop_exec_argument(argument: &str) -> String {
    let mut output = String::with_capacity(argument.len() + 2);
    output.push('"');
    for character in argument.chars() {
        match character {
            '\\' => output.push_str("\\\\\\\\"),
            '"' | '`' => {
                output.push('\\');
                output.push(character);
            }
            '$' => output.push_str("\\\\$"),
            '%' => output.push_str("%%"),
            _ => output.push(character),
        }
    }
    output.push('"');
    output
}

fn shell_argument(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return argument.to_owned();
    }
    format!("'{}'", argument.replace('\'', "'\"'\"'"))
}

fn start_command(executable: &Path, config_filename: &Path) -> Result<String> {
    let executable = shell_argument(
        executable
            .to_str()
            .context("executable path must be valid UTF-8 for a shell command")?,
    );
    let config_text = config_filename
        .to_str()
        .context("config path must be valid UTF-8 for a shell command")?;
    let default_config = crate::config::default_path().ok();
    if default_config.as_deref() == Some(config_filename) {
        Ok(executable)
    } else {
        Ok(format!(
            "{executable} --config {}",
            shell_argument(config_text)
        ))
    }
}

fn install_file(path: &Path, contents: &[u8], replace: bool) -> Result<FileResult> {
    let parent = path
        .parent()
        .context("installation filename has no parent")?;
    fs::create_dir_all(parent).context("create parent directory")?;
    if !replace && path.exists() {
        return Ok(FileResult::Preserved);
    }

    let mut temporary = NamedTempFile::new_in(parent).context("create temporary file")?;
    set_public_file_permissions(temporary.as_file())?;
    temporary.write_all(contents).context("write file")?;
    temporary.flush().context("write file")?;
    let existed = path.exists();
    if replace {
        temporary.persist(path).context("replace file")?;
    } else {
        match temporary.persist_noclobber(path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(FileResult::Preserved);
            }
            Err(error) => return Err(error.error).context("create file"),
        }
    }
    Ok(if existed {
        FileResult::Reinstalled
    } else {
        FileResult::Created
    })
}

fn set_public_file_permissions(file: &fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .context("set file permissions")?;
    }
    Ok(())
}

fn backup_existing_files(
    targets: &[(&PathBuf, &Path)],
    backup_parent: &Path,
) -> Result<Option<PathBuf>> {
    let mut existing = Vec::new();
    for target in targets {
        match fs::symlink_metadata(target.0) {
            Ok(_) => existing.push(target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect file to back up at {}", target.0.display()));
            }
        }
    }
    if existing.is_empty() {
        return Ok(None);
    }
    fs::create_dir_all(backup_parent).context("create backup parent directory")?;
    let temporary = tempfile::Builder::new()
        .prefix("salmon-watch-reinstall-")
        .tempdir_in(backup_parent)
        .context("create reinstall backup directory")?;
    set_private_directory_permissions(temporary.path())?;
    for (source, relative) in existing {
        let destination = temporary.path().join(relative);
        fs::create_dir_all(destination.parent().expect("backup path has a parent"))
            .context("create reinstall backup subdirectory")?;
        fs::copy(source, &destination).with_context(|| {
            format!("back up {} to {}", source.display(), destination.display())
        })?;
    }
    Ok(Some(temporary.keep()))
}

fn set_private_directory_permissions(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .context("set reinstall backup directory permissions")?;
    }
    Ok(())
}

fn report(
    output: &mut dyn Write,
    description: &str,
    path: &Path,
    result: FileResult,
) -> Result<()> {
    match result {
        FileResult::Created => writeln!(output, "Created {description} at {}", path.display())?,
        FileResult::Preserved => writeln!(
            output,
            "{}{} already exists at {}; leaving it unchanged",
            description[..1].to_ascii_uppercase(),
            &description[1..],
            path.display()
        )?,
        FileResult::Reinstalled => {
            writeln!(output, "Reinstalled {description} at {}", path.display())?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestLayout {
        _root: tempfile::TempDir,
        config: PathBuf,
        paths: InstallPaths,
        executable: PathBuf,
        backups: PathBuf,
    }

    impl TestLayout {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            Self {
                config: root.path().join("config/salmon-watch.yml"),
                paths: InstallPaths {
                    autostart: root.path().join("config/autostart/salmon-watch.desktop"),
                    launcher: root.path().join("data/applications/salmon-watch.desktop"),
                    icon: root
                        .path()
                        .join("data/icons/hicolor/scalable/apps/salmon-watch.svg"),
                },
                executable: PathBuf::from("/opt/Salmon Watch/salmon-watch"),
                backups: root.path().join("backups"),
                _root: root,
            }
        }

        fn run(&self, operation: SetupOperation, reinstall: bool) -> (String, Option<PathBuf>) {
            let mut output = Vec::new();
            let backup = execute_with(
                &mut output,
                &self.config,
                operation,
                reinstall,
                &self.paths,
                &self.executable,
                &self.backups,
            )
            .unwrap();
            (String::from_utf8(output).unwrap(), backup)
        }
    }

    #[test]
    fn complete_setup_creates_valid_config_and_distinct_desktop_entries() {
        let layout = TestLayout::new();
        let (output, backup) = layout.run(SetupOperation::Complete, false);
        assert!(backup.is_none());
        Config::load(&layout.config).unwrap();
        assert_eq!(fs::read(&layout.paths.icon).unwrap(), APPLICATION_ICON);

        let autostart = fs::read_to_string(&layout.paths.autostart).unwrap();
        let launcher = fs::read_to_string(&layout.paths.launcher).unwrap();
        for entry in [&autostart, &launcher] {
            assert!(entry.contains("Type=Application"));
            assert!(entry.contains("Icon=salmon-watch"));
            assert!(entry.contains("Terminal=false"));
            assert!(entry.contains("\"/opt/Salmon Watch/salmon-watch\""));
            assert!(entry.contains(&desktop_exec_argument(layout.config.to_str().unwrap())));
            assert!(!entry.contains(DESKTOP_EXEC_PLACEHOLDER));
        }
        assert!(autostart.contains(" --start-hidden\n"));
        assert!(!launcher.contains("--start-hidden"));
        assert!(output.contains("Created configuration"));
        assert!(output.contains("Created desktop autostart entry"));
        assert!(output.contains("Created application icon"));
        assert!(output.contains("Created application launcher"));
        assert!(output.contains("To start it now"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for path in [
                &layout.config,
                &layout.paths.autostart,
                &layout.paths.launcher,
                &layout.paths.icon,
            ] {
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o644
                );
            }
        }
    }

    #[test]
    fn normal_setup_preserves_every_existing_file() {
        let layout = TestLayout::new();
        layout.run(SetupOperation::Complete, false);
        let custom_config = "wsClient:\n  servers:\n    - id: custom\n      addr: localhost:1234\n";
        fs::write(&layout.config, custom_config).unwrap();
        fs::write(&layout.paths.autostart, "custom autostart").unwrap();
        fs::write(&layout.paths.launcher, "custom launcher").unwrap();
        fs::write(&layout.paths.icon, "custom icon").unwrap();

        let (output, backup) = layout.run(SetupOperation::Complete, false);
        assert!(backup.is_none());
        assert_eq!(fs::read_to_string(&layout.config).unwrap(), custom_config);
        assert_eq!(
            fs::read_to_string(&layout.paths.autostart).unwrap(),
            "custom autostart"
        );
        assert_eq!(
            fs::read_to_string(&layout.paths.launcher).unwrap(),
            "custom launcher"
        );
        assert_eq!(
            fs::read_to_string(&layout.paths.icon).unwrap(),
            "custom icon"
        );
        assert!(output.contains("Configuration already exists"));
        assert!(output.contains("Desktop autostart entry already exists"));
        assert!(output.contains("Application icon already exists"));
        assert!(output.contains("Application launcher already exists"));
    }

    #[test]
    fn reinstall_backs_up_and_replaces_desktop_files_but_not_config() {
        let layout = TestLayout::new();
        layout.run(SetupOperation::Complete, false);
        let custom_config = "wsClient:\n  servers:\n    - id: custom\n      addr: localhost:1234\n";
        fs::write(&layout.config, custom_config).unwrap();
        fs::write(&layout.paths.autostart, "old autostart").unwrap();
        fs::write(&layout.paths.launcher, "old launcher").unwrap();
        fs::write(&layout.paths.icon, "old icon").unwrap();

        let (output, backup) = layout.run(SetupOperation::Complete, true);
        let backup = backup.expect("reinstall did not create a backup");
        assert_eq!(
            fs::read_to_string(backup.join("autostart/salmon-watch.desktop")).unwrap(),
            "old autostart"
        );
        assert_eq!(
            fs::read_to_string(backup.join("applications/salmon-watch.desktop")).unwrap(),
            "old launcher"
        );
        assert_eq!(
            fs::read_to_string(backup.join("icons/salmon-watch.svg")).unwrap(),
            "old icon"
        );
        assert_eq!(fs::read_to_string(&layout.config).unwrap(), custom_config);
        assert!(
            fs::read_to_string(&layout.paths.autostart)
                .unwrap()
                .contains("--start-hidden")
        );
        assert!(
            !fs::read_to_string(&layout.paths.launcher)
                .unwrap()
                .contains("--start-hidden")
        );
        assert_eq!(fs::read(&layout.paths.icon).unwrap(), APPLICATION_ICON);
        assert!(output.contains(&format!(
            "Backed up files that will be replaced to {}",
            backup.display()
        )));
        assert!(output.contains("Configuration already exists"));
        assert!(output.contains("Reinstalled desktop autostart entry"));
        assert!(output.contains("Reinstalled application icon"));
        assert!(output.contains("Reinstalled application launcher"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(backup).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn a_backup_failure_prevents_all_replacements() {
        let layout = TestLayout::new();
        fs::create_dir_all(layout.paths.autostart.parent().unwrap()).unwrap();
        fs::create_dir_all(&layout.paths.launcher).unwrap();
        fs::create_dir_all(layout.paths.icon.parent().unwrap()).unwrap();
        fs::write(&layout.paths.autostart, "old autostart").unwrap();
        fs::write(&layout.paths.icon, "old icon").unwrap();
        fs::create_dir_all(layout.config.parent().unwrap()).unwrap();
        fs::write(&layout.config, DEFAULT_CONFIG).unwrap();

        let error = execute_with(
            &mut Vec::new(),
            &layout.config,
            SetupOperation::Complete,
            true,
            &layout.paths,
            &layout.executable,
            &layout.backups,
        )
        .unwrap_err();
        assert!(error.to_string().contains("back up"), "{error:#}");
        assert_eq!(
            fs::read_to_string(&layout.paths.autostart).unwrap(),
            "old autostart"
        );
        assert_eq!(fs::read_to_string(&layout.paths.icon).unwrap(), "old icon");
        assert!(layout.paths.launcher.is_dir());
    }

    #[test]
    fn individual_operations_touch_only_their_own_files() {
        let layout = TestLayout::new();
        let (output, _) = layout.run(SetupOperation::CreateConfig, true);
        assert!(output.contains("Created configuration"));
        assert!(!layout.paths.autostart.exists());
        assert!(!layout.paths.launcher.exists());

        layout.run(SetupOperation::InstallAutostart, false);
        assert!(layout.paths.autostart.exists());
        assert!(!layout.paths.launcher.exists());
        assert!(!layout.paths.icon.exists());

        layout.run(SetupOperation::InstallLauncher, false);
        assert!(layout.paths.launcher.exists());
        assert!(layout.paths.icon.exists());
    }

    #[test]
    fn desktop_and_shell_arguments_are_escaped_like_the_old_client() {
        assert_eq!(
            desktop_exec_argument("a b%\"\\$`"),
            "\"a b%%\\\"\\\\\\\\\\\\$\\`\""
        );
        assert_eq!(shell_argument("/tmp/simple.yml"), "/tmp/simple.yml");
        assert_eq!(
            shell_argument("/tmp/custom config's.yml"),
            "'/tmp/custom config'\"'\"'s.yml'"
        );
    }

    #[test]
    fn desktop_template_requires_exactly_one_exec_placeholder() {
        assert_eq!(
            render_desktop_entry_template("before {{EXEC}} after", "command").unwrap(),
            "before command after"
        );
        assert!(render_desktop_entry_template("no placeholder", "command").is_err());
        assert!(render_desktop_entry_template("{{EXEC}} and {{EXEC}}", "command").is_err());
    }

    #[test]
    fn validates_platform_and_rejects_temporary_executables() {
        validate_platform("linux").unwrap();
        assert!(
            validate_platform("macos")
                .unwrap_err()
                .to_string()
                .contains("macos")
        );
        assert!(
            validate_platform("windows")
                .unwrap_err()
                .to_string()
                .contains("windows")
        );
        assert!(
            validate_executable_path(Path::new("/tmp/build/salmon-watch"), Path::new("/tmp"))
                .unwrap_err()
                .to_string()
                .contains("temporary executable")
        );
        assert_eq!(
            validate_executable_path(Path::new("/opt/salmon-watch"), Path::new("/tmp")).unwrap(),
            Path::new("/opt/salmon-watch")
        );
    }
}
