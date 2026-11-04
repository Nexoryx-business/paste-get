// SPDX-License-Identifier: GPL-3.0-or-later

//! `pasteget`: desktop daemon and command-line tool.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anstream::{eprintln, println};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use data_encoding::BASE64;
use pasteget_core::clip::{DEFAULT_MAX_CLIP_LEN, mime};
use pasteget_desktop::clipboard::{MemoryClipboard, SystemClipboard};
use pasteget_desktop::config::{DEFAULT_PROFILE, Profile};
use pasteget_desktop::control::{self, Endpoint, Request, Response};
use pasteget_desktop::daemon::{Daemon, DaemonOptions};
use pasteget_desktop::notify::DesktopNotifier;
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;

#[derive(Parser)]
#[command(
    name = "pasteget",
    version,
    about = "Copy on your phone, paste on your PC.",
    long_about = None
)]
struct Cli {
    /// Profile to use. Each profile is a separate device with its own pairings.
    #[arg(long, global = true, default_value = DEFAULT_PROFILE)]
    profile: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground (Ctrl+C to stop).
    Daemon {
        /// Use a private in-memory clipboard instead of the system one. For running two
        /// daemons on one machine while testing.
        #[arg(long)]
        memory_clipboard: bool,
        /// Port to listen on, instead of the one in settings.toml.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Show a pairing QR code and wait for a device to pair.
    Pair,
    /// Pair with another paste-get device using the code it shows.
    Join {
        /// The pairing code (starts with pasteget://pair?).
        invite: String,
    },
    /// Send text, or a text or PNG file, to every connected device.
    Send {
        /// Text to send.
        #[arg(required_unless_present = "file", conflicts_with = "file")]
        text: Option<String>,
        /// File to send instead: UTF-8 text or PNG.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Show this device and its paired devices.
    Status,
    /// Stop sending copies and receiving clips until `pasteget resume`.
    /// `pasteget send` still works while paused.
    Pause {
        /// Resume instead if already paused. Handy for a hotkey.
        #[arg(long)]
        toggle: bool,
    },
    /// Resume syncing after `pasteget pause`.
    Resume,
    /// List the last clips sent or received, newest first.
    Recent,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let profile = Profile::open(&cli.profile)?;
    let endpoint = Endpoint::for_profile(&profile);
    match cli.command {
        Command::Daemon {
            memory_clipboard,
            port,
        } => daemon(profile, &endpoint, memory_clipboard, port).await,
        Command::Pair => pair(&endpoint).await,
        Command::Join { invite } => join(&endpoint, invite).await,
        Command::Send { text, file } => send(&endpoint, text, file).await,
        Command::Status => status(&endpoint).await,
        Command::Pause { toggle } => pause(&endpoint, if toggle { None } else { Some(true) }).await,
        Command::Resume => pause(&endpoint, Some(false)).await,
        Command::Recent => recent(&endpoint).await,
    }
}

async fn daemon(
    profile: Profile,
    endpoint: &Endpoint,
    memory_clipboard: bool,
    port: Option<u16>,
) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    if control::is_running(endpoint).await {
        bail!(
            "a daemon is already running for profile \"{}\"",
            profile.name()
        );
    }
    let mut options = DaemonOptions::listen_on(port.unwrap_or(profile.settings()?.port));
    // A memory-clipboard daemon stands in for another device: its notifications would only
    // confuse.
    if !memory_clipboard {
        options.notifier = Some(Arc::new(DesktopNotifier::start()));
    }
    let daemon = if memory_clipboard {
        Daemon::start(profile, MemoryClipboard::default(), options).await?
    } else {
        Daemon::start(profile, SystemClipboard::new()?, options).await?
    };
    let daemon = Arc::new(daemon);
    let server = control::serve(Arc::clone(&daemon), endpoint).await?;

    let status = daemon.status().await;
    println!(
        "paste-get is running as \"{}\" on port {}.",
        status.device_name,
        status.listening_on.port()
    );
    if status.devices.is_empty() {
        println!("No device paired yet: run `pasteget pair` in another terminal.");
    }
    println!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c()
        .await
        .context("could not wait for Ctrl+C")?;
    server.abort();
    control::cleanup(endpoint);
    println!("Stopped.");
    Ok(())
}

async fn pair(endpoint: &Endpoint) -> Result<()> {
    let mut failure = None;
    control::call(endpoint, &Request::Pair, |response| match response {
        Response::Invite {
            uri,
            expires_in_secs,
        } => {
            if let Err(e) = print_qr(&uri) {
                eprintln!("(could not draw the QR code: {e})");
            }
            println!("Scan this code with paste-get on your phone.");
            println!("From another computer, run instead:");
            println!("  pasteget join \"{uri}\"");
            println!(
                "The code works once and expires in {} minutes.",
                expires_in_secs / 60
            );
        }
        Response::PairingAttemptFailed { reason } => {
            eprintln!("A device tried to pair but failed: {reason}");
        }
        Response::Paired(device) => println!("✓ Paired with {} ({}).", device.name, device.id),
        Response::Error { message } => failure = Some(message),
        _ => {}
    })
    .await?;
    match failure {
        Some(message) => bail!(message),
        None => Ok(()),
    }
}

async fn join(endpoint: &Endpoint, invite: String) -> Result<()> {
    let request = Request::Join { invite };
    expect_one(endpoint, &request, |response| match response {
        Response::Joined(device) => {
            println!("✓ Paired with {} ({}).", device.name, device.id);
            true
        }
        _ => false,
    })
    .await
}

async fn send(endpoint: &Endpoint, text: Option<String>, file: Option<PathBuf>) -> Result<()> {
    let (mime_type, data) = match (text, file) {
        (Some(text), _) => (mime::TEXT, text.into_bytes()),
        (None, Some(path)) => {
            let data = std::fs::read(&path)
                .with_context(|| format!("could not read {}", path.display()))?;
            if let Some(image) = mime::of_image(&data) {
                (image, data)
            } else if std::str::from_utf8(&data).is_ok() {
                (mime::TEXT, data)
            } else {
                bail!("only UTF-8 text, PNG and JPEG files can be sent for now");
            }
        }
        (None, None) => bail!("nothing to send"),
    };
    if data.len() as u64 > DEFAULT_MAX_CLIP_LEN {
        bail!(
            "too large: the limit is {} MiB",
            DEFAULT_MAX_CLIP_LEN / (1024 * 1024)
        );
    }
    let request = Request::Send {
        mime_type: mime_type.to_owned(),
        data_base64: BASE64.encode(&data),
    };
    expect_one(endpoint, &request, |response| match response {
        Response::Sent { devices: 0 } => {
            println!("No device is connected right now.");
            true
        }
        Response::Sent { devices } => {
            println!("Sent to {devices} device(s).");
            true
        }
        _ => false,
    })
    .await
}

async fn status(endpoint: &Endpoint) -> Result<()> {
    expect_one(endpoint, &Request::Status, |response| {
        let Response::Status(status) = response else {
            return false;
        };
        println!("{}", status.device_name);
        println!("  ID:        {}", status.device_id);
        println!("  Listening: {}", status.listening_on);
        if status.pairing_open {
            println!("  Pairing:   a pairing code is being shown");
        }
        if status.paused {
            println!("  Paused:    nothing is synced until `pasteget resume`");
        }
        if status.devices.is_empty() {
            println!("No paired device. Run `pasteget pair` to add one.");
        } else {
            println!("Paired devices:");
            for device in status.devices {
                let state = if device.connected {
                    "● connected"
                } else {
                    "○ offline"
                };
                println!("  {state}  {}", device.name);
            }
        }
        true
    })
    .await
}

async fn pause(endpoint: &Endpoint, paused: Option<bool>) -> Result<()> {
    expect_one(
        endpoint,
        &Request::Pause { paused },
        |response| match response {
            Response::Paused { paused: true } => {
                println!("Paused: nothing is synced until `pasteget resume`.");
                true
            }
            Response::Paused { paused: false } => {
                println!("Syncing.");
                true
            }
            _ => false,
        },
    )
    .await
}

async fn recent(endpoint: &Endpoint) -> Result<()> {
    expect_one(endpoint, &Request::Recent, |response| {
        let Response::Recent { clips } = response else {
            return false;
        };
        if clips.is_empty() {
            println!("Nothing yet.");
        }
        for clip in clips {
            println!(
                "{:>8}  {:<20}  {}",
                ago(clip.age_secs),
                clip.from.as_deref().unwrap_or("this computer"),
                describe(&clip)
            );
        }
        true
    })
    .await
}

/// "Text: hello world" or "Image (PNG, 1.2 MB)".
fn describe(clip: &control::RecentReply) -> String {
    match &clip.preview {
        Some(preview) => format!("Text: {preview}"),
        None => {
            let kind = clip
                .mime_type
                .strip_prefix("image/")
                .map_or(clip.mime_type.clone(), str::to_uppercase);
            format!("Image ({kind}, {})", size(clip.len))
        }
    }
}

fn size(len: usize) -> String {
    let len = len as f64;
    if len < 1024.0 {
        format!("{len} B")
    } else if len < 1024.0 * 1024.0 {
        format!("{:.0} KB", len / 1024.0)
    } else {
        format!("{:.1} MB", len / (1024.0 * 1024.0))
    }
}

fn ago(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min", secs / 60),
        _ => format!("{} h", secs / 3600),
    }
}

/// Sends a one-answer request. `on_success` returns `false` for unexpected answers.
async fn expect_one(
    endpoint: &Endpoint,
    request: &Request,
    mut on_success: impl FnMut(Response) -> bool,
) -> Result<()> {
    let mut outcome = Err(anyhow::anyhow!("the daemon did not answer"));
    let call = control::call(endpoint, request, |response| {
        outcome = match response {
            Response::Error { message } => Err(anyhow::anyhow!(message)),
            other => {
                if on_success(other) {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("unexpected answer from the daemon"))
                }
            }
        };
    });
    tokio::time::timeout(control::CLIENT_TIMEOUT, call)
        .await
        .context("the daemon did not answer in time")??;
    outcome
}

/// Draws a QR code with Unicode half blocks, forced black on white so it scans on dark themes.
fn print_qr(text: &str) -> Result<()> {
    let code = QrCode::new(text.as_bytes())?;
    let rendered = code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Dark)
        .light_color(Dense1x2::Light)
        .quiet_zone(true)
        .build();
    for line in rendered.lines() {
        println!("\x1b[30;47m{line}\x1b[0m");
    }
    Ok(())
}
