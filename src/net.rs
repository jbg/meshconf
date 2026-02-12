use anyhow::{anyhow, Context};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64};
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr};
use tokio_util::sync::CancellationToken;

/// ALPN protocol identifier for vc.
pub const ALPN: &[u8] = b"vc/1";

/// Serialize an EndpointAddr to a shareable ticket string (zstd-compressed, base64-encoded JSON).
fn addr_to_ticket(addr: &EndpointAddr) -> anyhow::Result<String> {
    let json = serde_json::to_vec(addr)?;
    let compressed = zstd::encode_all(json.as_slice(), 3)?;
    Ok(BASE64.encode(&compressed))
}

/// Deserialize an EndpointAddr from a ticket string.
pub fn ticket_to_addr(ticket: &str) -> anyhow::Result<EndpointAddr> {
    let b64 = BASE64.decode(ticket.trim())?;
    let json = zstd::decode_all(b64.as_slice())?;
    let addr: EndpointAddr = serde_json::from_slice(&json)?;
    Ok(addr)
}

/// Create an endpoint that can be used for both hosting and joining.
/// The endpoint is long-lived and supports accepting incoming connections.
pub async fn create_endpoint() -> anyhow::Result<Endpoint> {
    let endpoint = Endpoint::builder()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    endpoint.online().await;
    Ok(endpoint)
}

/// Generate a ticket string from an endpoint's address.
pub fn endpoint_ticket(endpoint: &Endpoint) -> anyhow::Result<String> {
    addr_to_ticket(&endpoint.addr())
}

/// Serialize an EndpointAddr to a ticket string (public wrapper).
pub fn addr_to_ticket_public(addr: &EndpointAddr) -> anyhow::Result<String> {
    addr_to_ticket(addr)
}

/// Connect to a peer given their EndpointAddr.
pub async fn connect_to_peer(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    cancel: CancellationToken,
) -> anyhow::Result<Connection> {
    tokio::select! {
        result = endpoint.connect(addr, ALPN) => {
            result.context("Failed to connect to peer")
        }
        _ = cancel.cancelled() => {
            anyhow::bail!("Cancelled while connecting to peer")
        }
    }
}

/// A host endpoint that can accept multiple connections with a stable ticket.
pub struct HostEndpoint {
    endpoint: Endpoint,
    ticket: String,
}

/// Callback for host status updates during connection setup.
pub enum HostStatus {
    /// Ticket is ready to share — display it to the user.
    Ticket(String),
    /// Waiting for a peer to connect.
    Waiting,
    /// A peer has connected.
    Connected,
}

impl HostEndpoint {
    /// Create a new host endpoint and generate a ticket.
    pub async fn new() -> anyhow::Result<Self> {
        let endpoint = create_endpoint().await?;
        let ticket = endpoint_ticket(&endpoint)?;
        Ok(Self { endpoint, ticket })
    }

    /// The shareable ticket string.
    pub fn ticket(&self) -> &str {
        &self.ticket
    }

    /// Get a reference to the underlying endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Wait for a peer to connect. Can be called multiple times to accept
    /// successive connections on the same endpoint (same ticket).
    pub async fn accept(
        &self,
        cancel: CancellationToken,
    ) -> anyhow::Result<Connection> {
        let conn = tokio::select! {
            incoming = self.endpoint.accept() => {
                incoming
                    .ok_or_else(|| anyhow!("No incoming connection"))?
                    .await
                    .context("Failed to accept connection")?
            }
            _ = cancel.cancelled() => {
                anyhow::bail!("Cancelled while waiting for connection");
            }
        };

        Ok(conn)
    }
}

/// Join mode: connect to an existing host using a ticket string.
/// Calls `on_status` with progress updates.
pub async fn join(
    ticket_str: &str,
    cancel: CancellationToken,
    mut on_status: impl FnMut(&str),
) -> anyhow::Result<Connection> {
    let addr = ticket_to_addr(ticket_str)?;

    let endpoint = Endpoint::builder()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;

    on_status("Connecting to host...");

    let conn = tokio::select! {
        result = endpoint.connect(addr, ALPN) => {
            result.context("Failed to connect to host")?
        }
        _ = cancel.cancelled() => {
            anyhow::bail!("Cancelled while connecting");
        }
    };

    on_status("Connected!");
    Ok(conn)
}
