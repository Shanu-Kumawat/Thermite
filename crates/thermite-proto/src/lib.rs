//! # thermite-proto
//!
//! Thermite's IPC protocol: length-prefixed JSON frames over a Unix domain
//! socket, plus the blocking client used by the CLI and GUI.
//!
//! Wire format: `u32` little-endian payload length, then a UTF-8 JSON
//! [`Frame`]. Requests carry a caller-chosen `id`; responses match it; events
//! arrive unsolicited on connections that sent [`Request::Subscribe`].
//!
//! The protocol is versioned via the [`Request::Hello`] handshake — clients
//! refuse mismatched servers rather than guessing at semantics.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thermite_core::{
    CapabilityReport, Config, CurvePoint, FailsafePhase, FanConfig, FanDescriptor, FanId, Telemetry,
};
use thiserror::Error;

/// Protocol version; bump on any incompatible wire change.
pub const PROTOCOL_VERSION: u32 = 2;
/// Default daemon socket path (owned by root, group-readable).
pub const DEFAULT_SOCKET_PATH: &str = "/run/thermite/thermite.sock";
/// Largest frame accepted; telemetry frames are ~1-2 KiB, configs a few KiB.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Requests a client may send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    rename_all = "kebab-case",
    tag = "type",
    rename_all_fields = "kebab-case"
)]
pub enum Request {
    /// Handshake; must be the first frame on a connection.
    Hello {
        /// Protocol version the client speaks.
        protocol_version: u32,
        /// Human-readable client identification (`thermite-cli 0.1.0`).
        client: String,
    },
    /// Ask the server to push [`Event::Tick`] frames to this connection.
    Subscribe,
    /// Full status snapshot (telemetry + control state + daemon info).
    GetStatus,
    /// The daemon's active configuration.
    GetConfig,
    /// Change one fan's control mode.
    SetFanConfig {
        /// Which fan.
        fan: FanId,
        /// New settings (validated and persisted by the daemon).
        config: FanConfig,
    },
    /// Apply a platform profile by sysfs name (e.g. `"quiet"`).
    SetPlatformProfile {
        /// Profile name as listed in [`CapabilityReport::platform_profiles`].
        profile: String,
    },
    /// Create or replace a named curve (validated; persists to config).
    PutCurve {
        /// Curve name.
        name: String,
        /// Points of the curve.
        points: Vec<CurvePoint>,
    },
    /// Delete a named curve (fails if a fan is using it).
    DeleteCurve {
        /// Curve name.
        name: String,
    },
    /// Liveness check.
    Ping,
}

/// Server responses, matched to requests by frame id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    rename_all = "kebab-case",
    tag = "type",
    rename_all_fields = "kebab-case"
)]
pub enum Response {
    /// Handshake reply; must be the first response on a connection.
    Hello {
        /// Protocol version the server speaks.
        protocol_version: u32,
        /// Server build version.
        server_version: String,
        /// Daemon PID.
        pid: u32,
        /// Hardware capability summary.
        capability: CapabilityReport,
    },
    /// Answer to [`Request::GetStatus`].
    Status {
        /// The snapshot.
        status: StatusSnapshot,
    },
    /// Answer to [`Request::GetConfig`].
    Config {
        /// The configuration.
        config: Config,
    },
    /// Generic success for mutating requests.
    Ok,
    /// Request failed (validation, permissions, hardware).
    Error {
        /// What went wrong.
        message: String,
    },
    /// Answer to [`Request::Ping`].
    Pong,
}

/// Per-fan control state as the daemon sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FanRuntimeState {
    /// Which fan.
    pub id: FanId,
    /// Configured mode (as set by config file or IPC).
    pub config: FanConfig,
    /// Curve actually in effect (battery-suffixed variant when applied),
    /// when the fan is curve-driven.
    pub resolved_curve: Option<String>,
    /// Last duty percent commanded this session.
    pub commanded_duty: Option<u8>,
    /// Last hardware write error for this fan, if any.
    pub last_write_error: Option<String>,
}

/// Control-loop state published with every tick and in status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ControlSnapshot {
    /// Fans the driver reports (labels + temp sources).
    pub fan_info: Vec<FanDescriptor>,
    /// Per-fan runtime state.
    pub fans: Vec<FanRuntimeState>,
    /// Current failsafe phase.
    pub failsafe_phase: FailsafePhase,
    /// True while the daemon is in the post-resume grace window and not
    /// writing to hardware.
    pub suspended_grace: bool,
}

/// Daemon metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// Server build version.
    pub version: String,
    /// Daemon PID.
    pub pid: u32,
    /// Milliseconds since daemon start.
    pub uptime_ms: u64,
    /// True when the daemon logs writes but does not perform them.
    pub dry_run: bool,
    /// Capabilities detected on this machine.
    pub capability: CapabilityReport,
}

/// Everything a client needs to render the main UI in one shot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    /// Latest sensor/fan telemetry.
    pub telemetry: Telemetry,
    /// Control-loop state.
    pub control: ControlSnapshot,
    /// Daemon metadata.
    pub daemon: DaemonInfo,
    /// Most recent alerts (newest last, bounded).
    #[serde(default)]
    pub recent_alerts: Vec<Alert>,
}

/// Alert severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlertLevel {
    /// Worth noticing.
    Warning,
    /// Safety-relevant.
    Critical,
}

/// Machine-readable alert categories (drives GUI badging and CLI filters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlertKind {
    /// Warn threshold crossed; full duty forced.
    FailsafeFull,
    /// Crit threshold crossed; control returned to firmware.
    FailsafeReleased,
    /// Recovered from a failsafe latch.
    FailsafeRecovered,
    /// Someone else (or the firmware) changed pwm state under us.
    ExternalOverride,
    /// State re-applied after suspend/resume.
    ResumeReapply,
    /// Firmware rejected a profile (e.g. turbo on ANV15-51).
    ProfileRejected,
    /// Configuration was reloaded.
    ConfigReloaded,
    /// Configuration failed validation; previous config kept.
    ConfigRejected,
}

/// An asynchronous notification pushed to subscribers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// Severity.
    pub level: AlertLevel,
    /// Category.
    pub kind: AlertKind,
    /// Human-readable message.
    pub message: String,
    /// Daemon tick at which the alert fired.
    pub tick_ms: u64,
}

/// Server-initiated frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    rename_all = "kebab-case",
    tag = "type",
    rename_all_fields = "kebab-case"
)]
pub enum Event {
    /// One control tick: telemetry plus the control decisions taken.
    Tick {
        /// Sensor/fan readings.
        telemetry: Telemetry,
        /// Control state for this tick.
        control: ControlSnapshot,
    },
    /// Failsafe/operational alert.
    Alert(Alert),
    /// Config changed (SIGHUP reload or edit by another client).
    ConfigChanged {
        /// The new configuration.
        config: Config,
    },
}

/// One wire message. Externally tagged (default) so the newtype `Event`
/// variant wraps its internally-tagged payload directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    /// Client request.
    Request {
        /// Caller-assigned correlation id.
        id: u64,
        /// The request.
        request: Request,
    },
    /// Server response.
    Response {
        /// Matches the request id.
        id: u64,
        /// Payload or RPC-level error.
        result: Result<Response, String>,
    },
    /// Server push (subscribed connections only).
    Event(Event),
}

/// Codec and transport failures.
#[derive(Debug, Error)]
pub enum IpcError {
    /// Socket I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Peer closed the connection.
    #[error("connection closed")]
    Closed,
    /// Frame length prefix exceeds [`MAX_FRAME_BYTES`].
    #[error("frame of {0} bytes exceeds maximum")]
    TooLarge(usize),
    /// Malformed frame payload.
    #[error("malformed frame: {0}")]
    Decode(String),
    /// Server answered a request with an error response.
    #[error("request failed: {0}")]
    Rpc(String),
    /// Peer violated the protocol (unexpected frame).
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// Handshake failed (bad version or shape).
    #[error("handshake failed: {0}")]
    Handshake(String),
    /// Could not connect.
    #[error("connect: {0}")]
    Connect(String),
}

/// Encode a frame to bytes (length prefix + JSON payload).
pub fn encode(frame: &Frame) -> Result<Vec<u8>, IpcError> {
    let payload = serde_json::to_vec(frame).map_err(|e| IpcError::Decode(e.to_string()))?;
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Write one frame (length-prefixed) and flush.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> Result<(), IpcError> {
    w.write_all(&encode(frame)?)?;
    w.flush().map_err(IpcError::Io)
}

/// Read one length-prefixed frame. Works with both plain readers and
/// `BufRead`ers via the `Read` bound; use a `BufReader` in practice.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Frame, IpcError> {
    let mut len_buf = [0u8; 4];
    read_exact_or_closed(r, &mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(IpcError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    read_exact_or_closed(r, &mut payload)?;
    serde_json::from_slice(&payload).map_err(|e| IpcError::Decode(e.to_string()))
}

fn read_exact_or_closed<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), IpcError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(IpcError::Closed),
        Err(e) => Err(e.into()),
    }
}

/// Blocking IPC client. Single-threaded by design: responses are matched by
/// id while events are buffered for later retrieval — see [`Client::call`]
/// and [`Client::read_event`].
#[derive(Debug)]
pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    pending_events: VecDeque<Event>,
    server_version: String,
}

impl Client {
    /// Connect to the daemon socket and perform the version handshake.
    pub fn connect(path: &Path, client_name: &str) -> Result<Self, IpcError> {
        let stream = UnixStream::connect(path).map_err(|e| IpcError::Connect(e.to_string()))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        let writer = stream.try_clone()?;
        let reader = BufReader::new(stream);
        let mut client = Self {
            writer,
            reader,
            next_id: 1,
            pending_events: VecDeque::new(),
            server_version: String::new(),
        };

        let mut response = client.call(Request::Hello {
            protocol_version: PROTOCOL_VERSION,
            client: client_name.to_string(),
        })?;
        let Response::Hello {
            protocol_version,
            server_version,
            ..
        } = &mut response
        else {
            return Err(IpcError::Handshake(format!(
                "expected Hello response, got {response:?}"
            )));
        };
        if *protocol_version != PROTOCOL_VERSION {
            return Err(IpcError::Handshake(format!(
                "protocol mismatch: client {PROTOCOL_VERSION}, server {protocol_version}"
            )));
        }
        client.server_version = server_version.clone();
        Ok(client)
    }

    /// Server version reported during the handshake.
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// Send a request and block for its response. Events that arrive while
    /// waiting are buffered (drain with [`Client::take_events`]).
    pub fn call(&mut self, request: Request) -> Result<Response, IpcError> {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.writer, &Frame::Request { id, request })?;
        loop {
            match read_frame(&mut self.reader)? {
                Frame::Event(event) => self.pending_events.push_back(event),
                Frame::Response { id: rid, result } => {
                    if rid != id {
                        return Err(IpcError::Protocol(format!(
                            "response id {rid} does not match request id {id}"
                        )));
                    }
                    return match result {
                        Ok(resp) => Ok(resp),
                        Err(message) => Err(IpcError::Rpc(message)),
                    };
                }
                Frame::Request { .. } => {
                    return Err(IpcError::Protocol("server sent a request".to_string()));
                }
            }
        }
    }

    /// Turn on event push for this connection and switch to blocking reads
    /// (no read timeout). Drain buffered events first via
    /// [`Client::take_events`].
    pub fn subscribe(&mut self) -> Result<(), IpcError> {
        self.call(Request::Subscribe)?;
        self.writer.set_read_timeout(None)?;
        Ok(())
    }

    /// Block until the next event frame arrives (or the connection closes).
    /// Buffered events are returned first.
    pub fn read_event(&mut self) -> Result<Event, IpcError> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(event);
        }
        match read_frame(&mut self.reader)? {
            Frame::Event(event) => Ok(event),
            Frame::Response { .. } => Err(IpcError::Protocol(
                "unexpected response while reading events".to_string(),
            )),
            Frame::Request { .. } => Err(IpcError::Protocol("server sent a request".to_string())),
        }
    }

    /// Drain events buffered during [`Client::call`] round-trips.
    pub fn take_events(&mut self) -> Vec<Event> {
        self.pending_events.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn sample_frame() -> Frame {
        Frame::Request {
            id: 7,
            request: Request::SetFanConfig {
                fan: FanId::new("cpu"),
                config: FanConfig {
                    mode: thermite_core::config::FanModeKind::Curve,
                    curve: Some("balanced".to_string()),
                    manual_duty_pct: None,
                },
            },
        }
    }

    #[test]
    fn frame_roundtrip_through_bytes() {
        let frame = sample_frame();
        let bytes = encode(&frame).unwrap();
        // Length prefix correctness
        let len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4);
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(read_frame(&mut cursor).unwrap(), frame);
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut bytes = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes().to_vec();
        bytes.extend_from_slice(b"xxxx");
        let mut cursor = std::io::Cursor::new(bytes);
        match read_frame(&mut cursor) {
            Err(IpcError::TooLarge(n)) => assert_eq!(n, MAX_FRAME_BYTES + 1),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn truncated_frame_reports_closed() {
        let bytes = encode(&sample_frame()).unwrap();
        let mut cursor = std::io::Cursor::new(&bytes[..bytes.len() - 5]);
        match read_frame(&mut cursor) {
            Err(IpcError::Closed) => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn client_handshake_and_call_over_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            // Handshake
            let Frame::Request { id, .. } = read_frame(&mut reader).unwrap() else {
                panic!("expected hello request");
            };
            write_frame(
                &mut writer,
                &Frame::Response {
                    id,
                    result: Ok(Response::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        server_version: "test".to_string(),
                        pid: 42,
                        capability: CapabilityReport::default(),
                    }),
                },
            )
            .unwrap();
            // Second request: answer Ok, then push an event.
            let Frame::Request { id, .. } = read_frame(&mut reader).unwrap() else {
                panic!("expected request");
            };
            write_frame(
                &mut writer,
                &Frame::Response {
                    id,
                    result: Ok(Response::Pong),
                },
            )
            .unwrap();
            write_frame(
                &mut writer,
                &Frame::Event(Event::Alert(Alert {
                    level: AlertLevel::Warning,
                    kind: AlertKind::ExternalOverride,
                    message: "test".to_string(),
                    tick_ms: 5,
                })),
            )
            .unwrap();
        });

        let mut client = Client::connect(&sock, "test-client").unwrap();
        assert_eq!(client.server_version(), "test");
        assert!(matches!(
            client.call(Request::Ping).unwrap(),
            Response::Pong
        ));
        // The server pushed an event during/after the call; it should surface
        // via read_event/take_events.
        let event = client
            .take_events()
            .first()
            .cloned()
            .or_else(|| client.read_event().ok());
        assert!(matches!(event, Some(Event::Alert(_))), "got {event:?}");
    }

    #[test]
    fn client_rejects_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let Frame::Request { id, .. } = read_frame(&mut reader).unwrap() else {
                panic!()
            };
            write_frame(
                &mut writer,
                &Frame::Response {
                    id,
                    result: Ok(Response::Hello {
                        protocol_version: 99,
                        server_version: "test".to_string(),
                        pid: 42,
                        capability: CapabilityReport::default(),
                    }),
                },
            )
            .unwrap();
        });
        let err = Client::connect(&sock, "test-client").unwrap_err();
        assert!(err.to_string().contains("protocol mismatch"), "{err}");
    }
}
