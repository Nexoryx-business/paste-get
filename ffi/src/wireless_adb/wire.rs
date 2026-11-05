// SPDX-License-Identifier: GPL-3.0-or-later

//! The bytes of the two ADB protocols this module speaks, without any I/O, so they can be
//! tested and fuzzed on their own. Both are Android's own, from `packages/modules/adb` in AOSP:
//!
//! - **ADB messages** (`adb.h`, `protocol.txt`): a 24-byte little-endian header, then the data.
//! - **Pairing packets** (`pairing_connection/pairing_connection.cpp`): a 6-byte header
//!   (version, type, big-endian payload size), then the payload; and the fixed-size `PeerInfo`
//!   (`pairing_connection.h`).

pub const A_CNXN: u32 = 0x4e58_4e43;
pub const A_OPEN: u32 = 0x4e45_504f;
pub const A_OKAY: u32 = 0x5941_4b4f;
pub const A_CLSE: u32 = 0x4553_4c43;
pub const A_WRTE: u32 = 0x4554_5257;
pub const A_AUTH: u32 = 0x4854_5541;
pub const A_STLS: u32 = 0x534c_5453;

/// The protocol version sent in `CNXN`. From this one on, the data checksum isn't checked.
pub const A_VERSION: u32 = 0x0100_0001;
pub const A_STLS_VERSION: u32 = 0x0100_0000;
/// The most data one message may carry.
pub const MAX_PAYLOAD: u32 = 1024 * 1024;
pub const HEADER_LEN: usize = 24;

/// One ADB message.
#[derive(Clone, PartialEq, Eq)]
pub struct Message {
    pub command: u32,
    pub arg0: u32,
    pub arg1: u32,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Message {
    // Data may be command output: show its size only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message")
            .field("command", &command_name(self.command))
            .field("arg0", &self.arg0)
            .field("arg1", &self.arg1)
            .field("len", &self.data.len())
            .finish()
    }
}

/// A message header, read before its data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub command: u32,
    pub arg0: u32,
    pub arg1: u32,
    pub data_length: u32,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("bad message: the magic doesn't match the command")]
    BadMagic,
    #[error("message too large: {0} bytes")]
    TooLarge(u32),
    #[error("unsupported pairing packet version {0}")]
    PairingVersion(u8),
    #[error("empty pairing packet")]
    EmptyPacket,
    #[error("peer info has {0} bytes, not 8192")]
    PeerInfoSize(usize),
}

impl Message {
    pub fn new(command: u32, arg0: u32, arg1: u32, data: impl Into<Vec<u8>>) -> Self {
        Self {
            command,
            arg0,
            arg1,
            data: data.into(),
        }
    }

    /// Header and data, ready to send.
    pub fn encode(&self) -> Vec<u8> {
        // Callers never build messages past MAX_PAYLOAD.
        let len = u32::try_from(self.data.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(HEADER_LEN + self.data.len());
        for word in [
            self.command,
            self.arg0,
            self.arg1,
            len,
            checksum(&self.data),
            self.command ^ 0xffff_ffff,
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&self.data);
        out
    }
}

/// Parses a header. The data that follows isn't checked: adbd and the versions from
/// [`A_VERSION`] on skip the checksum.
pub fn parse_header(bytes: &[u8; HEADER_LEN]) -> Result<Header, WireError> {
    let word = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let header = Header {
        command: word(0),
        arg0: word(4),
        arg1: word(8),
        data_length: word(12),
    };
    if word(20) != header.command ^ 0xffff_ffff {
        return Err(WireError::BadMagic);
    }
    if header.data_length > MAX_PAYLOAD {
        return Err(WireError::TooLarge(header.data_length));
    }
    Ok(header)
}

/// The old data check, a plain byte sum. Devices still check it on the first `CNXN`, before
/// they know which version the client speaks.
pub fn checksum(data: &[u8]) -> u32 {
    data.iter()
        .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
}

/// For logs.
pub fn command_name(command: u32) -> &'static str {
    match command {
        A_CNXN => "CNXN",
        A_OPEN => "OPEN",
        A_OKAY => "OKAY",
        A_CLSE => "CLSE",
        A_WRTE => "WRTE",
        A_AUTH => "AUTH",
        A_STLS => "STLS",
        _ => "????",
    }
}

// --- Pairing -----------------------------------------------------------------------------------

pub const PAIRING_VERSION: u8 = 1;
pub const PAIRING_HEADER_LEN: usize = 6;
/// `PairingPacket.Type` in AOSP's `pairing.proto`.
pub const SPAKE2_MSG: u8 = 0;
pub const PEER_INFO: u8 = 1;
pub const PEER_INFO_LEN: usize = 8192;
pub const MAX_PAIRING_PAYLOAD: u32 = 2 * PEER_INFO_LEN as u32;
/// `PeerInfoType`: what a `PeerInfo` carries.
pub const PEER_RSA_PUBLIC_KEY: u8 = 0;
pub const PEER_DEVICE_GUID: u8 = 1;

pub fn pairing_header(kind: u8, payload_len: u32) -> [u8; PAIRING_HEADER_LEN] {
    let len = payload_len.to_be_bytes();
    [PAIRING_VERSION, kind, len[0], len[1], len[2], len[3]]
}

/// The packet type and payload size.
pub fn parse_pairing_header(bytes: &[u8; PAIRING_HEADER_LEN]) -> Result<(u8, u32), WireError> {
    if bytes[0] != PAIRING_VERSION {
        return Err(WireError::PairingVersion(bytes[0]));
    }
    let len = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    if len == 0 {
        return Err(WireError::EmptyPacket);
    }
    if len > MAX_PAIRING_PAYLOAD {
        return Err(WireError::TooLarge(len));
    }
    Ok((bytes[1], len))
}

/// A `PeerInfo`: the type, then `data` NUL-terminated and padded with zeros to 8192 bytes.
pub fn peer_info(kind: u8, data: &[u8]) -> Result<Vec<u8>, WireError> {
    // Room for the type byte and the final NUL.
    if data.len() > PEER_INFO_LEN - 2 {
        return Err(WireError::TooLarge(
            u32::try_from(data.len()).unwrap_or(u32::MAX),
        ));
    }
    let mut out = vec![0; PEER_INFO_LEN];
    out[0] = kind;
    out[1..=data.len()].copy_from_slice(data);
    Ok(out)
}

/// The type and the data of a `PeerInfo`, up to its first NUL.
pub fn parse_peer_info(bytes: &[u8]) -> Result<(u8, Vec<u8>), WireError> {
    if bytes.len() != PEER_INFO_LEN {
        return Err(WireError::PeerInfoSize(bytes.len()));
    }
    let data = &bytes[1..];
    let end = data.iter().position(|b| *b == 0).unwrap_or(data.len());
    Ok((bytes[0], data[..end].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_with_magic_and_checksum() {
        let message = Message::new(A_OPEN, 1, 0, b"exec:id\0".to_vec());
        let bytes = message.encode();

        assert_eq!(bytes.len(), HEADER_LEN + 8);
        // Command on the wire as its ASCII name, little-endian.
        assert_eq!(&bytes[..4], b"OPEN");
        assert_eq!(
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            b"exec:id\0".iter().map(|b| u32::from(*b)).sum::<u32>()
        );
        let header = parse_header(bytes[..HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!(
            header,
            Header {
                command: A_OPEN,
                arg0: 1,
                arg1: 0,
                data_length: 8
            }
        );
    }

    #[test]
    fn commands_are_their_ascii_names() {
        for (command, name) in [
            (A_CNXN, b"CNXN"),
            (A_OKAY, b"OKAY"),
            (A_CLSE, b"CLSE"),
            (A_WRTE, b"WRTE"),
            (A_AUTH, b"AUTH"),
            (A_STLS, b"STLS"),
        ] {
            assert_eq!(&command.to_le_bytes(), name);
        }
    }

    #[test]
    fn bad_headers_are_refused() {
        let mut bytes = Message::new(A_CNXN, A_VERSION, MAX_PAYLOAD, Vec::new()).encode();
        bytes[20] ^= 1;
        assert_eq!(
            parse_header(bytes[..HEADER_LEN].try_into().unwrap()),
            Err(WireError::BadMagic)
        );

        let mut big = Message::new(A_WRTE, 1, 1, Vec::new()).encode();
        big[12..16].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        assert_eq!(
            parse_header(big[..HEADER_LEN].try_into().unwrap()),
            Err(WireError::TooLarge(MAX_PAYLOAD + 1))
        );
    }

    #[test]
    fn pairing_headers_are_big_endian() {
        let header = pairing_header(PEER_INFO, 8208);
        assert_eq!(header, [1, 1, 0, 0, 0x20, 0x10]);
        assert_eq!(parse_pairing_header(&header), Ok((PEER_INFO, 8208)));

        assert_eq!(
            parse_pairing_header(&[2, 0, 0, 0, 0, 1]),
            Err(WireError::PairingVersion(2))
        );
        assert_eq!(
            parse_pairing_header(&[1, 0, 0, 0, 0, 0]),
            Err(WireError::EmptyPacket)
        );
        assert_eq!(
            parse_pairing_header(&pairing_header(SPAKE2_MSG, MAX_PAIRING_PAYLOAD + 1)),
            Err(WireError::TooLarge(MAX_PAIRING_PAYLOAD + 1))
        );
    }

    #[test]
    fn peer_info_is_8192_bytes_nul_terminated() {
        let info = peer_info(PEER_RSA_PUBLIC_KEY, b"QAAAA key paste-get@phone").unwrap();

        assert_eq!(info.len(), PEER_INFO_LEN);
        assert_eq!(info[0], PEER_RSA_PUBLIC_KEY);
        assert_eq!(&info[1..26], b"QAAAA key paste-get@phone");
        assert!(info[26..].iter().all(|b| *b == 0));
        assert_eq!(
            parse_peer_info(&info),
            Ok((PEER_RSA_PUBLIC_KEY, b"QAAAA key paste-get@phone".to_vec()))
        );

        assert!(peer_info(0, &[b'x'; PEER_INFO_LEN - 1]).is_err());
        assert_eq!(parse_peer_info(&[0; 10]), Err(WireError::PeerInfoSize(10)));
    }
}
