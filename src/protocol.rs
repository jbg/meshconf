use anyhow::{anyhow, Result};
use iroh::endpoint::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// --- MoQ constants ---

/// MoQ STREAM_HEADER_SUBGROUP stream type.
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

// --- QUIC varint encoding (RFC 9000 §16) ---

/// Encode a u64 as a QUIC variable-length integer.
pub fn encode_varint(val: u64) -> Vec<u8> {
    if val < 0x40 {
        vec![val as u8]
    } else if val < 0x4000 {
        let v = (val as u16) | 0x4000;
        v.to_be_bytes().to_vec()
    } else if val < 0x40000000 {
        let v = (val as u32) | 0x80000000;
        v.to_be_bytes().to_vec()
    } else {
        let v = val | 0xC000000000000000;
        v.to_be_bytes().to_vec()
    }
}

/// Write a QUIC varint to an async writer.
pub async fn write_varint(
    w: &mut (impl AsyncWriteExt + Unpin),
    val: u64,
) -> Result<()> {
    let buf = encode_varint(val);
    w.write_all(&buf).await?;
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

// --- MoQ STREAM_HEADER_SUBGROUP framing ---

/// Subgroup header fields (after reading from a stream).
pub struct SubgroupHeader {
    pub track_alias: u64,
    pub group_id: u64,
    pub subgroup_id: u64,
    pub publisher_priority: u8,
}

/// Write a STREAM_HEADER_SUBGROUP header to a stream.
pub async fn write_subgroup_header(
    w: &mut (impl AsyncWriteExt + Unpin),
    track_alias: u64,
    group_id: u64,
    publisher_priority: u8,
) -> Result<()> {
    write_varint(w, STREAM_TYPE_SUBGROUP).await?;
    write_varint(w, track_alias).await?;
    write_varint(w, group_id).await?;
    write_varint(w, 0).await?; // subgroup_id = 0
    w.write_all(&[publisher_priority]).await?;
    Ok(())
}

/// Read a STREAM_HEADER_SUBGROUP header from a stream.
/// Validates that stream_type == 0x4.
pub async fn read_subgroup_header(
    r: &mut (impl AsyncReadExt + Unpin),
) -> Result<SubgroupHeader> {
    let stream_type = read_varint(r).await?;
    if stream_type != STREAM_TYPE_SUBGROUP {
        return Err(anyhow!(
            "Expected STREAM_HEADER_SUBGROUP (0x4), got 0x{:x}",
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

/// Write one object (object_id=0, then length-prefixed payload).
pub async fn write_object(
    w: &mut (impl AsyncWriteExt + Unpin),
    payload: &[u8],
) -> Result<()> {
    write_varint(w, 0).await?; // object_id = 0
    write_varint(w, payload.len() as u64).await?;
    w.write_all(payload).await?;
    Ok(())
}

/// Read one object (object_id + length + payload).
pub async fn read_object(
    r: &mut (impl AsyncReadExt + Unpin),
) -> Result<Vec<u8>> {
    let _object_id = read_varint(r).await?;
    let len = read_varint(r).await?;
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// A received media object with its MoQ group ID preserved.
pub struct MediaObject {
    pub group_id: u64,
    pub payload: Vec<u8>,
}

// --- Convenience: send one media object on a fresh uni stream ---

/// Open a new unidirectional QUIC stream, write a MoQ subgroup header + single object,
/// and finish the stream.
pub async fn send_object(
    conn: &Connection,
    track_alias: u64,
    group_id: u64,
    quic_priority: i32,
    publisher_priority: u8,
    payload: &[u8],
) -> Result<()> {
    let mut uni = conn.open_uni().await?;
    let _ = uni.set_priority(quic_priority);
    write_subgroup_header(&mut uni, track_alias, group_id, publisher_priority).await?;
    write_object(&mut uni, payload).await?;
    uni.finish()?;
    Ok(())
}
