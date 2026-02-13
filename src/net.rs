use anyhow::{anyhow, Context};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64};
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl};
use tokio_util::sync::CancellationToken;

/// ALPN protocol identifier for vc.
pub const ALPN: &[u8] = b"vc/1";


/// Serialize an EndpointAddr to a shareable ticket string (postcard-encoded, base64url).
fn addr_to_ticket(addr: &EndpointAddr) -> anyhow::Result<String> {
    let bytes = postcard::to_stdvec(addr)?;
    Ok(BASE64.encode(&bytes))
}

/// Deserialize an EndpointAddr from a ticket string.
pub fn ticket_to_addr(ticket: &str) -> anyhow::Result<EndpointAddr> {
    let bytes = BASE64.decode(ticket.trim())?;
    let addr: EndpointAddr = postcard::from_bytes(&bytes)?;
    Ok(addr)
}

/// Create an endpoint that can be used for both hosting and joining.
/// The endpoint is long-lived and supports accepting incoming connections.
///
/// If `relay_url` is `Some`, uses that single relay server instead of
/// iroh's default production relays.  Use `--relay http://localhost:3340`
/// with `iroh-relay --dev` for local development.
pub async fn create_endpoint(relay_url: Option<&str>) -> anyhow::Result<Endpoint> {
    let mut builder = Endpoint::builder().alpns(vec![ALPN.to_vec()]);
    if let Some(url) = relay_url {
        let parsed: RelayUrl = url.parse()
            .context("Invalid relay URL")?;
        builder = builder.relay_mode(RelayMode::custom([parsed]));
    }
    let endpoint = builder.bind().await?;
    endpoint.online().await;
    Ok(endpoint)
}

/// Bind an endpoint without waiting for it to come online.
/// The caller can use the endpoint's ID immediately, then call
/// `endpoint.online().await` later to wait for relay connectivity.
pub async fn bind_endpoint(relay_url: Option<&str>, no_relay: bool) -> anyhow::Result<Endpoint> {
    let mut builder = Endpoint::builder().alpns(vec![ALPN.to_vec()]);
    if no_relay {
        builder = builder.relay_mode(RelayMode::Disabled);
    } else if let Some(url) = relay_url {
        let parsed: RelayUrl = url.parse()
            .context("Invalid relay URL")?;
        builder = builder.relay_mode(RelayMode::custom([parsed]));
    }
    let endpoint = builder.bind().await?;
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
    tracing::info!(
        remote_id = ?addr,
        ip_addrs = ?addr.ip_addrs().collect::<Vec<_>>(),
        relay_urls = ?addr.relay_urls().collect::<Vec<_>>(),
        "Connecting to peer"
    );
    tokio::select! {
        result = endpoint.connect(addr, ALPN) => {
            match &result {
                Ok(conn) => tracing::info!(remote_id = %conn.remote_id().fmt_short(), "Connected to peer"),
                Err(e) => tracing::error!("Failed to connect to peer: {e:#}"),
            }
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
        let endpoint = create_endpoint(None).await?;
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
