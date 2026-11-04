// SPDX-License-Identifier: GPL-3.0-or-later

//! Local control channel between the `pasteget` command and a running daemon.
//!
//! One JSON object per line. The client sends one [`Request`]; the daemon answers with one or
//! more [`Response`]s and closes. Only the local user can connect:
//! - Unix: a socket in the profile directory, which is private to the user (0700).
//! - Windows: a named pipe whose name includes the user name. The daemon creates it with
//!   `first_pipe_instance`, so it fails rather than share a pipe someone else created, and
//!   remote clients are rejected.

#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use data_encoding::BASE64;
use pasteget_core::pairing::{PAIRING_TTL, PairingInvite};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::clipboard::Content;
use crate::config::Profile;
use crate::daemon::{Daemon, Event};

/// Longest request line accepted: a 64 MiB clip, base64-encoded, plus some room.
const MAX_REQUEST_LEN: u64 = 96 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Pair,
    Join {
        invite: String,
    },
    Send {
        mime_type: String,
        data_base64: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Status(StatusReply),
    Invite { uri: String, expires_in_secs: u64 },
    PairingAttemptFailed { reason: String },
    Paired(DeviceReply),
    Joined(DeviceReply),
    Sent { devices: usize },
    Error { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusReply {
    pub device_id: String,
    pub device_name: String,
    pub listening_on: String,
    pub pairing_open: bool,
    pub devices: Vec<DeviceStatusReply>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceStatusReply {
    pub id: String,
    pub name: String,
    pub connected: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceReply {
    pub id: String,
    pub name: String,
}

/// Where a profile's daemon listens for commands.
#[derive(Clone, Debug)]
pub struct Endpoint {
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(windows)]
    pipe_name: String,
}

impl Endpoint {
    pub fn for_profile(profile: &Profile) -> Self {
        #[cfg(unix)]
        {
            Self {
                path: profile.dir().join("control.sock"),
            }
        }
        #[cfg(windows)]
        {
            let user = std::env::var("USERNAME").unwrap_or_default();
            let safe = |s: &str| -> String {
                s.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect()
            };
            // The directory's fingerprint keeps profiles with the same name but different
            // locations (PASTEGET_HOME, tests) on separate pipes.
            let location = fnv1a(profile.dir().to_string_lossy().as_bytes());
            Self {
                pipe_name: format!(
                    r"\\.\pipe\paste-get-{}-{}-{location:016x}",
                    safe(&user),
                    profile.name()
                ),
            }
        }
    }
}

/// FNV-1a: a tiny hash that stays the same across builds, so a newer `pasteget` command still
/// finds a daemon started by an older one. Not a security measure.
#[cfg_attr(not(windows), allow(dead_code))]
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Serves control requests for `daemon` until the returned handle is aborted.
pub async fn serve(daemon: Arc<Daemon>, endpoint: &Endpoint) -> Result<JoinHandle<()>> {
    #[cfg(unix)]
    {
        use tokio::net::UnixListener;

        if endpoint.path.exists() {
            if tokio::net::UnixStream::connect(&endpoint.path)
                .await
                .is_ok()
            {
                anyhow::bail!("a daemon is already running for this profile");
            }
            std::fs::remove_file(&endpoint.path).context("could not remove a stale socket")?;
        }
        let listener = UnixListener::bind(&endpoint.path)
            .with_context(|| format!("could not listen on {}", endpoint.path.display()))?;
        Ok(tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(handle(Arc::clone(&daemon), stream));
                    }
                    Err(e) => tracing::warn!(error = %e, "control socket error"),
                }
            }
        }))
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;

        let name = endpoint.pipe_name.clone();
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name)
            .context("could not create the control pipe: is a daemon already running?")?;
        Ok(tokio::spawn(async move {
            loop {
                if let Err(e) = server.connect().await {
                    tracing::warn!(error = %e, "control pipe error");
                    continue;
                }
                let next = match ServerOptions::new()
                    .reject_remote_clients(true)
                    .create(&name)
                {
                    Ok(next) => next,
                    Err(e) => {
                        tracing::warn!(error = %e, "could not reopen the control pipe");
                        return;
                    }
                };
                let connected = std::mem::replace(&mut server, next);
                tokio::spawn(handle(Arc::clone(&daemon), connected));
            }
        }))
    }
}

/// Whether a daemon already answers on `endpoint`.
pub async fn is_running(endpoint: &Endpoint) -> bool {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(&endpoint.path)
            .await
            .is_ok()
    }
    #[cfg(windows)]
    {
        tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&endpoint.pipe_name)
            .is_ok()
    }
}

/// Removes the socket file, if any. Call on shutdown.
pub fn cleanup(endpoint: &Endpoint) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&endpoint.path);
    }
    #[cfg(windows)]
    {
        let _ = endpoint;
    }
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin>(daemon: Arc<Daemon>, stream: S) {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half.take(MAX_REQUEST_LEN));
    let mut line = String::new();
    let request = match reader.read_line(&mut line).await {
        Ok(0) => return,
        Ok(_) => serde_json::from_str::<Request>(&line).map_err(|e| anyhow!("bad request: {e}")),
        Err(e) => Err(anyhow!("could not read the request: {e}")),
    };
    let result: Result<()> = async {
        match request? {
            Request::Status => {
                let status = daemon.status().await;
                let response = Response::Status(StatusReply {
                    device_id: status.device_id.to_string(),
                    device_name: status.device_name,
                    listening_on: status.listening_on.to_string(),
                    pairing_open: status.pairing_open,
                    devices: status
                        .devices
                        .into_iter()
                        .map(|d| DeviceStatusReply {
                            id: d.device.id.to_string(),
                            name: d.device.name,
                            connected: d.connected,
                        })
                        .collect(),
                });
                write_half.write_all(line_of(response).as_bytes()).await?;
            }
            Request::Pair => {
                let mut events = daemon.subscribe();
                let invite = daemon.open_pairing().await?;
                let response = Response::Invite {
                    uri: invite.to_uri(),
                    expires_in_secs: PAIRING_TTL.as_secs(),
                };
                write_half.write_all(line_of(response).as_bytes()).await?;
                let outcome = tokio::time::timeout(PAIRING_TTL, async {
                    loop {
                        match events.recv().await {
                            Ok(Event::Paired(device)) => {
                                return Response::Paired(DeviceReply {
                                    id: device.id.to_string(),
                                    name: device.name,
                                });
                            }
                            Ok(Event::PairingFailed(reason)) => {
                                let response = Response::PairingAttemptFailed { reason };
                                let _ = write_half.write_all(line_of(response).as_bytes()).await;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => {
                                return Response::Error {
                                    message: "the daemon stopped".into(),
                                };
                            }
                        }
                    }
                })
                .await
                .unwrap_or(Response::Error {
                    message: "the pairing code expired".into(),
                });
                write_half.write_all(line_of(outcome).as_bytes()).await?;
            }
            Request::Join { invite } => {
                let invite: PairingInvite = invite.parse()?;
                let device = daemon.join(&invite).await?;
                let response = Response::Joined(DeviceReply {
                    id: device.id.to_string(),
                    name: device.name,
                });
                write_half.write_all(line_of(response).as_bytes()).await?;
            }
            Request::Send {
                mime_type,
                data_base64,
            } => {
                let data = BASE64
                    .decode(data_base64.as_bytes())
                    .context("data is not base64")?;
                let devices = daemon.send(Content {
                    mime_type,
                    data,
                    sensitive: false,
                });
                write_half
                    .write_all(line_of(Response::Sent { devices }).as_bytes())
                    .await?;
            }
        }
        Ok(())
    }
    .await;

    if let Err(e) = result {
        let response = Response::Error {
            message: format!("{e:#}"),
        };
        let _ = write_half.write_all(line_of(response).as_bytes()).await;
    }
    let _ = write_half.shutdown().await;
}

fn line_of(response: Response) -> String {
    let mut text = serde_json::to_string(&response).unwrap_or_default();
    text.push('\n');
    text
}

/// Client side: sends `request` and calls `on_response` for each answer line.
pub async fn call(
    endpoint: &Endpoint,
    request: &Request,
    mut on_response: impl FnMut(Response),
) -> Result<()> {
    #[cfg(unix)]
    let stream = tokio::net::UnixStream::connect(&endpoint.path)
        .await
        .map_err(|_| not_running())?;
    #[cfg(windows)]
    let stream = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(&endpoint.pipe_name)
        .map_err(|_| not_running())?;

    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;

    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        on_response(serde_json::from_str(&line).context("unexpected answer from the daemon")?);
    }
    Ok(())
}

fn not_running() -> anyhow::Error {
    anyhow!("the paste-get daemon is not running for this profile: start it with `pasteget daemon`")
}

/// How long a client waits before giving up on a daemon that doesn't answer.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{MemoryClipboard, Raw};
    use crate::daemon::DaemonOptions;
    use pasteget_core::clip::mime;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    struct Running {
        daemon: Arc<Daemon>,
        endpoint: Endpoint,
        clipboard: MemoryClipboard,
        _server: JoinHandle<()>,
        _home: tempfile::TempDir,
    }

    async fn run(name: &str) -> Running {
        let home = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(home.path(), name).unwrap();
        let clipboard = MemoryClipboard::default();
        let options = DaemonOptions {
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            advertised_ips: Some(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
            poll_interval: Duration::from_millis(10),
            announce: false,
            notifier: None,
        };
        let daemon = Arc::new(
            Daemon::start(profile.clone(), clipboard.clone(), options)
                .await
                .unwrap(),
        );
        let endpoint = Endpoint::for_profile(&profile);
        let server = serve(Arc::clone(&daemon), &endpoint).await.unwrap();
        Running {
            daemon,
            endpoint,
            clipboard,
            _server: server,
            _home: home,
        }
    }

    async fn ask(endpoint: &Endpoint, request: Request) -> Vec<Response> {
        let mut responses = Vec::new();
        call(endpoint, &request, |r| responses.push(r))
            .await
            .unwrap();
        responses
    }

    #[tokio::test]
    async fn pair_join_send_and_status_through_the_control_channel() {
        let pc = run("pc").await;
        let laptop = run("laptop").await;

        // `pasteget pair` on the PC streams the invite, then the result.
        let pc_endpoint = pc.endpoint.clone();
        let (invite_tx, invite_rx) = tokio::sync::oneshot::channel();
        let pairing = tokio::spawn(async move {
            let mut invite_tx = Some(invite_tx);
            let mut responses = Vec::new();
            call(&pc_endpoint, &Request::Pair, |response| {
                if let (Response::Invite { uri, .. }, Some(tx)) = (&response, invite_tx.take()) {
                    let _ = tx.send(uri.clone());
                }
                responses.push(response);
            })
            .await
            .unwrap();
            responses
        });
        let uri = invite_rx.await.unwrap();

        // `pasteget join <uri>` on the laptop.
        let joined = ask(&laptop.endpoint, Request::Join { invite: uri }).await;
        assert!(
            matches!(&joined[..], [Response::Joined(d)] if d.name == pc.daemon.status().await.device_name)
        );
        let pairing = pairing.await.unwrap();
        assert!(matches!(pairing.last(), Some(Response::Paired(_))));

        // `pasteget send` on the PC reaches the laptop's clipboard.
        for _ in 0..300 {
            if pc.daemon.status().await.devices.iter().any(|d| d.connected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let sent = ask(
            &pc.endpoint,
            Request::Send {
                mime_type: mime::TEXT.into(),
                data_base64: BASE64.encode(b"from the command line"),
            },
        )
        .await;
        assert!(matches!(&sent[..], [Response::Sent { devices: 1 }]));
        for _ in 0..300 {
            if laptop.clipboard.get() == Some(Raw::Text("from the command line".into())) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            laptop.clipboard.get(),
            Some(Raw::Text("from the command line".into()))
        );

        // `pasteget status` lists the paired device as connected.
        let status = ask(&pc.endpoint, Request::Status).await;
        let [Response::Status(status)] = &status[..] else {
            panic!("expected a status");
        };
        assert_eq!(status.devices.len(), 1);
        assert!(status.devices[0].connected);
    }

    #[test]
    fn fnv1a_matches_the_reference_values() {
        // Published FNV-1a 64-bit test vectors: the pipe name must never change between builds.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[tokio::test]
    async fn bad_requests_get_an_error() {
        let pc = run("pc").await;

        let joined = ask(
            &pc.endpoint,
            Request::Join {
                invite: "not a pairing code".into(),
            },
        )
        .await;

        assert!(matches!(&joined[..], [Response::Error { .. }]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_to_start_twice_for_one_profile() {
        let pc = run("pc").await;

        assert!(serve(Arc::clone(&pc.daemon), &pc.endpoint).await.is_err());
    }

    #[tokio::test]
    async fn client_reports_a_stopped_daemon() {
        let home = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(home.path(), "nobody").unwrap();

        let result = call(&Endpoint::for_profile(&profile), &Request::Status, |_| {}).await;

        assert!(result.unwrap_err().to_string().contains("not running"));
    }
}
