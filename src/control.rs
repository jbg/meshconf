use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Stream type tag for control streams (bidirectional).
/// Distinguished from MoQ media streams which start with varint 0x4.
pub const STREAM_TYPE_CONTROL: u8 = 0x01;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "msg_type")]
pub enum ControlMessage {
    /// Sent by a joiner to the host upon connecting.
    #[serde(rename = "hello")]
    Hello { addr: String },

    /// Sent by the host to a newly joined peer with the list of existing peers.
    #[serde(rename = "peer_list")]
    PeerList { peers: Vec<PeerInfo> },

    /// Sent by the host to existing peers when a new peer joins.
    #[serde(rename = "peer_joined")]
    PeerJoined { peer: PeerInfo },

    /// Sent by the host to remaining peers when a peer leaves.
    #[serde(rename = "peer_left")]
    PeerLeft { peer_id: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PeerInfo {
    pub peer_id: String,
    pub addr: String,
}

/// Write a length-prefixed JSON control message.
pub async fn write_control_msg(
    w: &mut (impl AsyncWriteExt + Unpin),
    msg: &ControlMessage,
) -> Result<()> {
    let json = serde_json::to_vec(msg)?;
    let len = (json.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(&json).await?;
    Ok(())
}

/// Read a length-prefixed JSON control message.
pub async fn read_control_msg(
    r: &mut (impl AsyncReadExt + Unpin),
) -> Result<ControlMessage> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    anyhow::ensure!(len <= 1_000_000, "Control message too large: {} bytes", len);
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}
