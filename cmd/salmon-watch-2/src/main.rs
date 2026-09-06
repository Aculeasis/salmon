mod notification;
mod state;
mod tray;
mod window_geometry;

use std::ffi::OsString;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::winit_030::WinitWindowAccessor;
use slint::{CloseRequestResponse, ModelRc, SharedString, Timer, TimerMode, VecModel};

use notification::{DesktopNotificationSink, NotificationSink};
use state::{StateFile, Theme};
use tray::{OverallState, TrayIcons, TrayState};
use window_geometry::WindowGeometryManager;

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
    let geometry =
        WindowGeometryManager::new(state_path.clone(), persisted.preferences.window_geometry);

    apply_preferences(&window, &persisted);
    install_mock_data(&window);
    install_window_callbacks(&window, &state_path, geometry.clone());
    window.on_mock_action(|key, action, argument| {
        if argument.is_empty() {
            eprintln!("salmon-watch-2: mocked {action} action for {key}");
        } else {
            eprintln!("salmon-watch-2: mocked {action} action for {key} with {argument}");
        }
    });
    install_tray_callbacks(
        &window,
        &tray,
        Rc::new(DesktopNotificationSink),
        geometry.clone(),
    );
    install_ctrl_c_handler(&tray)?;

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
        geometry.show(window.window())?;
    }

    // The tray keeps the event loop alive when --start-hidden is used or after
    // the status window is closed.
    slint::run_event_loop().context("native event loop failed")?;
    if window.window().is_visible() {
        geometry.save(window.window())?;
    }
    drop(flash_timer);
    Ok(())
}

fn apply_preferences(window: &MainWindow, state: &StateFile) {
    window.set_dark_theme(state.preferences.theme == Theme::Dark);
    window.set_servers_expanded(state.preferences.sections.servers_expanded);
    window.set_active_incidents_expanded(state.preferences.sections.active_incidents_expanded);
    window.set_snoozed_incidents_expanded(state.preferences.sections.snoozed_incidents_expanded);
}

fn install_window_callbacks(
    window: &MainWindow,
    state_path: &std::path::Path,
    geometry: WindowGeometryManager,
) {
    geometry.install_event_handler(window.window());

    let window_weak = window.as_weak();
    window.window().on_close_requested(move || {
        if let Some(window) = window_weak.upgrade()
            && let Err(error) = geometry.hide(window.window())
        {
            eprintln!("salmon-watch-2: failed to hide window: {error:#}");
        }
        CloseRequestResponse::KeepWindowShown
    });

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
    geometry: WindowGeometryManager,
) {
    let window_weak = window.as_weak();
    let geometry_for_toggle = geometry.clone();
    tray.on_toggle_window(move || {
        if let Some(window) = window_weak.upgrade() {
            if window.window().is_visible() {
                if let Err(error) = geometry_for_toggle.hide(window.window()) {
                    eprintln!("salmon-watch-2: failed to hide window: {error:#}");
                }
            } else {
                if let Err(error) = geometry_for_toggle.show(window.window()) {
                    eprintln!("salmon-watch-2: failed to show window: {error:#}");
                }
            }
        }
    });

    let window_weak = window.as_weak();
    let geometry_for_open = geometry.clone();
    tray.on_open_window(move || {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        if let Err(error) = geometry_for_open.show(window.window()) {
            eprintln!("salmon-watch-2: failed to show window: {error:#}");
            return;
        }
        activate_window(window.window());
    });

    tray.on_example_notification(move || {
        if let Err(error) = notifications.push(
            "Example notification",
            "Salmon Watch desktop notifications are working.",
        ) {
            eprintln!("salmon-watch-2: failed to show example notification: {error:#}");
        }
    });

    let window_weak = window.as_weak();
    tray.on_exit(move || {
        if let Some(window) = window_weak.upgrade()
            && window.window().is_visible()
            && let Err(error) = geometry.save(window.window())
        {
            eprintln!("salmon-watch-2: failed to save window geometry: {error:#}");
        }
        let _ = slint::quit_event_loop();
    });
}

fn activate_window(window: &slint::Window) {
    window.with_winit_window(|window| {
        window.set_minimized(false);
        window.focus_window();
    });
}

fn install_ctrl_c_handler(tray: &SalmonTray) -> Result<()> {
    let tray_weak = tray.as_weak();
    ctrlc::set_handler(move || {
        let tray_weak = tray_weak.clone();
        if let Err(error) = tray_weak.upgrade_in_event_loop(|tray| tray.invoke_exit()) {
            eprintln!("salmon-watch-2: failed to request shutdown after Ctrl+C: {error}");
        }
    })
    .context("failed to install Ctrl+C handler")
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
    use crate::state::WindowGeometry;
    use crate::window_geometry::WindowGeometryManager;
    use slint::ComponentHandle;
    use std::ffi::OsString;

    #[test]
    fn window_is_visible_by_default() {
        assert_eq!(parse_cli_options([]).unwrap(), CliOptions::default());
    }

    #[test]
    #[ignore = "requires a real desktop window manager"]
    fn native_startup_restores_geometry_and_maximized_state() {
        let geometry = WindowGeometry {
            x: 120,
            y: 80,
            width: 700,
            height: 500,
            maximized: true,
        };
        let directory = tempfile::tempdir().unwrap();
        let manager =
            WindowGeometryManager::new(directory.path().join("state.json"), Some(geometry));
        let window = super::MainWindow::new().unwrap();
        manager.install_event_handler(window.window());
        manager.show(window.window()).unwrap();

        let maximized = std::rc::Rc::new(std::cell::Cell::new(false));
        let maximized_result = maximized.clone();
        let window_weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
            if let Some(window) = window_weak.upgrade() {
                maximized_result.set(window.window().is_maximized());
                window.window().set_maximized(false);
            }
        });

        let normal_geometry = std::rc::Rc::new(std::cell::Cell::new(None));
        let geometry_result = normal_geometry.clone();
        let window_weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(900), move || {
            if let Some(window) = window_weak.upgrade() {
                let position = window.window().position();
                let size = window.window().size();
                geometry_result.set(Some(WindowGeometry {
                    x: position.x,
                    y: position.y,
                    width: size.width,
                    height: size.height,
                    maximized: false,
                }));
            }
            slint::quit_event_loop().unwrap();
        });
        slint::run_event_loop().unwrap();

        assert!(maximized.get());
        let restored = normal_geometry.get().unwrap();
        assert_eq!((restored.width, restored.height), (700, 500));
        assert!((restored.x - 120).abs() <= 2, "restored x = {}", restored.x);
        assert!((restored.y - 80).abs() <= 2, "restored y = {}", restored.y);
    }

    #[test]
    #[ignore = "requires a real desktop window manager"]
    fn native_hide_show_preserves_normal_geometry_while_maximized() {
        let geometry = WindowGeometry {
            x: 160,
            y: 110,
            width: 680,
            height: 480,
            maximized: false,
        };
        let directory = tempfile::tempdir().unwrap();
        let manager =
            WindowGeometryManager::new(directory.path().join("state.json"), Some(geometry));
        let window = super::MainWindow::new().unwrap();
        manager.install_event_handler(window.window());
        manager.show(window.window()).unwrap();

        let window_weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
            if let Some(window) = window_weak.upgrade() {
                window.window().set_maximized(true);
            }
        });

        let manager_for_hide = manager.clone();
        let window_weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(550), move || {
            if let Some(window) = window_weak.upgrade() {
                manager_for_hide.hide(window.window()).unwrap();
            }
        });

        let manager_for_show = manager.clone();
        let window_weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(700), move || {
            if let Some(window) = window_weak.upgrade() {
                manager_for_show.show(window.window()).unwrap();
            }
        });

        let was_maximized = std::rc::Rc::new(std::cell::Cell::new(false));
        let maximized_result = was_maximized.clone();
        let window_weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(950), move || {
            if let Some(window) = window_weak.upgrade() {
                maximized_result.set(window.window().is_maximized());
                window.window().set_maximized(false);
            }
        });

        let normal_geometry = std::rc::Rc::new(std::cell::Cell::new(None));
        let geometry_result = normal_geometry.clone();
        let window_weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(1250), move || {
            if let Some(window) = window_weak.upgrade() {
                let position = window.window().position();
                let size = window.window().size();
                geometry_result.set(Some(WindowGeometry {
                    x: position.x,
                    y: position.y,
                    width: size.width,
                    height: size.height,
                    maximized: false,
                }));
            }
            slint::quit_event_loop().unwrap();
        });
        // The application tray keeps its event loop alive while hidden. This
        // standalone test needs the explicit until-quit variant instead.
        slint::run_event_loop_until_quit().unwrap();

        assert!(was_maximized.get());
        let restored = normal_geometry.get().unwrap();
        assert_eq!((restored.width, restored.height), (680, 480));
        assert!((restored.x - 160).abs() <= 2, "restored x = {}", restored.x);
        assert!((restored.y - 110).abs() <= 2, "restored y = {}", restored.y);
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
