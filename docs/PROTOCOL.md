# paste-get wire protocol — version 0

Status: **draft**, may change without notice until v0.1 ships.
The schema lives in [`proto/pasteget.proto`](../proto/pasteget.proto); this document explains
how the messages are used. Both must change in the same commit.

## 1. Overview

paste-get devices talk over a single long-lived connection per pair of devices.
Every message is a Protobuf `Envelope` holding exactly one payload.

```
phone                                   PC
  │── TLS 1.3 handshake (mutual auth) ───▶│
  │── Hello ─────────────────────────────▶│
  │◀──────────────────────────── Hello ───│
  │── ClipOffer ─────────────────────────▶│
  │── ClipChunk (offset 0) ──────────────▶│
  │── ClipChunk (offset n) ──────────────▶│
  │◀────────────────────────── Ack(OK) ───│
  │◀──────────────────────────── Ping ────│
  │── Pong ──────────────────────────────▶│
```

## 2. Transport and trust

### 2.1 Device identity

Each device has an **Ed25519 key pair**, generated on first launch and kept in the OS keystore.
Its **device ID** is the SHA-256 of the public key's DER-encoded SubjectPublicKeyInfo:
32 bytes, shown as 52 characters of unpadded uppercase base32 (RFC 4648).

The key is wrapped in a self-signed X.509 certificate only because TLS needs one. Nothing in the
certificate except the public key is meaningful: names, validity dates and serial numbers are
ignored, and the certificate may be regenerated at any time without changing the device ID.

### 2.2 TLS

- TCP, then **TLS 1.3 only**, with ALPN `pasteget/0`. A peer that doesn't negotiate this ALPN
  is disconnected.
- **Both sides present a certificate** (mutual TLS).
- The connecting side sends no SNI (the server name is an IP address).

### 2.3 Who is trusted

- **The connecting side pins the device ID** of the device it wants to reach: if the server's
  public key hashes to anything else, the handshake fails. No certificate authority is involved.
- **The accepting side** requires a client certificate and lets TLS verify the client holds its
  key, but accepts any key at the TLS level, because an unknown device may be pairing (section 5).
  Right after the handshake it looks up the client's device ID:
  - **paired device** → normal session;
  - **pairing window open** → the client must complete pairing (section 5) as its first message;
  - **otherwise** → close the connection before reading anything.

In version 0, the PC accepts connections and the phone connects.

## 3. Framing

Inside the TLS stream, each message is one frame:

```
┌───────────────────────┬──────────────────────────────┐
│ length: u32 big-endian│ Envelope (Protobuf), length B│
└───────────────────────┴──────────────────────────────┘
```

- `length` counts the Envelope bytes only, not the 4-byte header.
- **Maximum `length`: 1 MiB (1 048 576).** A receiver that reads a larger length closes the
  connection without reading the body.
- `length = 0` is valid: an Envelope with no payload. Receivers ignore Envelopes whose payload
  they don't know (that's how newer peers add messages).
- A frame that doesn't decode as an Envelope is a fatal error: close the connection.

## 4. Messages

### 4.1 `Hello`

Each side sends `Hello` first, right after the TLS handshake, and must not send anything else
before it.

| Field | Meaning |
|---|---|
| `protocol_version` | Version spoken by the sender (`0`) |
| `device_name` | Human-readable name shown to the user |
| `app_version` | Sender's app version, for diagnostics only |
| `platform` | `ANDROID`, `WINDOWS`, `LINUX` or `MACOS` |

If the versions differ, the side with the higher version decides whether it can talk down;
otherwise it closes the connection. Version 0 implementations close on any mismatch.

`device_name` is untrusted: receivers sanitize it as in section 5.1.

### 4.2 Sending a clip: `ClipOffer`, `ClipChunk`, `Ack`

1. The sender picks a random 16-byte `clip_id` and sends a `ClipOffer` with the clip's
   SHA-256, MIME type and total size.
2. It then sends the bytes in `ClipChunk` messages, **in order and without gaps**: the first
   chunk has `offset = 0`, each next chunk starts where the previous one ended. A clip of size 0
   has no chunks.
3. When `offset + data.len() == size`, the receiver checks the SHA-256 and answers with an `Ack`.

Only one clip is in flight per direction at a time: a new `ClipOffer` cancels an unfinished one.

**Limits and checks:**
- A chunk carries **1 to 262 144 bytes** (256 KiB). Senders use full-size chunks except the last.
- Receivers choose their maximum clip size (default **64 MiB**) and answer `TOO_LARGE` to bigger
  offers right away. They must not allocate `size` bytes up front based on the offer alone.
- `clip_id` is 16 bytes, `sha256` and `origin_device` are 32 bytes; anything else is a
  `PROTOCOL_ERROR`.
- When the clip is complete, the receiver checks the hash, then that the content matches its type:
  valid UTF-8 for text, the PNG signature for images. Failing content is a `PROTOCOL_ERROR`.
- Any `PROTOCOL_ERROR` discards the clip in flight.

| `Ack.status` | When |
|---|---|
| `OK` | Clip complete and hash verified |
| `HASH_MISMATCH` | All bytes received but the hash differs: the clip is discarded |
| `TOO_LARGE` | `size` exceeds what the receiver accepts: sent instead of waiting for chunks |
| `UNSUPPORTED_TYPE` | The receiver can't use this MIME type |
| `PROTOCOL_ERROR` | Chunk for an unknown clip, wrong offset, or more bytes than announced |

**MIME types in version 0:**
- `text/plain;charset=utf-8`: UTF-8 text without byte-order mark
- `image/png`: images are always converted to PNG before sending

**`origin_device`** is the device ID of the device where the clip was first copied. A device
never re-sends a clip it received (see *Echo loops*), and ignores offers whose
`origin_device` is itself.

**`sensitive`** means the source marked the clip as secret (password, 2FA code). Receivers put it
on the clipboard but must keep it out of history, notifications and OS clipboard managers.

### 4.3 `Ping` / `Pong`

Either side may send `Ping` at any time; the peer answers with a `Pong` carrying the same
`nonce`. Used to detect dead connections.

## 5. Pairing

Pairing makes two devices trust each other's device ID. In version 0 the PC shows a QR code and
the phone scans it.

### 5.1 The QR code

```
pasteget://pair?v=0&id=<device ID>&s=<secret>&a=<addresses>&n=<name>
```

The query uses `application/x-www-form-urlencoded` encoding.

| Key | Value |
|---|---|
| `v` | Invite format version: `0` |
| `id` | Device ID of the PC (52 base32 characters) |
| `s` | One-time secret: 16 random bytes, unpadded base32 (26 characters) |
| `a` | Comma-separated socket addresses where the PC listens, e.g. `192.168.1.20:47290,[fe80::1]:47290` |
| `n` | PC name, for display |

Rules for readers:
- Reject invites longer than 1024 bytes, with a version other than `0`, with a duplicate key, or
  missing `id`, `s` or `a`.
- Ignore unknown keys: they are reserved for later versions.
- Device names are untrusted: strip control characters, trim, keep at most 64 characters.

### 5.2 Exchange

1. The PC opens a **pairing window**: a fresh secret, valid for **5 minutes**, for **one**
   successful pairing, and closed after **3 wrong secrets**.
2. The phone connects to one of the addresses, **pinning the device ID from the QR code**
   (section 2.3).
3. The PC sees an unknown client while its window is open. It waits for a `PairRequest`, and
   anything else closes the connection.
4. The phone sends `PairRequest { secret, device_name }`.
5. The PC compares the secret in constant time and answers `PairResponse { accepted,
   device_name }`. Both sides close the connection.
6. If accepted, each side stores the other's device ID and name. From then on they connect with
   ordinary pinned TLS.

### 5.3 Security properties

- **The phone can't be tricked into pairing with another machine**: the QR code carries the PC's
  device ID, and TLS fails for any other key.
- **The PC only pairs with a device that saw its screen**: the secret is only in the QR code and
  only travels inside TLS.
- **Guessing is useless**: 128-bit secret, 3 attempts, 5 minutes.
- **Not protected**: someone who photographs the QR code and pairs before the owner does within
  the 5 minutes. The PC must show which device just paired, and allow revoking it.

## 6. Echo loops

Putting a received clip on the clipboard makes the local clipboard watcher fire. Without care,
the clip would be sent straight back, forever. Two rules prevent it:

1. **Send only what's new.** Each device remembers a fingerprint of the clipboard's last known
   content: SHA-256 over the MIME type, a zero byte, and the data. A received clip updates that
   fingerprint when it's applied. A local change is sent only if its fingerprint differs. This also
   absorbs operating systems that report one change several times.
2. **Never accept your own clip.** Offers whose `origin_device` is the receiver's own device ID
   are ignored (they can come back once devices relay clips to each other).

On Android 13+, clips put on the clipboard from a peer also carry `EXTRA_IS_REMOTE_DEVICE`, so
the system UI shows them correctly.

## 7. Compatibility rules

- Field numbers are never reused or renumbered. Removed fields are marked `reserved`.
- New optional fields and new `oneof` payloads may be added without bumping the version:
  receivers ignore payloads they don't know.
- Anything that changes the meaning of existing fields bumps `protocol_version`.
