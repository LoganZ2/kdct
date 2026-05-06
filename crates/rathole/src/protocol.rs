pub const HASH_WIDTH_IN_BYTES: usize = 32;

use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::trace;

type ProtocolVersion = u8;
const _PROTO_V0: u8 = 0u8;
const PROTO_V1: u8 = 1u8;

pub const CURRENT_PROTO_VERSION: ProtocolVersion = PROTO_V1;

pub type Digest = [u8; HASH_WIDTH_IN_BYTES];

// ── Pipeline types ──────────────────────────────────────────────

/// Unique identifier for a pipeline run
pub type PipelineId = String;

/// A single step in a pipeline
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct PipelineStep {
    /// Human-readable step name (e.g., "install", "build")
    pub name: String,
    /// Shell command to execute
    pub command: String,
    /// Working directory (defaults to current dir if None)
    pub cwd: Option<String>,
    /// Timeout in seconds (0 = no timeout)
    pub timeout_secs: u64,
}

// ── Wire message types ──────────────────────────────────────────

#[derive(Deserialize, Serialize, Debug)]
pub enum Hello {
    ControlChannelHello(ProtocolVersion, Digest), // sha256sum(service name) or a nonce
    DataChannelHello(ProtocolVersion, Digest),    // token provided by CreateDataChannel
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Auth(pub Digest);

#[derive(Deserialize, Serialize, Debug)]
pub enum Ack {
    Ok,
    ServiceNotExist,
    AuthFailed,
}

impl std::fmt::Display for Ack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Ack::Ok => "Ok",
                Ack::ServiceNotExist => "Service not exist",
                Ack::AuthFailed => "Incorrect token",
            }
        )
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub enum ControlChannelCmd {
    CreateDataChannel,
    HeartBeat,
    /// Server → Client: Execute a pipeline
    RunPipeline {
        id: PipelineId,
        steps: Vec<PipelineStep>,
    },
    /// Server → Client: Cancel a running pipeline
    CancelPipeline {
        id: PipelineId,
    },
    /// Client → Server: Streaming output from a pipeline step
    PipelineOutput {
        id: PipelineId,
        step: String,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit_code: Option<i32>,
    },
    /// Server → Client: Port assignments from the pool
    PortsAssigned {
        /// (local_port, server_port) pairs
        mappings: Vec<(u16, u16)>,
    },
    /// Client → Server: Client info + available ports reported on connect
    ReportStatus {
        hostname: String,
        os: String,
        arch: String,
        /// Available local ports, e.g. ["3000-3005", "8080"]
        ports: Vec<String>,
    },
}

#[derive(Deserialize, Serialize, Debug)]
pub enum DataChannelCmd {
    /// Start TCP forwarding.
    /// None → use config's local_addr. Some(port) → connect to localhost:port.
    StartForwardTcp(Option<u16>),
    StartForwardUdp,
    /// Start HTTP forwarding with optional host/path routing
    StartForwardHttp {
        path_prefix: Option<String>,
        host: Option<String>,
    },
}

// ── UDP traffic (unchanged wire format) ─────────────────────────

type UdpPacketLen = u16; // `u16` should be enough for any practical UDP traffic on the Internet
#[derive(Deserialize, Serialize, Debug)]
struct UdpHeader {
    from: SocketAddr,
    len: UdpPacketLen,
}

#[derive(Debug)]
pub struct UdpTraffic {
    pub from: SocketAddr,
    pub data: Bytes,
}

impl UdpTraffic {
    pub async fn write<T: AsyncWrite + Unpin>(&self, writer: &mut T) -> Result<()> {
        let hdr = UdpHeader {
            from: self.from,
            len: self.data.len() as UdpPacketLen,
        };

        let v = bincode::serialize(&hdr).unwrap();

        trace!("Write {:?} of length {}", hdr, v.len());
        writer.write_u8(v.len() as u8).await?;
        writer.write_all(&v).await?;

        writer.write_all(&self.data).await?;

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn write_slice<T: AsyncWrite + Unpin>(
        writer: &mut T,
        from: SocketAddr,
        data: &[u8],
    ) -> Result<()> {
        let hdr = UdpHeader {
            from,
            len: data.len() as UdpPacketLen,
        };

        let v = bincode::serialize(&hdr).unwrap();

        trace!("Write {:?} of length {}", hdr, v.len());
        writer.write_u8(v.len() as u8).await?;
        writer.write_all(&v).await?;

        writer.write_all(data).await?;

        Ok(())
    }

    pub async fn read<T: AsyncRead + Unpin>(reader: &mut T, hdr_len: u8) -> Result<UdpTraffic> {
        let mut buf = vec![0; hdr_len as usize];
        reader
            .read_exact(&mut buf)
            .await
            .with_context(|| "Failed to read udp header")?;

        let hdr: UdpHeader =
            bincode::deserialize(&buf).with_context(|| "Failed to deserialize UdpHeader")?;

        trace!("hdr {:?}", hdr);

        let mut data = BytesMut::new();
        data.resize(hdr.len as usize, 0);
        reader.read_exact(&mut data).await?;

        Ok(UdpTraffic {
            from: hdr.from,
            data: data.freeze(),
        })
    }
}

// ── Hashing ─────────────────────────────────────────────────────

pub fn digest(data: &[u8]) -> Digest {
    use sha2::{Digest, Sha256};
    let d = Sha256::new().chain_update(data).finalize();
    d.into()
}

// ── Length-prefixed read helpers ────────────────────────────────

async fn read_len_prefixed<T: AsyncRead + Unpin>(conn: &mut T) -> Result<Vec<u8>> {
    let len = conn.read_u16().await.context("Failed to read length prefix")? as usize;
    let mut buf = vec![0u8; len];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| format!("Failed to read {} bytes", len))?;
    Ok(buf)
}

pub async fn read_hello<T: AsyncRead + Unpin>(conn: &mut T) -> Result<Hello> {
    let buf = read_len_prefixed(conn).await?;
    let hello: Hello =
        bincode::deserialize(&buf).with_context(|| "Failed to deserialize hello")?;

    match hello {
        Hello::ControlChannelHello(v, _) | Hello::DataChannelHello(v, _) => {
            if v != CURRENT_PROTO_VERSION {
                bail!(
                    "Protocol version mismatched. Expected {}, got {}. Please update `rathole`.",
                    CURRENT_PROTO_VERSION,
                    v
                );
            }
        }
    }

    Ok(hello)
}

pub async fn read_auth<T: AsyncRead + Unpin>(conn: &mut T) -> Result<Auth> {
    let buf = read_len_prefixed(conn).await?;
    bincode::deserialize(&buf).with_context(|| "Failed to deserialize auth")
}

pub async fn read_ack<T: AsyncRead + Unpin>(conn: &mut T) -> Result<Ack> {
    let buf = read_len_prefixed(conn).await?;
    bincode::deserialize(&buf).with_context(|| "Failed to deserialize ack")
}

pub async fn read_control_cmd<T: AsyncRead + Unpin>(
    conn: &mut T,
) -> Result<ControlChannelCmd> {
    let buf = read_len_prefixed(conn).await?;
    bincode::deserialize(&buf).with_context(|| "Failed to deserialize control cmd")
}

pub async fn read_data_cmd<T: AsyncRead + Unpin>(
    conn: &mut T,
) -> Result<DataChannelCmd> {
    let buf = read_len_prefixed(conn).await?;
    bincode::deserialize(&buf).with_context(|| "Failed to deserialize data cmd")
}

// ── Length-prefixed write helpers ───────────────────────────────

async fn write_len_prefixed<T: AsyncWrite + Unpin>(conn: &mut T, payload: &[u8]) -> Result<()> {
    conn.write_u16(payload.len() as u16).await?;
    conn.write_all(payload).await?;
    conn.flush().await?;
    Ok(())
}

pub async fn write_hello<T: AsyncWrite + Unpin>(conn: &mut T, hello: &Hello) -> Result<()> {
    let payload = bincode::serialize(hello).context("Failed to serialize hello")?;
    write_len_prefixed(conn, &payload).await
}

pub async fn write_auth<T: AsyncWrite + Unpin>(conn: &mut T, auth: &Auth) -> Result<()> {
    let payload = bincode::serialize(auth).context("Failed to serialize auth")?;
    write_len_prefixed(conn, &payload).await
}

pub async fn write_ack<T: AsyncWrite + Unpin>(conn: &mut T, ack: &Ack) -> Result<()> {
    let payload = bincode::serialize(ack).context("Failed to serialize ack")?;
    write_len_prefixed(conn, &payload).await
}

pub async fn write_control_cmd<T: AsyncWrite + Unpin>(
    conn: &mut T,
    cmd: &ControlChannelCmd,
) -> Result<()> {
    let payload = bincode::serialize(cmd).context("Failed to serialize control cmd")?;
    write_len_prefixed(conn, &payload).await
}

pub async fn write_data_cmd<T: AsyncWrite + Unpin>(
    conn: &mut T,
    cmd: &DataChannelCmd,
) -> Result<()> {
    let payload = bincode::serialize(cmd).context("Failed to serialize data cmd")?;
    write_len_prefixed(conn, &payload).await
}
