use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use bytes::Bytes;
use iroh::endpoint::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::pool::{BufPool, SharedBuf};

/// MoQ SUBGROUP_HEADER stream type tag.
pub const STREAM_TYPE_SUBGROUP: u64 = 0x4;

/// Track aliases (used in place of MoQ track_alias).
pub const TRACK_AUDIO: u64 = 0;
pub const TRACK_VIDEO: u64 = 1;

/// Publisher priority values (MoQ: lower = more important).
pub const PRIORITY_AUDIO: u8 = 0;
pub const PRIORITY_VIDEO: u8 = 1;

/// QUIC stream priority values (quinn: higher i32 = more important).
pub const QUIC_PRIORITY_AUDIO: i32 = 1;
pub const QUIC_PRIORITY_VIDEO: i32 = 0;

/// LOC extension header type for Capture Timestamp.
const LOC_EXT_CAPTURE_TIMESTAMP: u64 = 2;

/// LOC extension header type for Codec identifier.
const LOC_EXT_CODEC: u64 = 4;

/// Codec identifiers carried in LOC_EXT_CODEC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Codec {
    Av1 = 0,
    Hevc = 1,
}

impl Codec {
    pub fn from_u64(v: u64) -> Option<Self> {
        match v {
            0 => Some(Self::Av1),
            1 => Some(Self::Hevc),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Varint encoding/decoding (QUIC variable-length integer, RFC 9000 §16)
// ---------------------------------------------------------------------------

/// Encode a u64 as a QUIC varint into a stack buffer.
/// Returns `(bytes, len)` where `bytes[..len]` is the encoded varint.
pub fn encode_varint(val: u64) -> ([u8; 8], usize) {
    if val <= 0x3F {
        let mut buf = [0u8; 8];
        buf[0] = val as u8;
        (buf, 1)
    } else if val <= 0x3FFF {
        let v = (val as u16) | 0x4000;
        let b = v.to_be_bytes();
        let mut buf = [0u8; 8];
        buf[..2].copy_from_slice(&b);
        (buf, 2)
    } else if val <= 0x3FFF_FFFF {
        let v = (val as u32) | 0x8000_0000;
        let b = v.to_be_bytes();
        let mut buf = [0u8; 8];
        buf[..4].copy_from_slice(&b);
        (buf, 4)
    } else {
        let v = val | 0xC000_0000_0000_0000;
        (v.to_be_bytes(), 8)
    }
}

/// Write a QUIC varint to an async writer.
pub async fn write_varint(
    w: &mut (impl AsyncWriteExt + Unpin),
    val: u64,
) -> Result<()> {
    let (buf, len) = encode_varint(val);
    w.write_all(&buf[..len]).await?;
    Ok(())
}

/// Read a QUIC varint from an async reader.
pub async fn read_varint(
    r: &mut (impl AsyncReadExt + Unpin),
) -> Result<u64> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first).await?;
    let prefix = first[0] >> 6;
    match prefix {
        0 => Ok((first[0] & 0x3F) as u64),
        1 => {
            let mut second = [0u8; 1];
            r.read_exact(&mut second).await?;
            let val = u16::from_be_bytes([first[0] & 0x3F, second[0]]);
            Ok(val as u64)
        }
        2 => {
            let mut rest = [0u8; 3];
            r.read_exact(&mut rest).await?;
            let val = u32::from_be_bytes([first[0] & 0x3F, rest[0], rest[1], rest[2]]);
            Ok(val as u64)
        }
        3 => {
            let mut rest = [0u8; 7];
            r.read_exact(&mut rest).await?;
            let val = u64::from_be_bytes([
                first[0] & 0x3F,
                rest[0], rest[1], rest[2], rest[3], rest[4], rest[5], rest[6],
            ]);
            Ok(val)
        }
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// MoQ SUBGROUP_HEADER framing (draft-ietf-moq-transport-10 §9.4.2)
// ---------------------------------------------------------------------------

/// Subgroup header fields (after reading from a stream).
pub struct SubgroupHeader {
    pub track_alias: u64,
    pub group_id: u64,
    pub subgroup_id: u64,
    pub publisher_priority: u8,
}

/// Write a SUBGROUP_HEADER to a stream.
pub async fn write_subgroup_header(
    w: &mut (impl AsyncWriteExt + Unpin),
    track_alias: u64,
    group_id: u64,
    subgroup_id: u64,
    publisher_priority: u8,
) -> Result<()> {
    write_varint(w, STREAM_TYPE_SUBGROUP).await?;
    write_varint(w, track_alias).await?;
    write_varint(w, group_id).await?;
    write_varint(w, subgroup_id).await?;
    w.write_all(&[publisher_priority]).await?;
    Ok(())
}

/// Read a SUBGROUP_HEADER from a stream.
/// Validates that stream_type == 0x4.
pub async fn read_subgroup_header(
    r: &mut (impl AsyncReadExt + Unpin),
) -> Result<SubgroupHeader> {
    let stream_type = read_varint(r).await?;
    if stream_type != STREAM_TYPE_SUBGROUP {
        return Err(anyhow!(
            "Expected SUBGROUP_HEADER (0x4), got 0x{:x}",
            stream_type
        ));
    }
    let track_alias = read_varint(r).await?;
    let group_id = read_varint(r).await?;
    let subgroup_id = read_varint(r).await?;
    let mut prio = [0u8; 1];
    r.read_exact(&mut prio).await?;
    Ok(SubgroupHeader {
        track_alias,
        group_id,
        subgroup_id,
        publisher_priority: prio[0],
    })
}

// ---------------------------------------------------------------------------
// MoQ Object with LOC extensions (draft-ietf-moq-transport-10 §9.4.2)
// ---------------------------------------------------------------------------
//
// On-wire layout per object within a subgroup stream:
//
//   Object ID (varint)
//   Extension Headers Length (varint)
//   [Extension Headers (...)]
//   Object Payload Length (varint)
//   [Object Status (varint)]   — only if payload length is 0
//   Object Payload (..)
//
// We write a single LOC extension: Capture Timestamp (ID=2).
// Even-typed extensions use: Header Type (varint) + Header Value (varint).

/// Build the serialised extension headers block (Capture Timestamp + Codec)
/// into a stack buffer. Returns `(bytes, len)`.
///
/// Maximum size: 4 varints × 8 bytes = 32 bytes.
fn build_extension_headers(capture_timestamp_us: u64, codec: Option<Codec>) -> ([u8; 32], usize) {
    let mut buf = [0u8; 32];
    let mut pos = 0;

    let (v, n) = encode_varint(LOC_EXT_CAPTURE_TIMESTAMP);
    buf[pos..pos + n].copy_from_slice(&v[..n]);
    pos += n;

    let (v, n) = encode_varint(capture_timestamp_us);
    buf[pos..pos + n].copy_from_slice(&v[..n]);
    pos += n;

    if let Some(c) = codec {
        let (v, n) = encode_varint(LOC_EXT_CODEC);
        buf[pos..pos + n].copy_from_slice(&v[..n]);
        pos += n;

        let (v, n) = encode_varint(c as u64);
        buf[pos..pos + n].copy_from_slice(&v[..n]);
        pos += n;
    }

    (buf, pos)
}

/// Write one MoQ object with LOC Capture Timestamp and optional Codec extension.
pub async fn write_object(
    w: &mut (impl AsyncWriteExt + Unpin),
    object_id: u64,
    capture_timestamp_us: u64,
    codec: Option<Codec>,
    payload: &[u8],
) -> Result<()> {
    write_varint(w, object_id).await?;

    // Extension headers
    let (ext, ext_len) = build_extension_headers(capture_timestamp_us, codec);
    write_varint(w, ext_len as u64).await?;
    w.write_all(&ext[..ext_len]).await?;

    // Payload
    write_varint(w, payload.len() as u64).await?;
    w.write_all(payload).await?;
    Ok(())
}

/// Parsed extension header values.
struct ExtensionValues {
    capture_timestamp_us: u64,
    codec: Option<Codec>,
}

/// Parse extension headers from a byte slice, extracting known LOC extensions.
/// Unknown extensions are skipped.
fn parse_extension_headers(mut data: &[u8]) -> Result<ExtensionValues> {
    let mut vals = ExtensionValues {
        capture_timestamp_us: 0,
        codec: None,
    };

    while !data.is_empty() {
        let (header_type, rest) = decode_varint_from_slice(data)?;
        data = rest;

        if header_type % 2 == 0 {
            // Even type: value is a single varint
            let (value, rest) = decode_varint_from_slice(data)?;
            data = rest;
            if header_type == LOC_EXT_CAPTURE_TIMESTAMP {
                vals.capture_timestamp_us = value;
            } else if header_type == LOC_EXT_CODEC {
                vals.codec = Codec::from_u64(value);
            }
        } else {
            // Odd type: varint length + length bytes of value
            let (length, rest) = decode_varint_from_slice(data)?;
            let length = length as usize;
            if rest.len() < length {
                return Err(anyhow!("Extension header truncated"));
            }
            data = &rest[length..];
        }
    }

    Ok(vals)
}

/// Decode a QUIC varint from the front of a byte slice, returning (value, remaining).
fn decode_varint_from_slice(data: &[u8]) -> Result<(u64, &[u8])> {
    if data.is_empty() {
        return Err(anyhow!("Unexpected end of varint"));
    }
    let first = data[0];
    let prefix = first >> 6;

    match prefix {
        0 => Ok(((first & 0x3F) as u64, &data[1..])),
        1 => {
            if data.len() < 2 {
                return Err(anyhow!("Varint truncated"));
            }
            let val = u16::from_be_bytes([first & 0x3F, data[1]]);
            Ok((val as u64, &data[2..]))
        }
        2 => {
            if data.len() < 4 {
                return Err(anyhow!("Varint truncated"));
            }
            let val = u32::from_be_bytes([first & 0x3F, data[1], data[2], data[3]]);
            Ok((val as u64, &data[4..]))
        }
        3 => {
            if data.len() < 8 {
                return Err(anyhow!("Varint truncated"));
            }
            let val = u64::from_be_bytes([
                first & 0x3F,
                data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]);
            Ok((val as u64, &data[8..]))
        }
        _ => unreachable!(),
    }
}

/// Parsed fields from a single MoQ object.
pub struct ObjectData {
    pub object_id: u64,
    pub capture_timestamp_us: u64,
    pub codec: Option<Codec>,
    pub payload: SharedBuf<u8>,
}

/// Read one MoQ object (with extension headers) from a stream.
/// Returns `Ok(None)` on clean stream finish (EOF on first byte),
/// `Ok(Some(..))` for a successfully read object, or `Err` on
/// protocol/IO errors mid-object.
///
/// The payload is read into a buffer from `payload_pool`, eliminating
/// per-frame heap allocations in steady state.
pub async fn read_object(
    r: &mut (impl AsyncReadExt + Unpin),
    payload_pool: &BufPool<u8>,
) -> Result<Option<ObjectData>> {
    // Try to read the first byte of the object_id varint.
    // A clean EOF here means the stream is finished (no more objects).
    let mut first = [0u8; 1];
    match r.read_exact(&mut first).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    // We got a byte — decode the rest of the object_id varint.
    let object_id = decode_varint_first_byte(first[0], r).await?;

    // Extension headers — use a stack buffer (extensions are always small,
    // typically ~20 bytes for capture timestamp + codec).
    let ext_len = read_varint(r).await? as usize;
    let exts = if ext_len > 0 {
        let mut ext_buf = [0u8; 64];
        if ext_len > ext_buf.len() {
            return Err(anyhow!("Extension headers too large: {ext_len}"));
        }
        r.read_exact(&mut ext_buf[..ext_len]).await?;
        parse_extension_headers(&ext_buf[..ext_len])?
    } else {
        ExtensionValues { capture_timestamp_us: 0, codec: None }
    };

    // Payload
    let payload_len = read_varint(r).await?;
    if payload_len == 0 {
        // Object Status follows for zero-length payloads; read and discard.
        let _status = read_varint(r).await?;
        return Ok(Some(ObjectData {
            object_id,
            capture_timestamp_us: exts.capture_timestamp_us,
            codec: exts.codec,
            payload: payload_pool.checkout(0).share(),
        }));
    }
    let mut buf = payload_pool.checkout(payload_len as usize);
    r.read_exact(&mut *buf).await?;
    Ok(Some(ObjectData {
        object_id,
        capture_timestamp_us: exts.capture_timestamp_us,
        codec: exts.codec,
        payload: buf.share(),
    }))
}

/// Finish decoding a varint given the first byte already read.
async fn decode_varint_first_byte(
    first: u8,
    r: &mut (impl AsyncReadExt + Unpin),
) -> Result<u64> {
    let prefix = first >> 6;
    match prefix {
        0 => Ok((first & 0x3F) as u64),
        1 => {
            let mut second = [0u8; 1];
            r.read_exact(&mut second).await?;
            let val = u16::from_be_bytes([first & 0x3F, second[0]]);
            Ok(val as u64)
        }
        2 => {
            let mut rest = [0u8; 3];
            r.read_exact(&mut rest).await?;
            let val = u32::from_be_bytes([first & 0x3F, rest[0], rest[1], rest[2]]);
            Ok(val as u64)
        }
        3 => {
            let mut rest = [0u8; 7];
            r.read_exact(&mut rest).await?;
            let val = u64::from_be_bytes([
                first & 0x3F,
                rest[0], rest[1], rest[2], rest[3], rest[4], rest[5], rest[6],
            ]);
            Ok(val)
        }
        _ => unreachable!(),
    }
}

/// A received media object with MoQ group/subgroup/object IDs and LOC capture timestamp.
///
/// The payload is backed by a [`SharedBuf`] from a pool — when the last
/// consumer drops it, the buffer returns to the pool rather than being freed.
pub struct MediaObject {
    pub group_id: u64,
    /// Subgroup 0 = independently decodable (keyframe for video).
    pub subgroup_id: u64,
    pub object_id: u64,
    pub capture_timestamp_us: u64,
    pub codec: Option<Codec>,
    pub payload: SharedBuf<u8>,
}

/// Return the current wall-clock time as microseconds since the Unix epoch.
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

// ---------------------------------------------------------------------------
// Audio datagram framing
// ---------------------------------------------------------------------------
//
// Audio uses QUIC datagrams (unreliable, no stream overhead, no HoL blocking).
// Wire format:
//   capture_timestamp_us (varint)
//   opus payload (remaining bytes)

/// Encode an audio frame as a datagram payload into a caller-provided buffer.
/// Clears `buf` first, then writes the timestamp varint + payload.
/// Reuse the same `Vec` across calls to avoid per-frame allocations.
pub fn encode_audio_datagram_into(capture_timestamp_us: u64, payload: &[u8], buf: &mut Vec<u8>) {
    buf.clear();
    let (ts, ts_len) = encode_varint(capture_timestamp_us);
    buf.reserve(ts_len + payload.len());
    buf.extend_from_slice(&ts[..ts_len]);
    buf.extend_from_slice(payload);
}

/// Decode an audio datagram payload into a MediaObject.
///
/// Uses `audio_pool` to avoid per-packet heap allocations for the payload.
pub fn decode_audio_datagram(data: &[u8], audio_pool: &BufPool<u8>) -> Result<MediaObject> {
    let (capture_timestamp_us, rest) = decode_varint_from_slice(data)?;
    let mut buf = audio_pool.checkout(rest.len());
    buf.copy_from_slice(rest);
    Ok(MediaObject {
        group_id: 0,
        subgroup_id: 0,
        object_id: 0,
        capture_timestamp_us,
        codec: None,
        payload: buf.share(),
    })
}

/// Encode an audio frame into a pooled `Bytes` ready for `send_datagram`.
///
/// The returned `Bytes` wraps a pooled buffer — zero copy, and when the
/// QUIC stack (and any clones) drop it the buffer returns to the pool.
pub fn encode_audio_datagram(
    capture_timestamp_us: u64,
    payload: &[u8],
    pool: &BufPool<u8>,
) -> Bytes {
    let (ts, ts_len) = encode_varint(capture_timestamp_us);
    let mut buf = pool.checkout_empty();
    buf.push_slice(&ts[..ts_len]);
    buf.push_slice(payload);
    buf.share().into_bytes()
}

// ---------------------------------------------------------------------------
// Video stream helpers
// ---------------------------------------------------------------------------

/// Open a uni stream and write a video subgroup header. Returns the stream
/// for the caller to write objects to.
pub async fn open_video_subgroup(
    conn: &Connection,
    group_id: u64,
    subgroup_id: u64,
    quic_priority: i32,
    publisher_priority: u8,
) -> Result<iroh::endpoint::SendStream> {
    let mut uni = conn.open_uni().await?;
    let _ = uni.set_priority(quic_priority);
    write_subgroup_header(&mut uni, TRACK_VIDEO, group_id, subgroup_id, publisher_priority)
        .await?;
    Ok(uni)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_roundtrip() {
        for val in [0u64, 1, 63, 64, 16383, 16384, 0x3FFF_FFFF, 0x4000_0000] {
            let (buf, len) = encode_varint(val);
            let (decoded, rest) = decode_varint_from_slice(&buf[..len]).unwrap();
            assert_eq!(decoded, val);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn test_audio_datagram_roundtrip() {
        let ts = 1_700_000_000_000_000u64;
        let payload = b"opus data here";
        let mut buf = Vec::new();
        encode_audio_datagram_into(ts, payload, &mut buf);
        let pool = crate::pool::BufPool::new();
        let obj = decode_audio_datagram(&buf, &pool).unwrap();
        assert_eq!(obj.capture_timestamp_us, ts);
        assert_eq!(obj.payload, payload[..]);
    }
}
