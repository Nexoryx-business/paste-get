// SPDX-License-Identifier: GPL-3.0-or-later

//! The "Pair a phone" window: shows the pairing QR code, then says which phone paired.
//!
//! It asks the daemon for a code through the control channel, like `pasteget pair`, and shows
//! what comes back in a small page (`dist/pair.html`). The page calls nothing itself: this side
//! drives it with [`script`], one call to `window.pasteget.show` per change.

use std::time::Duration;

use anyhow::{Context, Result};
use pasteget_desktop::control::{self, Endpoint, Request, Response};
use qrcode::QrCode;
use qrcode::render::svg;
use serde_json::json;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::{mpsc, oneshot};

pub const WINDOW: &str = "pair";

/// How long "Paired with …" stays before the window closes.
const CLOSE_AFTER: Duration = Duration::from_secs(4);
/// How long the page may take to load.
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// What the window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// A code to scan, valid for `minutes`.
    Code {
        svg: String,
        minutes: u64,
    },
    /// A device tried and failed; the code still works.
    AttemptFailed(String),
    Paired(String),
    /// The code can't be shown or stopped working (expired, no network…).
    Error(String),
}

impl View {
    pub fn from_response(response: Response) -> Option<Self> {
        Some(match response {
            Response::Invite {
                uri,
                expires_in_secs,
            } => match qr_svg(&uri) {
                Ok(svg) => Self::Code {
                    svg,
                    minutes: expires_in_secs.div_ceil(60),
                },
                Err(e) => Self::Error(format!("could not draw the code: {e:#}")),
            },
            Response::PairingAttemptFailed { reason } => Self::AttemptFailed(reason),
            Response::Paired(device) => Self::Paired(device.name),
            Response::Error { message } => Self::Error(message),
            _ => return None,
        })
    }
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
        View::Code { svg, minutes } => json!({ "kind": "code", "svg": svg, "minutes": minutes }),
        View::AttemptFailed(reason) => json!({ "kind": "attempt-failed", "text": reason }),
        View::Paired(name) => json!({ "kind": "paired", "text": name }),
        View::Error(message) => json!({ "kind": "error", "text": message }),
    };
    format!("window.pasteget && window.pasteget.show({data});")
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
        .title("Pair a phone · paste-get")
        .inner_size(400.0, 540.0)
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
    tokio::time::timeout(LOAD_TIMEOUT, loaded)
        .await
        .context("the pairing window did not load")?
        .context("the pairing window closed")?;

    // The control client calls back synchronously: hand each answer over to this task.
    let (views_tx, mut views) = mpsc::unbounded_channel();
    let request = tokio::spawn(async move {
        let result = control::call(&endpoint, &Request::Pair, |response| {
            if let Some(view) = View::from_response(response) {
                let _ = views_tx.send(view);
            }
        })
        .await;
        if let Err(e) = result {
            let _ = views_tx.send(View::Error(format!("{e:#}")));
        }
    });

    while let Some(view) = views.recv().await {
        // The user may have closed the window: the pairing goes on without it.
        let _ = window.eval(script(&view));
        if matches!(view, View::Paired(_)) {
            tracing::info!("paired from the tray");
            tokio::time::sleep(CLOSE_AFTER).await;
            let _ = window.close();
            break;
        }
    }
    request.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pasteget_desktop::control::DeviceReply;

    #[test]
    fn invites_become_a_scannable_svg() {
        let view = View::from_response(Response::Invite {
            uri: "pasteget://pair?v=0&id=X".into(),
            expires_in_secs: 300,
        });

        let Some(View::Code { svg, minutes }) = view else {
            panic!("expected a code");
        };
        assert_eq!(minutes, 5);
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("#000000") && svg.contains("#ffffff"));
    }

    #[test]
    fn answers_map_to_what_the_window_says() {
        let paired = Response::Paired(DeviceReply {
            id: "ID".into(),
            name: "POCO M8 5G".into(),
        });
        assert_eq!(
            View::from_response(paired),
            Some(View::Paired("POCO M8 5G".into()))
        );
        assert_eq!(
            View::from_response(Response::Error {
                message: "the pairing code expired".into()
            }),
            Some(View::Error("the pairing code expired".into()))
        );
        assert_eq!(View::from_response(Response::Copied), None);
    }

    #[test]
    fn scripts_carry_untrusted_text_as_json() {
        let script = script(&View::Paired(r#"</script>"); alert(1); ("#.into()));

        // The name is inside a JSON string, quotes escaped: it can't end the call.
        assert!(script.starts_with("window.pasteget && window.pasteget.show({"));
        assert!(script.contains(r#""text":"</script>\"); alert(1); (""#));
        assert!(script.ends_with("});"));
    }
}
