mod notification;
mod state;
mod tray;

use std::ffi::OsString;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::{CloseRequestResponse, ModelRc, SharedString, Timer, TimerMode, VecModel};

use notification::{DesktopNotificationSink, NotificationSink};
use state::{StateFile, Theme};
use tray::{OverallState, TrayIcons, TrayState};

slint::include_modules!();

fn main() {
    if let Err(error) = execute() {
        eprintln!("salmon-watch-2: {error:#}");
        std::process::exit(1);
    }
}

#[derive(Debug, Default, PartialEq)]
struct CliOptions {
    start_hidden: bool,
    scale: Option<f32>,
    help: bool,
}

fn execute() -> Result<()> {
    let options = parse_cli_options(std::env::args_os().skip(1))?;
    if options.help {
        println!(
            "Usage: salmon-watch-2 [--start-hidden] [--scale FACTOR]\n\nOptions:\n  --start-hidden  Start with the status window hidden\n  --scale FACTOR  Set the UI scale factor (must be greater than zero)\n  -h, --help      Print help"
        );
        return Ok(());
    }
    if let Some(scale) = options.scale {
        // SAFETY: This runs at the beginning of main, before Slint is initialized
        // and before this process creates any threads that could read the
        // environment concurrently.
        unsafe { std::env::set_var("SLINT_SCALE_FACTOR", scale.to_string()) };
    }
    run(options.start_hidden)
}

fn parse_cli_options(args: impl IntoIterator<Item = OsString>) -> Result<CliOptions> {
    let mut options = CliOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--start-hidden") => options.start_hidden = true,
            Some("--scale") => {
                let value = args.next().context("--scale requires a numeric factor")?;
                options.scale = Some(parse_scale(&value)?);
            }
            Some(arg) if arg.starts_with("--scale=") => {
                options.scale = Some(parse_scale(OsString::from(&arg[8..]).as_ref())?);
            }
            Some("-h" | "--help") => options.help = true,
            Some(arg) => anyhow::bail!("unknown argument {arg:?}"),
            None => anyhow::bail!("arguments must be valid UTF-8"),
        }
    }
    Ok(options)
}

fn parse_scale(value: &std::ffi::OsStr) -> Result<f32> {
    let value = value.to_str().context("--scale must be valid UTF-8")?;
    let scale: f32 = value
        .parse()
        .with_context(|| format!("invalid scale factor {value:?}"))?;
    anyhow::ensure!(
        scale.is_finite() && scale > 0.0,
        "scale factor must be greater than zero"
    );
    Ok(scale)
}

fn run(start_hidden: bool) -> Result<()> {
    let state_path = state::default_state_path()?;
    let persisted = state::load(&state_path)?;
    let window = MainWindow::new().context("failed to create native window")?;
    let tray = SalmonTray::new().context("failed to create system tray icon")?;

    apply_preferences(&window, &persisted);
    install_mock_data(&window);
    install_window_callbacks(&window, &state_path);
    window.on_mock_action(|key, action, argument| {
        if argument.is_empty() {
            eprintln!("salmon-watch-2: mocked {action} action for {key}");
        } else {
            eprintln!("salmon-watch-2: mocked {action} action for {key} with {argument}");
        }
    });
    install_tray_callbacks(&window, &tray, Rc::new(DesktopNotificationSink));

    let tray_state = mocked_tray_state();
    let icons = TrayIcons::load()?;
    let solid_icon = icons.icon(tray_state)?;
    let transparent_icon = icons.transparent_icon();
    tray.set_status_title(tray_state.status_title().into());
    tray.set_tray_icon(solid_icon.clone());

    let flash_timer = Timer::default();
    if tray_state.is_flashing() {
        let tray_weak = tray.as_weak();
        let mut showing_solid = true;
        flash_timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
            showing_solid = !showing_solid;
            if let Some(tray) = tray_weak.upgrade() {
                tray.set_tray_icon(if showing_solid {
                    solid_icon.clone()
                } else {
                    transparent_icon.clone()
                });
            }
        });
    }

    if !start_hidden {
        window.show().context("failed to show native window")?;
    }

    // The tray keeps the event loop alive when --start-hidden is used or after
    // the status window is closed.
    slint::run_event_loop().context("native event loop failed")?;
    drop(flash_timer);
    Ok(())
}

fn apply_preferences(window: &MainWindow, state: &StateFile) {
    window.set_dark_theme(state.preferences.theme == Theme::Dark);
    window.set_servers_expanded(state.preferences.sections.servers_expanded);
    window.set_active_incidents_expanded(state.preferences.sections.active_incidents_expanded);
    window.set_snoozed_incidents_expanded(state.preferences.sections.snoozed_incidents_expanded);
}

fn install_window_callbacks(window: &MainWindow, state_path: &std::path::Path) {
    window
        .window()
        .on_close_requested(|| CloseRequestResponse::HideWindow);

    let window_weak = window.as_weak();
    let state_path = state_path.to_owned();
    window.on_preferences_changed(move || {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        match state::load(&state_path) {
            Ok(mut state) => {
                state.preferences.theme = if window.get_dark_theme() {
                    Theme::Dark
                } else {
                    Theme::Light
                };
                state.preferences.sections.servers_expanded = window.get_servers_expanded();
                state.preferences.sections.active_incidents_expanded =
                    window.get_active_incidents_expanded();
                state.preferences.sections.snoozed_incidents_expanded =
                    window.get_snoozed_incidents_expanded();
                if let Err(error) = state::save(&state_path, &state) {
                    eprintln!("salmon-watch-2: failed to save preferences: {error:#}");
                }
            }
            Err(error) => {
                eprintln!("salmon-watch-2: failed to reload preferences: {error:#}");
            }
        }
    });
}

fn install_tray_callbacks(
    window: &MainWindow,
    tray: &SalmonTray,
    notifications: Rc<dyn NotificationSink>,
) {
    let window_weak = window.as_weak();
    tray.on_toggle_window(move || {
        if let Some(window) = window_weak.upgrade() {
            if window.window().is_visible() {
                let _ = window.hide();
            } else {
                let _ = window.show();
            }
        }
    });

    let window_weak = window.as_weak();
    tray.on_open_window(move || {
        if let Some(window) = window_weak.upgrade() {
            let _ = window.show();
        }
    });

    tray.on_example_notification(move || {
        if let Err(error) = notifications.push(
            "Example notification",
            "Salmon Watch desktop notifications are working.",
        ) {
            eprintln!("salmon-watch-2: failed to show example notification: {error:#}");
        }
    });

    tray.on_exit(|| {
        let _ = slint::quit_event_loop();
    });
}

fn install_mock_data(window: &MainWindow) {
    let servers = vec![
        ServerView {
            id: "laptop".into(),
            connection: "online".into(),
            last_heartbeat: "14:25:08 (<15s ago)".into(),
            connected: true,
        },
        ServerView {
            id: "homelab".into(),
            connection: "online".into(),
            last_heartbeat: "14:25:03 (<15s ago)".into(),
            connected: true,
        },
        ServerView {
            id: "office".into(),
            connection: "offline".into(),
            last_heartbeat: "14:23:41 (1m 32s ago)".into(),
            connected: false,
        },
    ];
    window.set_servers(ModelRc::from(Rc::new(VecModel::from(servers))));

    let active = vec![
        IncidentView {
            key: "homelab.systemd.backup.service".into(),
            state: "error".into(),
            details: "backup.service entered failed state".into(),
            started_at: "14:18:02 (7m 11s ago)".into(),
            snoozed_until: SharedString::default(),
            severity: 3,
            stale: false,
            snoozed: false,
        },
        IncidentView {
            key: "office.disk.root".into(),
            state: "warning (STALE)".into(),
            details: "filesystem usage is 86%".into(),
            started_at: "13:42:17 (42m 56s ago)".into(),
            snoozed_until: SharedString::default(),
            severity: 2,
            stale: true,
            snoozed: false,
        },
    ];
    window.set_active_incidents(ModelRc::from(Rc::new(VecModel::from(active))));

    let snoozed = vec![IncidentView {
        key: "laptop.systemd.syncthing.service".into(),
        state: "warning (SNOOZED)".into(),
        details: "syncthing has restarted repeatedly".into(),
        started_at: "13:58:20 (26m 53s ago)".into(),
        snoozed_until: "15:30:00 (in 1h 4m)".into(),
        severity: 2,
        stale: false,
        snoozed: true,
    }];
    window.set_snoozed_incidents(ModelRc::from(Rc::new(VecModel::from(snoozed))));
}

fn mocked_tray_state() -> TrayState {
    // This stage intentionally uses mocked data. The override makes every tray
    // visual easy to inspect without changing source code while the real core
    // is still being implemented.
    let alerting = match std::env::var("SALMON_WATCH_2_MOCK_STATE").as_deref() {
        Ok("unknown") => OverallState::Unknown,
        Ok("ok") => OverallState::Ok,
        Ok("internal-error") => OverallState::InternalError,
        Ok("warning") => OverallState::Warning,
        _ => OverallState::Error,
    };
    TrayState {
        alerting,
        unknown_server_count: usize::from(alerting == OverallState::Unknown),
        server_count: 3,
        alerting_count: 2,
        snoozed: Some(OverallState::Warning),
        snoozed_count: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{CliOptions, parse_cli_options};
    use std::ffi::OsString;

    #[test]
    fn window_is_visible_by_default() {
        assert_eq!(parse_cli_options([]).unwrap(), CliOptions::default());
    }

    #[test]
    fn start_hidden_flag_is_parsed() {
        assert!(
            parse_cli_options([OsString::from("--start-hidden")])
                .unwrap()
                .start_hidden
        );
    }

    #[test]
    fn scale_flag_is_parsed_in_both_forms() {
        assert_eq!(
            parse_cli_options([OsString::from("--scale"), OsString::from("1.25")])
                .unwrap()
                .scale,
            Some(1.25)
        );
        assert_eq!(
            parse_cli_options([OsString::from("--scale=0.75")])
                .unwrap()
                .scale,
            Some(0.75)
        );
    }

    #[test]
    fn invalid_scale_values_are_rejected() {
        for value in ["0", "-1", "NaN", "inf", "wat"] {
            assert!(
                parse_cli_options([OsString::from("--scale"), OsString::from(value)]).is_err(),
                "accepted invalid scale {value}"
            );
        }
        assert!(parse_cli_options([OsString::from("--scale")]).is_err());
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse_cli_options([OsString::from("--wat")]).is_err());
    }
}
