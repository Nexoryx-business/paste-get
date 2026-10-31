# paste-get wire protocol — version 0

Status: **draft**, may change without notice until v0.1 ships.
The schema lives in [`proto/pasteget.proto`](../proto/pasteget.proto); this document explains
how the messages are used. Both must change in the same commit.

Sections still marked *(pending)* are filled in as the roadmap's M1 steps land.

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

## 2. Transport *(pending)*

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

### 4.2 Sending a clip: `ClipOffer`, `ClipChunk`, `Ack`

1. The sender picks a random 16-byte `clip_id` and sends a `ClipOffer` with the clip's
   SHA-256, MIME type and total size.
2. It then sends the bytes in `ClipChunk` messages, **in order and without gaps**: the first
   chunk has `offset = 0`, each next chunk starts where the previous one ended. A clip of size 0
   has no chunks.
3. When `offset + data.len() == size`, the receiver checks the SHA-256 and answers with an `Ack`.

Only one clip is in flight per direction at a time: a new `ClipOffer` cancels an unfinished one.

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

## 5. Pairing *(pending)*

## 6. Echo loops *(pending)*

## 7. Compatibility rules

- Field numbers are never reused or renumbered. Removed fields are marked `reserved`.
- New optional fields and new `oneof` payloads may be added without bumping the version:
  receivers ignore payloads they don't know.
- Anything that changes the meaning of existing fields bumps `protocol_version`.
