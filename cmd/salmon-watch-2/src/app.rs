use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cli;
use crate::cli::Command as CliCommand;
use crate::config::{self, Config};
use crate::logging;
use crate::notification::{DesktopNotificationSink, NotificationSink};
use crate::persistence::{self, StateFile, Store, Theme};
use crate::runtime::{Command, RuntimeHandle};
use crate::tray::{FlashCycle, TrayFlashController, TrayIcons};
use crate::ui::{MainWindow, SalmonTray, apply_snapshot};
use crate::window_geometry::WindowGeometryManager;
use anyhow::{Context, Result};
use slint::winit_030::WinitWindowAccessor;
use slint::{CloseRequestResponse, ComponentHandle, Timer};

/// Parses the process mode and dispatches setup, token generation, or the GUI.
///
/// Logging and Slint are initialized only for GUI mode. An explicit scale is
/// exported before Slint or any worker thread exists; mutating the environment
/// later would be both ineffective and unsafe in a multithreaded process.
pub fn execute() -> Result<()> {
    let options = cli::parse_env();
    if options.version {
        print!("{}", crate::build_info::full_description());
        return Ok(());
    }
    if let CliCommand::GenerateBearerToken { server_id, output } = &options.command {
        let config_path = options.config.clone().unwrap_or(config::default_path()?);
        let stdout = std::io::stdout();
        let mut output_stream = stdout.lock();
        crate::bearer_token::generate(
            &mut output_stream,
            &config_path,
            server_id,
            output.as_deref(),
        )?;
        return Ok(());
    }
    if let CliCommand::Setup {
        operation,
        reinstall,
    } = options.command
    {
        let config_path = options.config.clone().unwrap_or(config::default_path()?);
        let stdout = std::io::stdout();
        let mut output_stream = stdout.lock();
        crate::setup::execute(&mut output_stream, &config_path, operation, reinstall)?;
        return Ok(());
    }
    logging::init(options.log_level)?;
    let automatic_scale = options.scale.is_none()
        && std::env::var_os("SLINT_SCALE_FACTOR").is_none()
        && std::env::var_os("WINIT_X11_SCALE_FACTOR").is_none();
    if let Some(scale) = options.scale {
        // SAFETY: This runs at the beginning of main, before Slint is initialized
        // and before this process creates any threads that could read the
        // environment concurrently.
        unsafe { std::env::set_var("SLINT_SCALE_FACTOR", scale.to_string()) };
    }
    let config_path = options.config.unwrap_or(config::default_path()?);
    run(options.start_hidden, config_path, automatic_scale)
}

/// Constructs long-lived UI/runtime resources and performs ordered teardown.
///
/// The native event loop remains on the main thread; Tokio owns a separate OS
/// thread. On exit, visible geometry is saved before networking is synchronously
/// stopped, so a normal return means tunnels and child processes are gone.
fn run(start_hidden: bool, config_path: PathBuf, automatic_scale: bool) -> Result<()> {
    let config = Config::load(&config_path)?;
    log::info!(
        "starting with config {} ({} servers, window {})",
        config_path.display(),
        config.ws_client.servers.len(),
        if start_hidden { "hidden" } else { "visible" }
    );
    let state_path = persistence::default_state_path()?;
    log::debug!("loading persisted state from {}", state_path.display());
    let store = Store::new(state_path);
    let persisted = store.load()?;
    let snoozes = persisted.decoded_snoozes()?;
    let window = MainWindow::new().context("failed to create native window")?;
    install_scale_logging(&window, automatic_scale);
    let tray = SalmonTray::new().context("failed to create system tray icon")?;
    log::info!("UI and system tray initialized");
    let geometry = WindowGeometryManager::new(store.clone(), persisted.preferences.window_geometry);

    apply_preferences(&window, &persisted);
    install_window_callbacks(&window, store.clone(), geometry.clone());
    install_tray_callbacks(
        &window,
        &tray,
        Rc::new(DesktopNotificationSink),
        geometry.clone(),
    );
    install_termination_handler(&tray)?;

    let icons = Arc::new(TrayIcons::load()?);
    let flash = TrayFlashController::default();
    let window_weak = window.as_weak();
    let tray_weak = tray.as_weak();
    let publish_icons = icons.clone();
    let publish_flash = flash.clone();
    let publish = Arc::new(move |snapshot| {
        let tray_weak = tray_weak.clone();
        let icons = publish_icons.clone();
        let flash = publish_flash.clone();
        if let Err(error) = window_weak.upgrade_in_event_loop(move |window| {
            if let Some(tray) = tray_weak.upgrade()
                && let Some(cycle) = apply_snapshot(&window, &tray, &icons, &flash, snapshot)
            {
                schedule_flash_tick(&window, &tray, icons.transparent_icon(), flash, cycle);
            }
        }) {
            log::error!("failed to publish UI state: {error}");
        }
    });
    let persist_store = store.clone();
    let persist = Arc::new(
        move |snoozes: &std::collections::BTreeMap<String, time::OffsetDateTime>| {
            persist_store.update(|state| state.replace_snoozes(snoozes))
        },
    );
    let runtime = RuntimeHandle::start(config, snoozes, publish, persist)?;
    log::debug!("application runtime started");
    install_incident_actions(&window, runtime.commands());

    if !start_hidden {
        geometry.show(window.window())?;
    }

    // The tray keeps the event loop alive when --start-hidden is used or after
    // the status window is closed.
    let event_loop_result = slint::run_event_loop().context("native event loop failed");
    log::info!("shutting down");
    let geometry_result = if window.window().is_visible() {
        geometry.save(window.window())
    } else {
        Ok(())
    };
    if let Err(error) = &geometry_result {
        log::error!("failed to save window geometry during shutdown: {error:#}");
    }
    runtime.shutdown();
    log::info!("shutdown complete");
    event_loop_result?;
    geometry_result?;
    Ok(())
}

/// Logs backend-selected scale after Winit has attached a real monitor.
///
/// The native window handle is asynchronous and some backends initially expose
/// a 1x1 dummy monitor. The short timer moves inspection past initial creation;
/// failures are logged rather than changing scale after layout already exists.
fn install_scale_logging(window: &MainWindow, automatic: bool) {
    let window_weak = window.as_weak();
    if let Err(error) = slint::spawn_local(async move {
        let Some(window) = window_weak.upgrade() else {
            log::error!("could not detect UI scale: Slint window was destroyed");
            return;
        };
        let slint_window = window.window();
        let native_window = match slint_window.winit_window().await {
            Ok(window) => window,
            Err(error) => {
                log::error!("could not detect UI scale: {error}");
                return;
            }
        };
        let window_weak = window.as_weak();
        Timer::single_shot(std::time::Duration::from_millis(100), move || {
            let Some(window) = window_weak.upgrade() else {
                log::error!("could not detect UI scale: Slint window was destroyed");
                return;
            };
            let Some(monitor) = native_window.current_monitor() else {
                log::error!(
                    "could not detect monitor scale; Slint scale={}, winit window scale={}",
                    window.window().scale_factor(),
                    native_window.scale_factor()
                );
                return;
            };
            let monitor_size = monitor.size();
            if monitor_size.width <= 1 || monitor_size.height <= 1 {
                log::error!(
                    "could not detect monitor scale: winit returned its dummy monitor; Slint and winit are using fallback scale {}",
                    window.window().scale_factor()
                );
                return;
            }
            if automatic {
                log::debug!(
                    "auto-detected UI scale: Slint={}, winit window={}, monitor={} ({:?}, {}x{})",
                    window.window().scale_factor(),
                    native_window.scale_factor(),
                    monitor.scale_factor(),
                    monitor.name(),
                    monitor_size.width,
                    monitor_size.height
                );
            } else {
                log::debug!(
                    "configured UI scale: Slint={}; native winit window={}, monitor={} ({:?}, {}x{})",
                    window.window().scale_factor(),
                    native_window.scale_factor(),
                    monitor.scale_factor(),
                    monitor.name(),
                    monitor_size.width,
                    monitor_size.height
                );
            }
        });
    }) {
        log::error!("could not schedule UI scale detection: {error}");
    }
}

fn apply_preferences(window: &MainWindow, state: &StateFile) {
    window.set_dark_theme(state.preferences.theme == Theme::Dark);
    window.set_servers_expanded(state.preferences.sections.servers_expanded);
    window.set_active_incidents_expanded(state.preferences.sections.active_incidents_expanded);
    window.set_snoozed_incidents_expanded(state.preferences.sections.snoozed_incidents_expanded);
}

/// Wires close-to-tray behavior and persists presentation changes immediately.
///
/// Returning `KeepWindowShown` after explicitly hiding is intentional: allowing
/// Slint's normal close path would destroy the window and end tray-only operation.
fn install_window_callbacks(window: &MainWindow, store: Store, geometry: WindowGeometryManager) {
    geometry.install_event_handler(window.window());

    let window_weak = window.as_weak();
    window.window().on_close_requested(move || {
        if let Some(window) = window_weak.upgrade()
            && let Err(error) = geometry.hide(window.window())
        {
            log::error!("failed to hide window: {error:#}");
        }
        CloseRequestResponse::KeepWindowShown
    });

    let window_weak = window.as_weak();
    window.on_preferences_changed(move || {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        match store.update(|state| {
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
            Ok(())
        }) {
            Ok(()) => {}
            Err(error) => {
                log::error!("failed to save preferences: {error:#}");
            }
        }
    });
}

/// Connects tray activation, menu actions, notification diagnostics, and exit.
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
            match tray_toggle_action(
                window.window().is_visible(),
                window_has_focus(window.window()),
            ) {
                TrayToggleAction::Hide => {
                    if let Err(error) = geometry_for_toggle.hide(window.window()) {
                        log::error!("failed to hide window: {error:#}");
                    }
                }
                TrayToggleAction::Show => {
                    if let Err(error) = geometry_for_toggle.show(window.window()) {
                        log::error!("failed to show window: {error:#}");
                        return;
                    }
                    activate_window(window.window());
                }
                TrayToggleAction::Activate => activate_window(window.window()),
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
            log::error!("failed to show window: {error:#}");
            return;
        }
        activate_window(window.window());
    });

    tray.on_example_notification(move || {
        if let Err(error) = notifications.push(
            "Example notification",
            "Salmon Watch desktop notifications are working.",
        ) {
            log::error!("failed to show example notification: {error:#}");
        }
    });

    let window_weak = window.as_weak();
    tray.on_exit(move || {
        if let Some(window) = window_weak.upgrade()
            && window.window().is_visible()
            && let Err(error) = geometry.save(window.window())
        {
            log::error!("failed to save window geometry: {error:#}");
        }
        let _ = slint::quit_event_loop();
    });
}

/// Restores a minimized native window and asks the window manager for focus.
fn activate_window(window: &slint::Window) {
    window.with_winit_window(|window| {
        window.set_minimized(false);
        window.focus_window();
    });
}

fn window_has_focus(window: &slint::Window) -> bool {
    window
        .with_winit_window(|window| window.has_focus())
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrayToggleAction {
    Show,
    Activate,
    Hide,
}

/// Decision separated from platform calls so the nonstandard tray UX is testable.
///
/// Clicking an already visible but unfocused window activates it; only a second
/// click while focused hides it.
fn tray_toggle_action(visible: bool, focused: bool) -> TrayToggleAction {
    match (visible, focused) {
        (false, _) => TrayToggleAction::Show,
        (true, false) => TrayToggleAction::Activate,
        (true, true) => TrayToggleAction::Hide,
    }
}

/// Marshals SIGINT, SIGTERM, and SIGHUP onto the Slint event loop.
///
/// The ctrlc crate invokes handlers on its own thread, where touching Slint
/// components directly would violate their thread affinity. A second signal
/// deliberately abandons cleanup so an operator can escape a stuck shutdown.
fn install_termination_handler(tray: &SalmonTray) -> Result<()> {
    let tray_weak = tray.as_weak();
    let shutdown_requested = AtomicBool::new(false);
    ctrlc::set_handler(move || {
        if shutdown_requested.swap(true, Ordering::SeqCst) {
            log::warn!("received another termination signal; forcing exit");
            std::process::exit(1);
        }
        log::info!("received termination signal; shutting down");
        let tray_weak = tray_weak.clone();
        if let Err(error) = tray_weak.upgrade_in_event_loop(|tray| tray.invoke_exit()) {
            log::error!("failed to request shutdown after termination signal: {error}");
        }
    })
    .context("failed to install termination signal handler")
}

/// Schedules one generation-tagged flash edge and recursively schedules the next.
///
/// A repeating timer would be easy to restart during the one-second UI refresh,
/// shortening the transparent phase. The controller rejects callbacks belonging
/// to superseded icon generations, keeping cadence stable across state updates.
fn schedule_flash_tick(
    window: &MainWindow,
    tray: &SalmonTray,
    transparent: slint::Image,
    flash: TrayFlashController,
    cycle: FlashCycle,
) {
    let window_weak = window.as_weak();
    let tray_weak = tray.as_weak();
    Timer::single_shot(std::time::Duration::from_millis(500), move || {
        let (Some(window), Some(tray)) = (window_weak.upgrade(), tray_weak.upgrade()) else {
            return;
        };
        let Some(showing_solid) = flash.tick(cycle) else {
            return;
        };
        tray.set_tray_icon(if showing_solid {
            window.get_current_tray_icon()
        } else {
            transparent.clone()
        });
        schedule_flash_tick(&window, &tray, transparent, flash, cycle);
    });
}

/// Translates the closed Slint action vocabulary into bounded runtime commands.
///
/// `try_send` is deliberate on the UI thread: it must never block rendering.
/// Saturation is exceptional and is logged rather than freezing the interface.
fn install_incident_actions(window: &MainWindow, commands: tokio::sync::mpsc::Sender<Command>) {
    window.on_incident_action(move |key, action, argument| {
        let command = match action.as_str() {
            "snooze" => parse_snooze_duration(&argument).map(|seconds| Command::Snooze {
                key: key.to_string(),
                seconds,
            }),
            "unsnooze" => Some(Command::Unsnooze {
                key: key.to_string(),
            }),
            "forget" => Some(Command::ForgetStale {
                key: key.to_string(),
            }),
            _ => None,
        };
        if let Some(command) = command
            && let Err(error) = commands.try_send(command)
        {
            log::error!("failed to queue incident action: {error}");
        }
    });
}

/// Parses only durations exposed by the static snooze menu.
fn parse_snooze_duration(value: &str) -> Option<i64> {
    // TODO: Replace this fixed menu-value mapping with a general duration parser.
    Some(match value {
        "15m" => 15 * 60,
        "30m" => 30 * 60,
        "1h" => 60 * 60,
        "4h" => 4 * 60 * 60,
        "6h" => 6 * 60 * 60,
        "12h" => 12 * 60 * 60,
        "1d" => 24 * 60 * 60,
        "2d" => 2 * 24 * 60 * 60,
        "7d" => 7 * 24 * 60 * 60,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use crate::persistence::{Store, WindowGeometry};
    use crate::window_geometry::WindowGeometryManager;
    use slint::ComponentHandle;

    #[test]
    fn tray_toggle_shows_activates_or_hides_based_on_window_state() {
        use super::TrayToggleAction;

        assert_eq!(
            super::tray_toggle_action(false, false),
            TrayToggleAction::Show
        );
        assert_eq!(
            super::tray_toggle_action(false, true),
            TrayToggleAction::Show
        );
        assert_eq!(
            super::tray_toggle_action(true, false),
            TrayToggleAction::Activate
        );
        assert_eq!(
            super::tray_toggle_action(true, true),
            TrayToggleAction::Hide
        );
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
        let manager = WindowGeometryManager::new(
            Store::new(directory.path().join("state.json")),
            Some(geometry),
        );
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
        let manager = WindowGeometryManager::new(
            Store::new(directory.path().join("state.json")),
            Some(geometry),
        );
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
}
