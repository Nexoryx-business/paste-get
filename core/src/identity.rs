// SPDX-License-Identifier: GPL-3.0-or-later

//! Device identity: an Ed25519 key pair and the device ID derived from it.
//!
//! The device ID is the SHA-256 of the public key (SubjectPublicKeyInfo DER). It is what users
//! pair with and what TLS connections are pinned to. The self-signed certificate wrapping the
//! key is only a TLS envelope and is regenerated on load.

use std::fmt;
use std::str::FromStr;

use data_encoding::BASE32_NOPAD;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ED25519};
use ring::digest::{SHA256, digest};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::ParsedCertificate;
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("could not create or load the key pair: {0}")]
    Key(#[from] rcgen::Error),
    #[error("could not read the device certificate: {0}")]
    Certificate(#[from] rustls::Error),
}

/// SHA-256 of a device's public key. Not secret.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId([u8; DeviceId::LEN]);

impl DeviceId {
    pub const LEN: usize = 32;

    pub const fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }

    /// Device ID of the public key inside an X.509 certificate.
    pub fn of_certificate(cert: &CertificateDer<'_>) -> Result<Self, rustls::Error> {
        let parsed = ParsedCertificate::try_from(cert)?;
        let spki = parsed.subject_public_key_info();
        let mut id = [0; Self::LEN];
        id.copy_from_slice(digest(&SHA256, spki.as_ref()).as_ref());
        Ok(Self(id))
    }
}

/// Wrong length when converting bytes or text into a [`DeviceId`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a valid device ID")]
pub struct InvalidDeviceId;

impl TryFrom<&[u8]> for DeviceId {
    type Error = InvalidDeviceId;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        bytes.try_into().map(Self).map_err(|_| InvalidDeviceId)
    }
}

/// Unpadded uppercase base32: 52 characters, compact in a QR code's alphanumeric mode.
impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&BASE32_NOPAD.encode(&self.0))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

impl FromStr for DeviceId {
    type Err = InvalidDeviceId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = BASE32_NOPAD
            .decode(s.as_bytes())
            .map_err(|_| InvalidDeviceId)?;
        Self::try_from(bytes.as_slice())
    }
}

/// This device's key pair and TLS certificate.
pub struct Identity {
    key_pkcs8: Zeroizing<Vec<u8>>,
    certificate: CertificateDer<'static>,
    device_id: DeviceId,
}

impl Identity {
    /// Creates a new random identity.
    pub fn generate() -> Result<Self, IdentityError> {
        Self::from_key(KeyPair::generate_for(&PKCS_ED25519)?)
    }

    /// Loads an identity saved with [`Identity::pkcs8_der`].
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, IdentityError> {
        Self::from_key(KeyPair::try_from(der)?)
    }

    fn from_key(key: KeyPair) -> Result<Self, IdentityError> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "paste-get device");
        params.distinguished_name = name;
        let certificate = params.self_signed(&key)?.der().clone();
        let device_id = DeviceId::of_certificate(&certificate)?;

        Ok(Self {
            key_pkcs8: Zeroizing::new(key.serialize_der()),
            certificate,
            device_id,
        })
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// The private key, PKCS#8 DER. **Secret**: store it in the OS keystore, never log it.
    pub fn pkcs8_der(&self) -> &[u8] {
        &self.key_pkcs8
    }

    pub(crate) fn certificate(&self) -> CertificateDer<'static> {
        self.certificate.clone()
    }

    pub(crate) fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.key_pkcs8.to_vec()).into()
    }
}

/// Shows the device ID only, never the key.
impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloading_keeps_the_device_id() {
        let identity = Identity::generate().unwrap();

        let reloaded = Identity::from_pkcs8_der(identity.pkcs8_der()).unwrap();

        assert_eq!(reloaded.device_id(), identity.device_id());
    }

    #[test]
    fn identities_are_unique() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();

        assert_ne!(a.device_id(), b.device_id());
    }

    #[test]
    fn device_id_text_round_trips() {
        let id = Identity::generate().unwrap().device_id();

        let text = id.to_string();

        assert_eq!(text.len(), 52);
        assert!(
            text.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
        assert_eq!(text.parse::<DeviceId>().unwrap(), id);
    }

    #[test]
    fn rejects_malformed_device_ids() {
        assert_eq!("not base32!".parse::<DeviceId>(), Err(InvalidDeviceId));
        assert_eq!(DeviceId::try_from(&[0u8; 31][..]), Err(InvalidDeviceId));
    }

    #[test]
    fn debug_never_shows_the_key() {
        let identity = Identity::generate().unwrap();

        let shown = format!("{identity:?}");

        assert!(shown.contains(&identity.device_id().to_string()));
        assert!(!shown.contains("key"));
    }

    #[test]
    fn garbage_key_is_rejected() {
        assert!(Identity::from_pkcs8_der(b"not a key").is_err());
    }
}
