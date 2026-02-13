use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Stream type tag for control streams (bidirectional).
/// Distinguished from MoQ media streams which start with varint 0x4.
pub const STREAM_TYPE_CONTROL: u8 = 0x01;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "msg_type")]
pub enum ControlMessage {
    /// Sent by the connecting peer upon joining.
    #[serde(rename = "hello")]
    Hello {
        addr: String,
        /// Display name of the peer (typically the OS username).
        #[serde(default)]
        name: String,
    },

    /// Full peer list. Sent in response to Hello, and periodically
    /// thereafter so that all peers converge on the same membership.
    #[serde(rename = "peer_list")]
    PeerList { peers: Vec<PeerInfo> },

    /// Receiver-side congestion feedback, sent periodically to the peer
    /// whose video we are receiving.  Fields are modelled on RTCP Receiver
    /// Reports (RFC 3550 §6.4.1).
    #[serde(rename = "congestion_feedback")]
    CongestionFeedback {
        /// Video frames received in the reporting interval.
        received: u64,
        /// Video frames dropped (late, duplicate, decode failure) in the
        /// reporting interval.
        dropped: u64,
        /// Fraction of frames lost (0.0–1.0), pre-computed for convenience.
        fraction_lost: f64,
        /// Observed inter-arrival jitter in microseconds.
        jitter_us: u64,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PeerInfo {
    pub peer_id: String,
    pub addr: String,
    /// Display name of the peer.
    #[serde(default)]
    pub name: String,
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
