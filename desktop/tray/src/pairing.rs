// SPDX-License-Identifier: GPL-3.0-or-later

//! The "Pair a phone" window: shows the pairing QR code, then says which phone paired.
//!
//! It asks the daemon for a code through the control channel, like `pasteget pair`, and shows
//! what comes back in a small page (`dist/pair.html`). This side drives the page with
//! [`script`], one call to `window.pasteget.show` per change; the page itself only asks for the
//! address of the Android app (`app_info`).
//!
//! A code works for a few minutes. When it expires and the window is still open, the window asks
//! for a new one by itself; when it can't get one (no network, paste-get restarting), it says why
//! and tries again.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use pasteget_core::pairing::PairingError;
use pasteget_desktop::control::{self, Endpoint, Request, Response};
use pasteget_desktop::i18n::{french, tr};
use qrcode::QrCode;
use qrcode::render::svg;
use serde_json::json;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::oneshot;

pub const WINDOW: &str = "pair";

/// How long "Paired with …" stays before the window closes.
const CLOSE_AFTER: Duration = Duration::from_secs(4);
/// How long the page may take to load.
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// A request that ends this close to its code's expiry ended because the code expired.
const EXPIRY_SLACK: Duration = Duration::from_secs(2);
/// Before asking again for a code that couldn't be had.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// Before asking again when the daemon went away without a word: it is restarting.
const RESTART_PAUSE: Duration = Duration::from_secs(1);

/// What the window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// Asking the daemon for a code.
    Preparing,
    /// A code to scan, replaced by a new one after `secs`.
    Code {
        svg: String,
        secs: u64,
    },
    /// A device tried and failed; the code still works. In the user's language.
    AttemptFailed(String),
    Paired(String),
    /// No code could be had, in the user's language. Another is asked for in a few seconds.
    Error(String),
}

/// How one request for a code ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    Paired,
    /// Its time ran out: ask for a new one.
    Expired,
    /// The daemon went away before the code expired, saying nothing: it restarted.
    Interrupted,
    /// No code, or the code stopped working, for this reason (already in the user's language).
    Failed(String),
}

/// The QR code as an SVG element: black on white with a quiet zone, so it scans on any theme.
pub fn qr_svg(text: &str) -> Result<String> {
    let code = QrCode::new(text.as_bytes())?;
    let document = code
        .render::<svg::Color<'_>>()
        .min_dimensions(256, 256)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .quiet_zone(true)
        .build();
    // Only the <svg> element: the XML declaration means nothing inside an HTML page.
    let start = document
        .find("<svg")
        .context("the QR code renderer made no <svg>")?;
    Ok(document[start..].to_owned())
}

/// The JavaScript that shows `view`. Every value goes through JSON, so device names and error
/// messages can't inject code; the page puts text in with `textContent`.
pub fn script(view: &View) -> String {
    let data = match view {
        View::Preparing => json!({ "kind": "preparing" }),
        View::Code { svg, secs } => json!({ "kind": "code", "svg": svg, "secs": secs }),
        View::AttemptFailed(text) => json!({ "kind": "attempt-failed", "text": text }),
        View::Paired(name) => json!({ "kind": "paired", "text": name }),
        View::Error(text) => json!({ "kind": "error", "text": text }),
    };
    format!("window.pasteget && window.pasteget.show({data});")
}

/// Why no code could be shown, from the daemon's or the control channel's message (English, as
/// in the logs), in the user's language. Messages it doesn't know are shown as they are.
fn trouble(message: &str) -> String {
    if message.contains("is not running") {
        tr(
            "paste-get isn't running: its menu says why.",
            "paste-get ne tourne pas : son menu dit pourquoi.",
        )
        .into()
    } else if message.contains("no network address") {
        tr(
            "This computer isn't on a network. Connect it to the same Wi-Fi as your phone.",
            "Cet ordinateur n'est sur aucun réseau. Connecte-le au même Wi-Fi que ton téléphone.",
        )
        .into()
    } else if french() {
        format!("Le code ne peut pas s'afficher ({message}).")
    } else {
        format!("The code can't be shown ({message}).")
    }
}

/// What a device's failed attempt (a [`PairingError`] from the daemon) means, in the user's
/// language. The code still works.
fn attempt_failed(reason: &str) -> String {
    if reason == PairingError::WrongSecret.to_string()
        || reason == PairingError::Expired.to_string()
    {
        tr(
            "A device tried an old or wrong code. If it was your phone, scan this one.",
            "Un appareil a essayé un code ancien ou faux. Si c'était ton téléphone, scanne celui-ci.",
        )
        .into()
    } else if french() {
        format!("Un appareil a essayé de s'appairer, sans succès ({reason}).")
    } else {
        format!("A device tried to pair but it didn't work ({reason}).")
    }
}

/// Asks the daemon for one code and shows what comes of it, until it is used, expires or fails.
/// Wrong attempts don't end it: the daemon never closes a code for them.
async fn one_code(endpoint: &Endpoint, show: &mut impl FnMut(View)) -> Ended {
    let mut expires = None;
    let mut ended = None;
    let result = control::call(endpoint, &Request::Pair, |response| match response {
        Response::Invite {
            uri,
            expires_in_secs,
        } => {
            expires = Some(Instant::now() + Duration::from_secs(expires_in_secs));
            match qr_svg(&uri) {
                Ok(svg) => show(View::Code {
                    svg,
                    secs: expires_in_secs,
                }),
                Err(e) => ended = Some(Ended::Failed(trouble(&format!("{e:#}")))),
            }
        }
        Response::PairingAttemptFailed { reason } => {
            show(View::AttemptFailed(attempt_failed(&reason)));
        }
        Response::Paired(device) => {
            show(View::Paired(device.name));
            ended = Some(Ended::Paired);
        }
        Response::Error { message } => ended = Some(Ended::Failed(trouble(&message))),
        _ => {}
    })
    .await;
    if ended == Some(Ended::Paired) {
        return Ended::Paired;
    }
    // Whatever the daemon said then ("the pairing code expired"), the code's time is up.
    if expires.is_some_and(|expires| Instant::now() + EXPIRY_SLACK >= expires) {
        return Ended::Expired;
    }
    if let Err(e) = result {
        return Ended::Failed(trouble(&format!("{e:#}")));
    }
    ended.unwrap_or(if expires.is_some() {
        Ended::Interrupted
    } else {
        Ended::Failed(trouble("the daemon did not answer"))
    })
}

/// Shows codes one after the other, each as the last one expires, until a device pairs (`true`)
/// or `open` says the window was closed (`false`). A code shown when the window closes still
/// works until it expires; no other is asked for.
async fn keep_pairing(
    endpoint: &Endpoint,
    show: &mut impl FnMut(View),
    open: &impl Fn() -> bool,
) -> bool {
    loop {
        match one_code(endpoint, show).await {
            Ended::Paired => return true,
            Ended::Expired => {
                tracing::info!("the pairing code expired: showing a new one");
            }
            Ended::Interrupted => {
                show(View::Preparing);
                tokio::time::sleep(RESTART_PAUSE).await;
            }
            Ended::Failed(text) => {
                show(View::Error(text));
                if open() {
                    tokio::time::sleep(RETRY_AFTER).await;
                }
            }
        }
        if !open() {
            return false;
        }
    }
}

/// Opens the window, or brings it to the front if it's already open. Runs until the pairing
/// ends; dropping the future abandons it.
pub async fn run<R: Runtime>(app: AppHandle<R>, endpoint: Endpoint) -> Result<()> {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.unminimize();
        window.set_focus()?;
        return Ok(());
    }

    let (loaded_tx, loaded) = oneshot::channel();
    // `on_page_load` takes a `Fn`: the sender sits in a mutex to be taken once.
    let loaded_tx = std::sync::Mutex::new(Some(loaded_tx));
    let window = WebviewWindowBuilder::new(&app, WINDOW, WebviewUrl::App("pair.html".into()))
        .title(tr(
            "Pair a phone · paste-get",
            "Appairer un téléphone · paste-get",
        ))
        .initialization_script(crate::language_script())
        .inner_size(420.0, 680.0)
        .resizable(false)
        .center()
        .focused(true)
        .on_page_load(move |_, payload| {
            if payload.event() == PageLoadEvent::Finished
                && let Some(tx) = loaded_tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            {
                let _ = tx.send(());
            }
        })
        .build()
        .context("could not open the pairing window")?;

    #[cfg(windows)]
    {
        if let Ok(hwnd) = window.hwnd() {
            use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_MONITOR};
            unsafe {
                SetWindowDisplayAffinity(hwnd.0 as _, WDA_MONITOR);
            }
        }
    }

    tokio::time::timeout(LOAD_TIMEOUT, loaded)
        .await
        .context("the pairing window did not load")?
        .context("the pairing window closed")?;

    // The user may close the window: the code shown goes on working without it.
    let mut show = |view: View| {
        let _ = window.eval(script(&view));
    };
    let open = || app.get_webview_window(WINDOW).is_some();
    if keep_pairing(&endpoint, &mut show, &open).await {
        tracing::info!("paired from the tray");
        tokio::time::sleep(CLOSE_AFTER).await;
        let _ = window.close();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn english() {
        pasteget_desktop::i18n::force(pasteget_desktop::i18n::Lang::En);
    }

    #[test]
    fn scripts_carry_untrusted_text_as_json() {
        let script = script(&View::Paired(r#"</script>"); alert(1); ("#.into()));

        // The name is inside a JSON string, quotes escaped: it can't end the call.
        assert!(script.starts_with("window.pasteget && window.pasteget.show({"));
        assert!(script.contains(r#""text":"</script>\"); alert(1); (""#));
        assert!(script.ends_with("});"));
    }

    #[test]
    fn codes_are_scannable_svgs() {
        let svg = qr_svg("pasteget://pair?v=0&id=X").unwrap();

        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("#000000") && svg.contains("#ffffff"));
        let script = script(&View::Code { svg, secs: 300 });
        assert!(script.contains(r#""kind":"code""#) && script.contains(r#""secs":300"#));
    }

    #[test]
    fn failed_attempts_are_explained() {
        english();
        let wrong = attempt_failed(&PairingError::WrongSecret.to_string());
        assert!(wrong.contains("old or wrong code"), "{wrong}");
        // Others keep the technical reason, for a report.
        let other = attempt_failed(&PairingError::Protocol.to_string());
        assert!(other.contains("didn't work") && other.contains("pairing protocol"));
    }

    #[tokio::test]
    async fn a_stopped_daemon_is_explained() {
        english();
        let home = tempfile::tempdir().unwrap();
        let profile = pasteget_desktop::config::Profile::open_in(home.path(), "pc").unwrap();
        let mut shown = Vec::new();

        let ended = one_code(&Endpoint::for_profile(&profile), &mut |view| {
            shown.push(view)
        })
        .await;

        assert_eq!(
            ended,
            Ended::Failed("paste-get isn't running: its menu says why.".into())
        );
        assert!(shown.is_empty());
    }

    /// The daemon's own message, from a daemon that has no address to put in a code.
    #[tokio::test]
    async fn no_network_is_explained() {
        use pasteget_desktop::clipboard::MemoryClipboard;
        use pasteget_desktop::daemon::{Daemon, DaemonOptions};
        use std::net::{Ipv4Addr, SocketAddr};
        use std::sync::Arc;

        english();
        let home = tempfile::tempdir().unwrap();
        let profile = pasteget_desktop::config::Profile::open_in(home.path(), "pc").unwrap();
        let mut options = DaemonOptions::listen_on(0);
        options.listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        options.announce = false;
        options.advertised_ips = Some(Vec::new());
        let daemon = Arc::new(
            Daemon::start(profile.clone(), MemoryClipboard::default(), options)
                .await
                .unwrap(),
        );
        let endpoint = Endpoint::for_profile(&profile);
        let server = control::serve(daemon, &endpoint).await.unwrap();
        let mut shown = Vec::new();

        let ended = one_code(&endpoint, &mut |view| shown.push(view)).await;

        assert_eq!(
            ended,
            Ended::Failed(
                "This computer isn't on a network. Connect it to the same Wi-Fi as your phone."
                    .into()
            )
        );
        server.abort();
    }

    /// A stand-in for the daemon's control channel: it answers each request for a code with the
    /// lines of `answers`, in turn, then hangs up.
    #[cfg(unix)]
    fn fake_daemon(
        profile: &pasteget_desktop::config::Profile,
        answers: Vec<Vec<String>>,
    ) -> tokio::task::JoinHandle<usize> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::UnixListener::bind(profile.dir().join("control.sock")).unwrap();
        tokio::spawn(async move {
            let mut asked = 0;
            for lines in answers {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let (read, mut write) = stream.into_split();
                let mut request = String::new();
                let _ = BufReader::new(read).read_line(&mut request).await;
                assert!(request.contains(r#""cmd":"pair""#), "{request}");
                asked += 1;
                for line in lines {
                    let _ = write.write_all(format!("{line}\n").as_bytes()).await;
                }
            }
            asked
        })
    }

    #[cfg(unix)]
    fn invite(expires_in_secs: u64) -> String {
        serde_json::to_string(&Response::Invite {
            uri: "pasteget://pair?v=0&id=X".into(),
            expires_in_secs,
        })
        .unwrap()
    }

    /// An expired code is replaced by a new one, without the user doing anything, and the
    /// window doesn't show the daemon's "expired".
    #[cfg(unix)]
    #[tokio::test]
    async fn an_expired_code_is_replaced_by_a_new_one() {
        english();
        let home = tempfile::tempdir().unwrap();
        let profile = pasteget_desktop::config::Profile::open_in(home.path(), "pc").unwrap();
        let expired = serde_json::to_string(&Response::Error {
            message: "the pairing code expired".into(),
        })
        .unwrap();
        let paired = r#"{"paired":{"id":"PHONE","name":"POCO M8 5G"}}"#.to_owned();
        let daemon = fake_daemon(
            &profile,
            vec![
                // The code expires at once (the daemon says so after its time).
                vec![invite(0), expired],
                vec![invite(300), paired],
            ],
        );
        let mut shown = Vec::new();

        let paired = keep_pairing(
            &Endpoint::for_profile(&profile),
            &mut |view| shown.push(view),
            &|| true,
        )
        .await;

        assert!(paired);
        assert_eq!(daemon.await.unwrap(), 2);
        let kinds: Vec<_> = shown
            .iter()
            .map(|view| match view {
                View::Code { secs, .. } => format!("code {secs}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["code 0", "code 300", r#"Paired("POCO M8 5G")"#]);
    }

    /// The daemon restarted (new settings) while the code was shown: a new code comes from the
    /// new daemon.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_daemon_that_went_away_gives_a_new_code() {
        english();
        let home = tempfile::tempdir().unwrap();
        let profile = pasteget_desktop::config::Profile::open_in(home.path(), "pc").unwrap();
        let paired = r#"{"paired":{"id":"PHONE","name":"POCO"}}"#.to_owned();
        let daemon = fake_daemon(&profile, vec![vec![invite(300)], vec![invite(300), paired]]);
        let mut shown = Vec::new();

        let paired = keep_pairing(
            &Endpoint::for_profile(&profile),
            &mut |view| shown.push(view),
            &|| true,
        )
        .await;

        assert!(paired);
        assert_eq!(daemon.await.unwrap(), 2);
        assert_eq!(shown[1], View::Preparing);
    }

    /// A phone that scanned the code before it was renewed tries the old one at each address:
    /// each try is said, and the code shown keeps working, without a new one.
    #[cfg(unix)]
    #[tokio::test]
    async fn wrong_attempts_keep_the_code_shown() {
        english();
        let home = tempfile::tempdir().unwrap();
        let profile = pasteget_desktop::config::Profile::open_in(home.path(), "pc").unwrap();
        let failed = serde_json::to_string(&Response::PairingAttemptFailed {
            reason: PairingError::WrongSecret.to_string(),
        })
        .unwrap();
        let paired = r#"{"paired":{"id":"PHONE","name":"POCO"}}"#.to_owned();
        let daemon = fake_daemon(
            &profile,
            vec![vec![
                invite(300),
                failed.clone(),
                failed.clone(),
                failed,
                paired,
            ]],
        );
        let mut shown = Vec::new();

        let paired = keep_pairing(
            &Endpoint::for_profile(&profile),
            &mut |view| shown.push(view),
            &|| true,
        )
        .await;

        assert!(paired);
        assert_eq!(daemon.await.unwrap(), 1);
        let kinds: Vec<_> = shown
            .iter()
            .map(|view| match view {
                View::Code { secs, .. } => format!("code {secs}"),
                View::AttemptFailed(text) => text.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        let failed = "A device tried an old or wrong code. If it was your phone, scan this one.";
        assert_eq!(
            kinds,
            ["code 300", failed, failed, failed, r#"Paired("POCO")"#]
        );
    }

    /// Closed: the code shown expires, and no other is asked for.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_closed_window_gets_no_new_code() {
        english();
        let home = tempfile::tempdir().unwrap();
        let profile = pasteget_desktop::config::Profile::open_in(home.path(), "pc").unwrap();
        let daemon = fake_daemon(&profile, vec![vec![invite(0)], vec![invite(300)]]);
        let mut shown = Vec::new();

        let paired = keep_pairing(
            &Endpoint::for_profile(&profile),
            &mut |view| shown.push(view),
            &|| false,
        )
        .await;

        assert!(!paired);
        assert_eq!(shown.len(), 1);
        daemon.abort();
    }
}
