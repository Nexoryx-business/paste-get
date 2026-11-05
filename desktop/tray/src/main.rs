// SPDX-License-Identifier: GPL-3.0-or-later

//! `pasteget-tray`: the paste-get tray icon.
//!
//! It runs the daemon in this process, unless one already runs for the profile (started with
//! `pasteget daemon`, for instance): then it only shows that one. Either way it talks to the
//! daemon through the same control channel as the `pasteget` command, so the menu shows exactly
//! what `pasteget status` and `pasteget recent` would.

// No console window on Windows. Debug builds keep it, for the logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod menu;
mod pairing;

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use pasteget_desktop::autostart::{self, Autostart, Launch};
use pasteget_desktop::clipboard::SystemClipboard;
use pasteget_desktop::config::{DEFAULT_PROFILE, Profile};
use pasteget_desktop::control::{self, Endpoint, Request, Response};
use pasteget_desktop::daemon::{Daemon, DaemonOptions};
use pasteget_desktop::notify::DesktopNotifier;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager, RunEvent, Runtime};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::menu::{Command, Entry, State};

/// How often the menu is brought up to date when nothing is clicked.
const REFRESH_EVERY: Duration = Duration::from_secs(2);
/// How long to wait before trying again to start a daemon that failed to start.
const RESTART_EVERY: Duration = Duration::from_secs(15);
/// How long a control request may take before the daemon counts as stuck.
const ASK_TIMEOUT: Duration = Duration::from_secs(5);

const TRAY_ID: &str = "paste-get";
const ICON: &[u8] = include_bytes!("../icons/tray.png");
const ICON_PAUSED: &[u8] = include_bytes!("../icons/tray-paused.png");

#[derive(Parser)]
#[command(name = "pasteget-tray", version, about = "paste-get tray icon.")]
struct Cli {
    /// Profile to use, as with `pasteget --profile`.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    profile: String,
}

/// Shared by the menu handler and the refresh loop.
struct Tray {
    profile: Profile,
    endpoint: Endpoint,
    /// The daemon this process runs, if it runs one.
    hosted: Mutex<Option<Hosted>>,
    /// `None` where starting at login isn't supported.
    autostart: Option<Autostart>,
    /// Wakes the refresh loop right after a click.
    refresh: Notify,
    /// Rebuild the menu even if nothing changed: a click ticks a checkbox by itself, even when
    /// the action behind it fails.
    redraw: AtomicBool,
    /// The pairing window's task. A new one replaces it.
    pairing: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

struct Hosted {
    _daemon: Arc<Daemon>,
    server: JoinHandle<()>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match run(&Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    let profile = Profile::open(&cli.profile)?;
    let autostart = if autostart::SUPPORTED {
        Autostart::for_profile(profile.name())
            .inspect_err(|e| tracing::warn!(error = %e, "can't start at login"))
            .ok()
    } else {
        None
    };
    let tray = Arc::new(Tray {
        endpoint: Endpoint::for_profile(&profile),
        profile,
        hosted: Mutex::new(None),
        autostart,
        refresh: Notify::new(),
        redraw: AtomicBool::new(false),
        pairing: Mutex::new(None),
    });

    let for_setup = Arc::clone(&tray);
    let app = tauri::Builder::default()
        .setup(move |app| {
            // A tray app: no Dock icon on macOS.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            setup(app.handle(), for_setup)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .context("could not start the tray")?;
    app.run(move |_, event| match event {
        // Closing the pairing window, the only window, would end the app: only Quit does.
        // (Quit asks with an exit code; closing the last window asks without one.)
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        RunEvent::Exit => tray.stop_hosted(),
        _ => {}
    });
    Ok(())
}

fn setup<R: Runtime>(app: &AppHandle<R>, tray: Arc<Tray>) -> Result<()> {
    let state = State::Starting;
    let for_clicks = Arc::clone(&tray);
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(ICON)?)
        .tooltip(tooltip(&state))
        .menu(&build_menu(
            app,
            &menu::entries(&state, tray.start_at_login()),
        )?)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| {
            if let Some(command) = Command::from_id(event.id().as_ref()) {
                on_command(app, &for_clicks, command);
            }
        })
        .build(app)?;
    tauri::async_runtime::spawn(refresh_loop(app.clone(), tray));
    Ok(())
}

fn on_command<R: Runtime>(app: &AppHandle<R>, tray: &Arc<Tray>, command: Command) {
    let request = match command {
        Command::Quit => {
            app.exit(0);
            return;
        }
        Command::Pair => {
            open_pairing(app, tray);
            return;
        }
        Command::ToggleStartAtLogin => {
            if let Err(e) = tray.toggle_start_at_login() {
                tracing::warn!(error = %format!("{e:#}"), "could not change starting at login");
            }
            tray.redraw.store(true, Ordering::Relaxed);
            tray.refresh.notify_one();
            return;
        }
        Command::TogglePause => Request::Pause { paused: None },
        Command::CopyRecent(id) => Request::CopyRecent { id },
        Command::Forget(device) => Request::Forget { device },
    };
    let tray = Arc::clone(tray);
    tauri::async_runtime::spawn(async move {
        if let Err(e) = ask(&tray.endpoint, &request).await {
            tracing::warn!(error = %e, "the daemon refused a menu command");
        }
        tray.redraw.store(true, Ordering::Relaxed);
        tray.refresh.notify_one();
    });
}

/// Opens the pairing window, or brings it to the front.
fn open_pairing<R: Runtime>(app: &AppHandle<R>, tray: &Arc<Tray>) {
    let mut slot = tray.pairing.lock().unwrap_or_else(PoisonError::into_inner);
    let window_open = app.get_webview_window(pairing::WINDOW).is_some();
    if let Some(task) = slot.take() {
        if window_open {
            // Still pairing: just show it again.
            *slot = Some(task);
            drop(slot);
            if let Some(window) = app.get_webview_window(pairing::WINDOW) {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            return;
        }
        // Closed earlier: start over with a fresh code.
        task.abort();
    }
    let app = app.clone();
    let tray = Arc::clone(tray);
    let endpoint = tray.endpoint.clone();
    *slot = Some(tauri::async_runtime::spawn(async move {
        if let Err(e) = pairing::run(app, endpoint).await {
            tracing::warn!(error = %format!("{e:#}"), "pairing window failed");
        }
        tray.refresh.notify_one();
    }));
}

/// Keeps the menu and icon in step with the daemon, and starts the daemon when none runs.
async fn refresh_loop<R: Runtime>(app: AppHandle<R>, tray: Arc<Tray>) {
    let mut shown: Option<(Vec<Entry>, bool)> = None;
    let mut next_start = Instant::now();
    // The first time paste-get runs, nothing is paired yet: show the code right away.
    let mut offered_pairing = false;
    loop {
        let mut state = fetch(&tray.endpoint).await;
        if matches!(state, State::Down(_)) && !tray.is_hosting() && Instant::now() >= next_start {
            match tray.start_hosted().await {
                Ok(()) => state = fetch(&tray.endpoint).await,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "could not start the daemon");
                    state = State::Down(format!("{e:#}"));
                    next_start = Instant::now() + RESTART_EVERY;
                }
            }
        }

        if !offered_pairing && let State::Running { status, .. } = &state {
            offered_pairing = true;
            if status.devices.is_empty() {
                open_pairing(&app, &tray);
            }
        }

        let wanted = (menu::entries(&state, tray.start_at_login()), state.paused());
        let redraw = tray.redraw.swap(false, Ordering::Relaxed);
        if redraw || shown.as_ref() != Some(&wanted) {
            if let Err(e) = show(&app, &state, &wanted.0) {
                tracing::warn!(error = %e, "could not update the tray menu");
            }
            shown = Some(wanted);
        }

        let _ = tokio::time::timeout(REFRESH_EVERY, tray.refresh.notified()).await;
    }
}

fn show<R: Runtime>(app: &AppHandle<R>, state: &State, entries: &[Entry]) -> Result<()> {
    let icon: TrayIcon<R> = app
        .tray_by_id(TRAY_ID)
        .ok_or_else(|| anyhow!("the tray icon is gone"))?;
    icon.set_menu(Some(build_menu(app, entries)?))?;
    icon.set_icon(Some(Image::from_bytes(if state.paused() {
        ICON_PAUSED
    } else {
        ICON
    })?))?;
    icon.set_tooltip(Some(tooltip(state)))?;
    Ok(())
}

fn tooltip(state: &State) -> String {
    format!("paste-get: {}", state.summary())
}

/// What the daemon reports, or why it can't be reached.
async fn fetch(endpoint: &Endpoint) -> State {
    let status = match ask(endpoint, &Request::Status).await {
        Ok(Response::Status(status)) => status,
        Ok(_) => return State::Down("unexpected answer from the daemon".into()),
        Err(e) => return State::Down(format!("{e:#}")),
    };
    let recent = match ask(endpoint, &Request::Recent).await {
        Ok(Response::Recent { clips }) => clips,
        _ => Vec::new(),
    };
    State::Running { status, recent }
}

/// Sends a one-answer request.
async fn ask(endpoint: &Endpoint, request: &Request) -> Result<Response> {
    let mut answer = None;
    tokio::time::timeout(
        ASK_TIMEOUT,
        control::call(endpoint, request, |response| answer = Some(response)),
    )
    .await
    .context("the daemon did not answer in time")??;
    match answer {
        Some(Response::Error { message }) => Err(anyhow!(message)),
        Some(response) => Ok(response),
        None => Err(anyhow!("the daemon did not answer")),
    }
}

impl Tray {
    fn start_at_login(&self) -> Option<bool> {
        self.autostart.as_ref().map(Autostart::is_enabled)
    }

    fn toggle_start_at_login(&self) -> Result<()> {
        let Some(autostart) = &self.autostart else {
            return Ok(());
        };
        if autostart.is_enabled() {
            autostart.disable()
        } else {
            autostart.enable(&Launch::this_program(self.profile.name())?)
        }
    }

    fn is_hosting(&self) -> bool {
        self.hosted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Runs the daemon in this process, on the system clipboard, like `pasteget daemon`.
    async fn start_hosted(&self) -> Result<()> {
        let settings = self.profile.settings()?;
        let mut options = DaemonOptions::listen_on(settings.port);
        options.notifier = Some(Arc::new(DesktopNotifier::start()));
        options.hotkeys = true;
        let daemon =
            Arc::new(Daemon::start(self.profile.clone(), SystemClipboard::new()?, options).await?);
        let server = control::serve(Arc::clone(&daemon), &self.endpoint).await?;
        tracing::info!(profile = self.profile.name(), "daemon started in the tray");
        *self.hosted.lock().unwrap_or_else(PoisonError::into_inner) = Some(Hosted {
            _daemon: daemon,
            server,
        });
        Ok(())
    }

    fn stop_hosted(&self) {
        let hosted = self
            .hosted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(hosted) = hosted {
            hosted.server.abort();
            control::cleanup(&self.endpoint);
            tracing::info!("daemon stopped");
        }
    }
}

fn build_menu<R: Runtime>(app: &AppHandle<R>, entries: &[Entry]) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;
    for entry in entries {
        menu.append(&*menu_item(app, entry)?)?;
    }
    Ok(menu)
}

fn menu_item<R: Runtime>(
    app: &AppHandle<R>,
    entry: &Entry,
) -> tauri::Result<Box<dyn IsMenuItem<R>>> {
    Ok(match entry {
        Entry::Info(label) => Box::new(MenuItem::new(app, label, false, None::<&str>)?),
        Entry::Separator => Box::new(PredefinedMenuItem::separator(app)?),
        Entry::Action { id, label } => Box::new(MenuItem::with_id(
            app,
            id.as_str(),
            label,
            true,
            None::<&str>,
        )?),
        Entry::Check { id, label, checked } => Box::new(CheckMenuItem::with_id(
            app,
            id.as_str(),
            label,
            true,
            *checked,
            None::<&str>,
        )?),
        Entry::Submenu { label, entries } => {
            let submenu = Submenu::new(app, label, true)?;
            for entry in entries {
                submenu.append(&*menu_item(app, entry)?)?;
            }
            Box::new(submenu)
        }
    })
}
