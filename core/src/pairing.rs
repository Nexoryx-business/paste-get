// SPDX-License-Identifier: GPL-3.0-or-later

//! Pairing a new device by scanning a QR code. See `docs/PROTOCOL.md`, section 5.
//!
//! 1. The accepting device (the PC) opens a [`PairingWindow`] and shows its [`PairingInvite`]
//!    as a QR code: device ID, addresses and a one-time secret.
//! 2. The new device scans it and calls [`pair_with`]: it connects pinned to the device ID from
//!    the QR code, so it can't be tricked into pairing with another machine.
//! 3. It sends the secret inside the TLS connection. [`accept_pairing`] checks it, which proves
//!    the new device could see the QR code.
//!
//! Each side then stores the other's [`PairedDevice`]; later connections are ordinary pinned TLS.

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};

use data_encoding::BASE32_NOPAD;
use ring::rand::{SecureRandom, SystemRandom};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::Zeroizing;

use crate::identity::{DeviceId, Identity};
use crate::proto::{Envelope, PairRequest, PairResponse, envelope::Payload};
use crate::transport::{self, TransportError, UnverifiedConnection};

/// How long a pairing QR code stays valid.
pub const PAIRING_TTL: Duration = Duration::from_secs(5 * 60);

/// Wrong secrets tolerated before the window closes.
pub const MAX_ATTEMPTS: u8 = 3;

/// Longest device name kept, in characters.
pub const MAX_DEVICE_NAME_CHARS: usize = 64;

/// Longest invite URI accepted, in bytes.
pub const MAX_INVITE_LEN: usize = 1024;

const SECRET_LEN: usize = 16;
const INVITE_PREFIX: &str = "pasteget://pair?";

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("could not generate a random secret")]
    Random,
    #[error("the pairing code has expired")]
    Expired,
    #[error("the pairing code was already used or too many wrong attempts were made")]
    Closed,
    #[error("wrong pairing code")]
    WrongSecret,
    #[error("the other device refused to pair")]
    Rejected,
    #[error("the other device broke the pairing protocol")]
    Protocol,
}

/// The one-time secret shown in the pairing QR code.
#[derive(Clone)]
pub struct PairingSecret(Zeroizing<[u8; SECRET_LEN]>);

impl PairingSecret {
    fn generate() -> Result<Self, PairingError> {
        let mut bytes = Zeroizing::new([0; SECRET_LEN]);
        SystemRandom::new()
            .fill(bytes.as_mut())
            .map_err(|_| PairingError::Random)?;
        Ok(Self(bytes))
    }

    fn from_slice(bytes: &[u8]) -> Option<Self> {
        let array: [u8; SECRET_LEN] = bytes.try_into().ok()?;
        Some(Self(Zeroizing::new(array)))
    }

    /// Constant-time comparison, so response timing reveals nothing about the secret.
    fn matches(&self, candidate: &[u8]) -> bool {
        candidate.ct_eq(self.0.as_slice()).into()
    }
}

impl fmt::Debug for PairingSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingSecret(..)")
    }
}

/// Everything the QR code carries.
#[derive(Clone, Debug)]
pub struct PairingInvite {
    pub device_id: DeviceId,
    pub device_name: String,
    /// Where the inviting device listens. The new device tries them in order.
    pub addresses: Vec<SocketAddr>,
    pub secret: PairingSecret,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a valid paste-get pairing code")]
pub struct InvalidInvite;

impl PairingInvite {
    /// The URI to encode in the QR code.
    pub fn to_uri(&self) -> String {
        let addresses = self
            .addresses
            .iter()
            .map(SocketAddr::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("v", "0")
            .append_pair("id", &self.device_id.to_string())
            .append_pair("s", &BASE32_NOPAD.encode(self.secret.0.as_slice()))
            .append_pair("a", &addresses)
            .append_pair("n", &self.device_name)
            .finish();
        format!("{INVITE_PREFIX}{query}")
    }
}

impl FromStr for PairingInvite {
    type Err = InvalidInvite;

    /// Parses a scanned QR code. Input is untrusted (fuzzed in `fuzz/`).
    fn from_str(uri: &str) -> Result<Self, Self::Err> {
        if uri.len() > MAX_INVITE_LEN {
            return Err(InvalidInvite);
        }
        let query = uri.strip_prefix(INVITE_PREFIX).ok_or(InvalidInvite)?;

        let (mut version, mut id, mut secret, mut addresses, mut name) =
            (None, None, None, None, None);
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            let slot = match key.as_ref() {
                "v" => &mut version,
                "id" => &mut id,
                "s" => &mut secret,
                "a" => &mut addresses,
                "n" => &mut name,
                _ => continue, // Unknown keys are reserved for newer versions.
            };
            if slot.replace(value).is_some() {
                return Err(InvalidInvite); // Duplicate key.
            }
        }

        if version.as_deref() != Some("0") {
            return Err(InvalidInvite);
        }
        let device_id = id
            .ok_or(InvalidInvite)?
            .parse()
            .map_err(|_| InvalidInvite)?;
        let secret_bytes = Zeroizing::new(
            BASE32_NOPAD
                .decode(secret.ok_or(InvalidInvite)?.as_bytes())
                .map_err(|_| InvalidInvite)?,
        );
        let secret = PairingSecret::from_slice(&secret_bytes).ok_or(InvalidInvite)?;
        let addresses = addresses
            .ok_or(InvalidInvite)?
            .split(',')
            .map(SocketAddr::from_str)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| InvalidInvite)?;

        Ok(Self {
            device_id,
            device_name: sanitize_device_name(&name.unwrap_or_default()),
            addresses,
            secret,
        })
    }
}

/// A device both sides have agreed to trust. The caller stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedDevice {
    pub device_id: DeviceId,
    pub device_name: String,
}

/// A pairing QR code being shown on the accepting device.
#[derive(Debug)]
pub struct PairingWindow {
    secret: PairingSecret,
    expires_at: Instant,
    wrong_attempts: u8,
    closed: bool,
}

impl PairingWindow {
    /// Opens a window that lasts [`PAIRING_TTL`].
    pub fn open(now: Instant) -> Result<Self, PairingError> {
        Ok(Self {
            secret: PairingSecret::generate()?,
            expires_at: now + PAIRING_TTL,
            wrong_attempts: 0,
            closed: false,
        })
    }

    /// The invite to show as a QR code.
    pub fn invite(
        &self,
        identity: &Identity,
        device_name: &str,
        addresses: Vec<SocketAddr>,
    ) -> PairingInvite {
        PairingInvite {
            device_id: identity.device_id(),
            device_name: sanitize_device_name(device_name),
            addresses,
            secret: self.secret.clone(),
        }
    }

    /// Whether new pairing attempts can still succeed.
    pub fn is_open(&self, now: Instant) -> bool {
        !self.closed && now < self.expires_at
    }

    /// Checks a secret. A match closes the window: each code pairs one device.
    fn check(&mut self, candidate: &[u8], now: Instant) -> Result<(), PairingError> {
        if self.closed {
            return Err(PairingError::Closed);
        }
        if now >= self.expires_at {
            return Err(PairingError::Expired);
        }
        if self.secret.matches(candidate) {
            self.closed = true;
            return Ok(());
        }
        self.wrong_attempts += 1;
        if self.wrong_attempts >= MAX_ATTEMPTS {
            self.closed = true;
        }
        Err(PairingError::WrongSecret)
    }
}

/// New-device side: connects to the device from `invite` over `io` and pairs.
///
/// `io` must be connected to one of `invite.addresses`.
pub async fn pair_with<IO>(
    io: IO,
    identity: &Identity,
    invite: &PairingInvite,
    device_name: &str,
) -> Result<PairedDevice, PairingError>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    let mut connection = transport::connect(io, identity, invite.device_id).await?;
    connection
        .send(&Envelope {
            payload: Some(Payload::PairRequest(PairRequest {
                secret: invite.secret.0.to_vec(),
                device_name: sanitize_device_name(device_name),
            })),
        })
        .await?;

    let response = connection.recv().await?;
    // The pairing result doesn't depend on a clean shutdown.
    let _ = connection.close().await;
    match response {
        Some(Envelope {
            payload:
                Some(Payload::PairResponse(PairResponse {
                    accepted: true,
                    device_name,
                })),
        }) => Ok(PairedDevice {
            device_id: invite.device_id,
            device_name: sanitize_device_name(&device_name),
        }),
        Some(Envelope {
            payload:
                Some(Payload::PairResponse(PairResponse {
                    accepted: false, ..
                })),
        }) => Err(PairingError::Rejected),
        _ => Err(PairingError::Protocol),
    }
}

/// Accepting side: handles a connection from a device that is not paired yet.
///
/// Call it only for peers that failed [`UnverifiedConnection::authorize`] while `window` is open.
pub async fn accept_pairing<S>(
    connection: UnverifiedConnection<S>,
    window: &mut PairingWindow,
    device_name: &str,
) -> Result<PairedDevice, PairingError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut connection = connection.into_pairing();
    let peer = connection.peer();
    let Some(Envelope {
        payload: Some(Payload::PairRequest(request)),
    }) = connection.recv().await?
    else {
        return Err(PairingError::Protocol);
    };

    let secret = Zeroizing::new(request.secret);
    let verdict = window.check(&secret, Instant::now());
    connection
        .send(&Envelope {
            payload: Some(Payload::PairResponse(PairResponse {
                accepted: verdict.is_ok(),
                device_name: sanitize_device_name(device_name),
            })),
        })
        .await?;
    let _ = connection.close().await;

    verdict.map(|()| PairedDevice {
        device_id: peer,
        device_name: sanitize_device_name(&request.device_name),
    })
}

/// Removes control characters, trims, and caps the length of a name received from a peer.
pub fn sanitize_device_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME_CHARS)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Unknown device".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn sample_invite() -> (Identity, PairingWindow, PairingInvite) {
        let pc = Identity::generate().unwrap();
        let window = PairingWindow::open(Instant::now()).unwrap();
        let addresses = vec![
            "192.168.1.20:47290".parse().unwrap(),
            "[fe80::1]:47290".parse().unwrap(),
        ];
        let invite = window.invite(&pc, "HP-AIO (Arch)", addresses);
        (pc, window, invite)
    }

    #[test]
    fn invite_uri_round_trips() {
        let (_, _, mut invite) = sample_invite();
        invite.device_name = "Bureau d'été & co".into();

        let uri = invite.to_uri();
        let parsed: PairingInvite = uri.parse().unwrap();

        assert!(uri.starts_with("pasteget://pair?v=0&"));
        assert_eq!(parsed.device_id, invite.device_id);
        assert_eq!(parsed.device_name, invite.device_name);
        assert_eq!(parsed.addresses, invite.addresses);
        assert!(parsed.secret.matches(invite.secret.0.as_slice()));
    }

    #[test]
    fn invite_ignores_unknown_keys() {
        let uri = format!("{}&future=1", sample_invite().2.to_uri());

        assert!(uri.parse::<PairingInvite>().is_ok());
    }

    #[test]
    fn rejects_malformed_invites() {
        let valid = sample_invite().2.to_uri();
        let malformed = [
            String::new(),
            "https://example.com/?v=0".into(),
            valid.replace("v=0", "v=1"),
            valid.replacen("id=", "id=X", 1),
            valid.replace("&s=", "&x="),
            valid.replace("47290", "port"),
            format!("{valid}&v=0"),
            format!("{valid}&pad={}", "a".repeat(MAX_INVITE_LEN)),
        ];

        for uri in malformed {
            assert_eq!(
                uri.parse::<PairingInvite>().err(),
                Some(InvalidInvite),
                "{uri}"
            );
        }
    }

    #[test]
    fn window_accepts_the_secret_once() {
        let now = Instant::now();
        let mut window = PairingWindow::open(now).unwrap();
        let secret = window.secret.clone();

        assert!(window.check(secret.0.as_slice(), now).is_ok());
        assert!(matches!(
            window.check(secret.0.as_slice(), now),
            Err(PairingError::Closed)
        ));
        assert!(!window.is_open(now));
    }

    #[test]
    fn window_expires() {
        let now = Instant::now();
        let mut window = PairingWindow::open(now).unwrap();
        let secret = window.secret.clone();
        let later = now + PAIRING_TTL;

        assert!(!window.is_open(later));
        assert!(matches!(
            window.check(secret.0.as_slice(), later),
            Err(PairingError::Expired)
        ));
    }

    #[test]
    fn window_closes_after_too_many_wrong_secrets() {
        let now = Instant::now();
        let mut window = PairingWindow::open(now).unwrap();
        let secret = window.secret.clone();

        for _ in 0..MAX_ATTEMPTS {
            assert!(matches!(
                window.check(&[0; SECRET_LEN], now),
                Err(PairingError::WrongSecret)
            ));
        }
        assert!(matches!(
            window.check(secret.0.as_slice(), now),
            Err(PairingError::Closed)
        ));
    }

    #[test]
    fn secret_never_shows_in_debug_output() {
        let (_, window, invite) = sample_invite();
        let encoded = BASE32_NOPAD.encode(invite.secret.0.as_slice());

        assert!(!format!("{window:?} {invite:?}").contains(&encoded));
        assert!(!format!("{:?}", invite.secret).contains(&format!("{:?}", &invite.secret.0[..])));
    }

    #[test]
    fn sanitizes_device_names() {
        assert_eq!(sanitize_device_name("\u{7}Evil\nName  "), "EvilName");
        assert_eq!(sanitize_device_name(" \t"), "Unknown device");
        assert_eq!(
            sanitize_device_name(&"x".repeat(200)).chars().count(),
            MAX_DEVICE_NAME_CHARS
        );
    }

    #[tokio::test]
    async fn devices_pair_then_reconnect_as_trusted() {
        let (pc, mut window, invite) = sample_invite();
        let phone = Identity::generate().unwrap();
        let (phone_io, pc_io) = duplex(64 * 1024);

        let (phone_result, pc_result) =
            tokio::join!(pair_with(phone_io, &phone, &invite, "POCO M8 5G"), async {
                let incoming = transport::accept(pc_io, &pc).await?;
                accept_pairing(incoming, &mut window, "HP-AIO (Arch)").await
            });
        let paired_pc = phone_result.unwrap();
        let paired_phone = pc_result.unwrap();

        assert_eq!(paired_pc.device_id, pc.device_id());
        assert_eq!(paired_pc.device_name, "HP-AIO (Arch)");
        assert_eq!(paired_phone.device_id, phone.device_id());
        assert_eq!(paired_phone.device_name, "POCO M8 5G");

        // Afterwards: ordinary pinned TLS, authorized from the stored pairing.
        let (phone_io, pc_io) = duplex(64 * 1024);
        let (client, server) = tokio::join!(
            transport::connect(phone_io, &phone, paired_pc.device_id),
            async {
                transport::accept(pc_io, &pc)
                    .await?
                    .authorize(|id| *id == paired_phone.device_id)
            }
        );
        assert!(client.is_ok());
        assert!(server.is_ok());
    }

    #[tokio::test]
    async fn wrong_secret_is_rejected() {
        let (pc, mut window, mut invite) = sample_invite();
        invite.secret = PairingSecret::generate().unwrap();
        let phone = Identity::generate().unwrap();
        let (phone_io, pc_io) = duplex(64 * 1024);

        let (phone_result, pc_result) =
            tokio::join!(pair_with(phone_io, &phone, &invite, "POCO M8 5G"), async {
                let incoming = transport::accept(pc_io, &pc).await?;
                accept_pairing(incoming, &mut window, "HP-AIO (Arch)").await
            });

        assert!(matches!(phone_result, Err(PairingError::Rejected)));
        assert!(matches!(pc_result, Err(PairingError::WrongSecret)));
    }

    #[tokio::test]
    async fn refuses_a_machine_other_than_the_one_in_the_qr_code() {
        let (_pc, _, invite) = sample_invite();
        let impostor = Identity::generate().unwrap();
        let mut impostor_window = PairingWindow::open(Instant::now()).unwrap();
        let phone = Identity::generate().unwrap();
        let (phone_io, impostor_io) = duplex(64 * 1024);

        let (phone_result, impostor_result) =
            tokio::join!(pair_with(phone_io, &phone, &invite, "POCO M8 5G"), async {
                let incoming = transport::accept(impostor_io, &impostor).await?;
                accept_pairing(incoming, &mut impostor_window, "Impostor").await
            });

        assert!(matches!(phone_result, Err(PairingError::Transport(_))));
        assert!(impostor_result.is_err());
    }
}
