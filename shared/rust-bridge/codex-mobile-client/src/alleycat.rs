use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use codex_app_server_client::{
    AppServerClient, RemoteAppServerClient, RemoteAppServerConnectArgs, RemoteAppServerEndpoint,
};
use iroh::endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream, VarInt};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tracing::{debug, info, warn};

use crate::session::remote_transport::{Reconnected, RemoteTransport, SessionKeepalive};
use crate::transport::TransportError;
use crate::types::AgentRuntimeKind as AgentRuntimeKindId;

/// Typed enum form of the canonical runtime kinds litter knows about,
/// including the `AlleycatAgentRuntimeKind::Pi` arm used to gate Pi
/// runtime behavior across mobile (kotlin
/// `AlleycatAgentRuntimeKind.Pi` / `PI`). The stringly-typed
/// [`crate::types::AgentRuntimeKind`] remains the public boundary type
/// used across UniFFI (so new alleycat-advertised agents work without
/// a litter release), but features that need typed pattern-matching —
/// e.g. wiring an in-process pi runtime — go through this enum.
///
/// The UniFFI export name is `AlleycatAgentRuntimeKind` to avoid a
/// symbol clash with the iOS Swift-side `AgentRuntimeKind` typealias
/// (and to keep the Android Kotlin reference unambiguous as well).
///
/// `serde` round-trips through the canonical lowercase id (`"codex"`,
/// `"pi"`, `"claude"`, …), matching the stringly-typed boundary value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
pub enum AlleycatAgentRuntimeKind {
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "pi")]
    Pi,
    #[serde(rename = "amp")]
    Amp,
    #[serde(rename = "opencode")]
    Opencode,
    #[serde(rename = "claude")]
    Claude,
    #[serde(rename = "droid")]
    Droid,
    #[serde(rename = "hermes")]
    Hermes,
}

impl AlleycatAgentRuntimeKind {
    /// Stable lowercase id (matches the `serde` rename) used for the
    /// stringly-typed UniFFI boundary value.
    pub fn as_id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Amp => "amp",
            Self::Opencode => "opencode",
            Self::Claude => "claude",
            Self::Droid => "droid",
            Self::Hermes => "hermes",
        }
    }

    /// Convert to the stringly-typed boundary id.
    pub fn into_id(self) -> AgentRuntimeKindId {
        self.as_id().to_owned()
    }
}

pub const ALLEYCAT_PROTOCOL_VERSION: u32 = 1;
pub const ALLEYCAT_ALPN: &[u8] = b"alleycat/1";
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Litter-side cached manifest derived from an alleycat host's
/// `list_agents` response after a successful pair. Persisted to disk
/// by `pi-server-runner --alleycat-pair`, consumed by mobile platform
/// code (and by the alleycat-path validator) to decide which agent
/// runtime + capability flags apply to each advertised host entry.
///
/// This is intentionally distinct from the wire-format
/// [`AgentInfo`] shape: it carries litter's typed runtime kind
/// (`agent_runtime_kind`) and the small, behavior-gating capability
/// set the rest of the app actually branches on (`voice`, `plans`,
/// `tool_exec`, `uses_direct_codex_port`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LitterAlleycatManifest {
    /// Schema version of the manifest envelope; bump when the wire
    /// shape changes.
    pub version: u32,
    /// Iroh endpoint id of the paired alleycat host.
    pub host_id: String,
    /// Optional human-friendly host name from the pair payload (e.g.
    /// `studio.local`). Useful for UI labels.
    pub host_name: Option<String>,
    /// One entry per advertised agent on this host. Pi specifically
    /// is what `remote-pi-alleycat-advertisement` cares about, but the
    /// shape is generic so other runtimes (Codex, Amp, Claude, …) can
    /// land alongside it without further plumbing.
    pub hosts: Vec<LitterAlleycatManifestEntry>,
}

/// One row in the cached litter alleycat manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LitterAlleycatManifestEntry {
    /// Raw alleycat agent name, e.g. `"pi"`, `"codex"`.
    pub agent_name: String,
    /// Human-friendly display name from the host (`"Pi"`, `"Codex"`).
    pub agent_display_name: String,
    /// Typed runtime kind. Pi entries always serialize as `"Pi"` so
    /// validators can `jq '.hosts[] | select(.agent_runtime_kind=="Pi")'`.
    pub agent_runtime_kind: LitterManifestRuntimeKind,
    /// Mirrors the parsed pair payload's host id so each manifest row
    /// is self-contained.
    pub host_id: String,
    /// Mirrors the parsed pair payload's optional host name.
    pub host_name: Option<String>,
    /// Wire format the agent advertised (websocket vs jsonl).
    pub wire: LitterManifestWire,
    /// Whether the host reported the agent as ready to accept turns.
    pub available: bool,
    /// Behavior-gating flags used by the mobile UI + runner to decide
    /// whether to surface voice/plans/tool exec controls and whether
    /// to route via the direct codex port. The four flags are the
    /// canonical capability set defined by VAL-REM-009.
    pub capabilities: LitterManifestCapabilities,
}

/// Wire format on the litter manifest side. Distinct from
/// [`AgentWireWire`] (the alleycat-wire decode shape) so we can
/// stabilise the manifest JSON without coupling to upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LitterManifestWire {
    Websocket,
    Jsonl,
}

impl From<AgentWire> for LitterManifestWire {
    fn from(value: AgentWire) -> Self {
        match value {
            AgentWire::Websocket => LitterManifestWire::Websocket,
            AgentWire::Jsonl => LitterManifestWire::Jsonl,
        }
    }
}

/// Behavior-gating capabilities recorded in the litter manifest. Per
/// VAL-REM-009 a Pi entry must satisfy `voice = false`, `plans = false`,
/// `tool_exec = true`, `uses_direct_codex_port = false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LitterManifestCapabilities {
    pub voice: bool,
    pub plans: bool,
    pub tool_exec: bool,
    pub uses_direct_codex_port: bool,
}

impl LitterManifestCapabilities {
    /// Default capability set for a given runtime kind. Used when an
    /// alleycat host does not advertise rich `AgentCapabilities` of
    /// its own. Pi specifically is the case `remote-pi-alleycat-
    /// advertisement` cares about.
    pub fn defaults_for(kind: LitterManifestRuntimeKind) -> Self {
        match kind {
            LitterManifestRuntimeKind::Pi => Self {
                voice: false,
                plans: false,
                tool_exec: true,
                uses_direct_codex_port: false,
            },
            LitterManifestRuntimeKind::Codex => Self {
                voice: true,
                plans: true,
                tool_exec: true,
                uses_direct_codex_port: true,
            },
            LitterManifestRuntimeKind::Amp
            | LitterManifestRuntimeKind::Opencode
            | LitterManifestRuntimeKind::Claude
            | LitterManifestRuntimeKind::Droid
            | LitterManifestRuntimeKind::Hermes
            | LitterManifestRuntimeKind::Other => Self {
                voice: false,
                plans: false,
                tool_exec: true,
                uses_direct_codex_port: false,
            },
        }
    }
}

/// Manifest-side runtime kind. Serialised with capitalised variant
/// names (`"Pi"`, `"Codex"`, …) so validators can `jq` on a stable
/// human-readable label that matches the Rust enum variant.
///
/// Distinct from [`AlleycatAgentRuntimeKind`], which uses lowercase
/// canonical ids on the wire (`"pi"`) — that one stays for cross-
/// platform UniFFI signalling; this one is purely for the cached
/// manifest payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LitterManifestRuntimeKind {
    Codex,
    Pi,
    Amp,
    Opencode,
    Claude,
    Droid,
    Hermes,
    Other,
}

impl From<AlleycatAgentRuntimeKind> for LitterManifestRuntimeKind {
    fn from(kind: AlleycatAgentRuntimeKind) -> Self {
        match kind {
            AlleycatAgentRuntimeKind::Codex => LitterManifestRuntimeKind::Codex,
            AlleycatAgentRuntimeKind::Pi => LitterManifestRuntimeKind::Pi,
            AlleycatAgentRuntimeKind::Amp => LitterManifestRuntimeKind::Amp,
            AlleycatAgentRuntimeKind::Opencode => LitterManifestRuntimeKind::Opencode,
            AlleycatAgentRuntimeKind::Claude => LitterManifestRuntimeKind::Claude,
            AlleycatAgentRuntimeKind::Droid => LitterManifestRuntimeKind::Droid,
            AlleycatAgentRuntimeKind::Hermes => LitterManifestRuntimeKind::Hermes,
        }
    }
}

impl LitterManifestRuntimeKind {
    /// Map a free-form agent name/display-name pair to a manifest
    /// runtime kind, going through the same canonicalisation rules
    /// as [`agent_runtime_kind`]. Unknown agents collapse to
    /// `Other` so the manifest stays self-describing.
    pub fn from_agent(name: &str, display_name: &str) -> Self {
        match agent_runtime_kind(name, display_name).as_deref() {
            Some("codex") => LitterManifestRuntimeKind::Codex,
            Some("pi") => LitterManifestRuntimeKind::Pi,
            Some("amp") => LitterManifestRuntimeKind::Amp,
            Some("opencode") => LitterManifestRuntimeKind::Opencode,
            Some("claude") => LitterManifestRuntimeKind::Claude,
            Some("droid") => LitterManifestRuntimeKind::Droid,
            Some("hermes") => LitterManifestRuntimeKind::Hermes,
            _ => LitterManifestRuntimeKind::Other,
        }
    }
}

/// Schema version baked into every new [`LitterAlleycatManifest`].
pub const LITTER_ALLEYCAT_MANIFEST_VERSION: u32 = 1;

/// Build the litter-side cached manifest from a parsed pair payload
/// and the agent list returned by [`list_agents`]. Manifest-side
/// capability flags fall back to runtime-specific defaults when the
/// alleycat host does not advertise its own rich [`AgentCapabilities`].
/// Pi entries always end up with the VAL-REM-009 flag set
/// (`voice=false, plans=false, tool_exec=true,
/// uses_direct_codex_port=false`).
pub fn build_litter_manifest(
    params: &ParsedPairPayload,
    agents: &[AgentInfo],
) -> LitterAlleycatManifest {
    let hosts = agents
        .iter()
        .map(|agent| {
            let kind = LitterManifestRuntimeKind::from_agent(&agent.name, &agent.display_name);
            // Pi specifically MUST use the canonical default flag set
            // regardless of what the alleycat host advertises, since
            // VAL-REM-009 pins the four-flag shape verbatim. Other
            // runtimes can take advantage of any richer capability
            // metadata when the host advertises it.
            let capabilities = if matches!(kind, LitterManifestRuntimeKind::Pi) {
                LitterManifestCapabilities::defaults_for(kind)
            } else if let Some(caps) = agent.capabilities.as_ref() {
                LitterManifestCapabilities {
                    voice: false,
                    plans: false,
                    tool_exec: true,
                    uses_direct_codex_port: caps.uses_direct_codex_port,
                }
            } else {
                LitterManifestCapabilities::defaults_for(kind)
            };
            LitterAlleycatManifestEntry {
                agent_name: agent.name.clone(),
                agent_display_name: agent.display_name.clone(),
                agent_runtime_kind: kind,
                host_id: params.node_id.clone(),
                host_name: params.host_name.clone(),
                wire: LitterManifestWire::from(agent.wire),
                available: agent.available,
                capabilities,
            }
        })
        .collect();
    LitterAlleycatManifest {
        version: LITTER_ALLEYCAT_MANIFEST_VERSION,
        host_id: params.node_id.clone(),
        host_name: params.host_name.clone(),
        hosts,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPairPayload {
    pub version: u32,
    pub node_id: String,
    pub token: String,
    pub relay: Option<String>,
    pub host_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    pub name: String,
    pub display_name: String,
    pub wire: AgentWire,
    pub available: bool,
    /// UI-facing hints from the alleycat host (label/sort/beta/aliases).
    /// `None` means the host is older or doesn't ship rich metadata; the
    /// client falls back to generic rendering.
    pub presentation: Option<AgentPresentation>,
    /// Behavioral capability flags that gate UI logic (Amp reasoning lock,
    /// SSH-bridge eligibility, direct-Codex-port routing) without litter
    /// branching on the agent name.
    pub capabilities: Option<AgentCapabilities>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentPresentation {
    pub title: Option<String>,
    pub is_beta: bool,
    pub sort_order: i32,
    pub description: Option<String>,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub locks_reasoning_effort_after_activity: bool,
    pub visible_modes: Option<Vec<String>>,
    pub supports_ssh_bridge: bool,
    pub uses_direct_codex_port: bool,
    pub supports_thread_permission_overrides: bool,
    pub reports_effective_thread_permissions: bool,
}

/// Map an alleycat-advertised agent (`name` + `display_name`) to the
/// canonical runtime-kind id litter uses internally. Known agents get
/// their well-known alias normalized (e.g. `pi.dev` → `pi`,
/// `factory-droid` → `droid`) so the rest of the code can match against
/// stable ids. Anything else falls through to the agent's own
/// lowercased name (or display name if name is empty), so new agents
/// advertised by alleycat work without a litter release.
pub fn agent_runtime_kind(name: &str, display_name: &str) -> Option<AgentRuntimeKindId> {
    let name = name.trim().to_ascii_lowercase();
    let display_name = display_name.trim().to_ascii_lowercase();
    let candidate = if name.is_empty() {
        display_name.as_str()
    } else {
        name.as_str()
    };
    let canonical = match candidate {
        "codex" => Some("codex"),
        "pi" | "pi.dev" | "pidev" => Some("pi"),
        "amp" | "ampcode" | "amp-code" | "amp_code" => Some("amp"),
        "opencode" | "open-code" | "open_code" => Some("opencode"),
        "claude" | "claude-code" | "claude_code" => Some("claude"),
        "droid" | "factory" | "factory-droid" | "factory_droid" => Some("droid"),
        "hermes" => Some("hermes"),
        _ if display_name == "codex" => Some("codex"),
        _ if display_name == "pi" || display_name == "pi.dev" => Some("pi"),
        _ if display_name == "amp" || display_name == "amp code" => Some("amp"),
        _ if display_name == "opencode" || display_name == "open code" => Some("opencode"),
        _ if display_name == "claude" || display_name == "claude code" => Some("claude"),
        _ if display_name == "droid"
            || display_name == "factory"
            || display_name == "factory droid" =>
        {
            Some("droid")
        }
        _ if display_name == "hermes" => Some("hermes"),
        _ => None,
    };
    if let Some(kind) = canonical {
        return Some(kind.to_string());
    }
    if candidate.is_empty() {
        return None;
    }
    Some(candidate.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentWire {
    Websocket,
    Jsonl,
}

/// Reconnect strategy for an alleycat-backed session. The transport
/// holds a clone of the app-wide shared iroh `Endpoint` (cheap — iroh's
/// `Endpoint` is an `Arc`-backed handle) so reconnects open a fresh
/// `Connection` on the existing endpoint instead of binding a new one.
///
/// `current_session` tracks the most recently established `AlleycatSession`
/// (and therefore its `Connection`) so that lifecycle code outside the
/// session worker can call `close_current_connection()` to abandon a
/// silently-dead connection — e.g. after iOS resumed the process from a
/// long background, where iroh's idle timer would otherwise wait 30s
/// before declaring the path dead.
pub struct AlleycatReconnectTransport {
    pub params: ParsedPairPayload,
    pub agent: String,
    pub wire: AgentWire,
    endpoint: Endpoint,
    current_session: Arc<tokio::sync::Mutex<Option<Arc<AlleycatSession>>>>,
    last_seen_seq: Arc<AtomicU64>,
}

impl AlleycatReconnectTransport {
    pub fn new(
        params: ParsedPairPayload,
        agent: String,
        wire: AgentWire,
        endpoint: Endpoint,
    ) -> Self {
        Self {
            params,
            agent,
            wire,
            endpoint,
            current_session: Arc::new(tokio::sync::Mutex::new(None)),
            last_seen_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Open the initial app-server client using the same sequence tracker
    /// future reconnects will use. This lets reconnect send an explicit
    /// `resume.last_seq` cursor instead of relying only on server-side
    /// auto-resume heuristics.
    pub async fn connect_initial(
        &self,
    ) -> Result<(AppServerClient, Arc<AlleycatSession>), AlleycatError> {
        connect_app_server_client(
            &self.endpoint,
            self.params.clone(),
            self.agent.clone(),
            self.wire,
            Some(Arc::clone(&self.last_seen_seq)),
            None,
        )
        .await
    }

    /// Register the freshly-built session with the transport so external
    /// lifecycle code can target its `Connection`. Called once after
    /// `connect_remote_over_alleycat` builds the initial session, and
    /// implicitly by every successful `reconnect()`.
    pub async fn register_initial_session(&self, session: Arc<AlleycatSession>) {
        *self.current_session.lock().await = Some(session);
    }
}

#[async_trait]
impl RemoteTransport for AlleycatReconnectTransport {
    async fn reconnect(
        &self,
        _args: &RemoteAppServerConnectArgs,
        _websocket_url: &str,
    ) -> Result<Reconnected, TransportError> {
        // Open a brand-new iroh Connection on the shared Endpoint and run
        // the alleycat handshake on it. The previous Connection is dropped
        // only after the new keepalive is installed in the worker.
        let resume_from = self.last_seen_seq.load(Ordering::Relaxed);
        let (client, session) = connect_app_server_client(
            &self.endpoint,
            self.params.clone(),
            self.agent.clone(),
            self.wire,
            Some(Arc::clone(&self.last_seen_seq)),
            (resume_from > 0).then_some(resume_from),
        )
        .await
        .map_err(|error| TransportError::ConnectionFailed(error.to_string()))?;
        *self.current_session.lock().await = Some(Arc::clone(&session));
        let keepalive: Arc<dyn SessionKeepalive> = session;
        Ok(Reconnected {
            client,
            keepalive: Some(keepalive),
        })
    }

    async fn notify_network_change(&self) {
        if self.endpoint.is_closed() {
            debug!("alleycat notify_network_change: endpoint already closed; skipping");
            return;
        }
        info!(
            "alleycat notify_network_change: hinting iroh to re-evaluate paths node_id={}",
            self.params.node_id
        );
        self.endpoint.network_change().await;
    }

    async fn close_current_connection(&self) {
        let session = self.current_session.lock().await.clone();
        if let Some(session) = session {
            info!(
                "alleycat close_current_connection: abandoning Connection node_id={}",
                self.params.node_id
            );
            session.close();
        } else {
            debug!("alleycat close_current_connection: no current session");
        }
    }
}

/// Live alleycat session. Owns the iroh `Connection` (cheap-Arc handle)
/// rather than the `Endpoint` — the endpoint is shared app-wide and
/// outlives any individual session. Dropping an `AlleycatSession`
/// implicitly closes the `Connection` (the last Arc handle drops); call
/// `close().await` first for a graceful shutdown that sends a
/// CONNECTION_CLOSE frame to the host.
pub struct AlleycatSession {
    connection: Connection,
    pub params: ParsedPairPayload,
    pub agent: String,
    pub wire: AgentWire,
}

impl AlleycatSession {
    /// Clone of the underlying iroh `Connection`. Useful for diagnostics
    /// (`close_reason`, `rtt`) or for spawning per-connection liveness
    /// probes that race a `Connection::closed()` future.
    pub fn connection(&self) -> Connection {
        self.connection.clone()
    }

    pub(crate) fn close(&self) {
        <Self as SessionKeepalive>::close(self);
    }
}

impl SessionKeepalive for AlleycatSession {
    fn close(&self) {
        // iroh's `Connection::close` is sync (queues the CLOSE frame); the
        // actual flush happens on the endpoint's IO loop. Calling it on an
        // already-closed connection is a no-op.
        debug!(
            "alleycat session close: sending CONNECTION_CLOSE node_id={}",
            self.params.node_id
        );
        self.connection
            .close(VarInt::from_u32(0), b"client disconnect");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AlleycatError {
    #[error("invalid pair payload: {0}")]
    InvalidPayload(String),
    #[error("protocol version mismatch: payload={payload} client={client}")]
    ProtocolMismatch { payload: u32, client: u32 },
    #[error("transport error: {0}")]
    Transport(String),
}

#[derive(Debug, Deserialize)]
struct PairPayloadWire {
    v: u32,
    node_id: String,
    token: String,
    relay: Option<String>,
    #[serde(default, alias = "hostname", alias = "display_name", alias = "name")]
    host_name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    ListAgents {
        v: u32,
        token: String,
    },
    RestartAgent {
        v: u32,
        token: String,
        agent: String,
    },
    Connect {
        v: u32,
        token: String,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<Resume>,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
struct Resume {
    last_seq: u64,
}

#[derive(Debug, Deserialize)]
struct Response {
    v: u32,
    ok: bool,
    #[serde(default)]
    agents: Vec<AgentInfoWire>,
    #[serde(default)]
    session: Option<SessionInfoWire>,
    error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct SessionInfoWire {
    attached: AttachKindWire,
    current_seq: u64,
    floor_seq: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AttachKindWire {
    Fresh,
    Resumed,
    DriftReload,
}

#[derive(Debug, Deserialize)]
struct AgentInfoWire {
    name: String,
    display_name: String,
    wire: AgentWireWire,
    available: bool,
    #[serde(default)]
    presentation: Option<AgentPresentationWire>,
    #[serde(default)]
    capabilities: Option<AgentCapabilitiesWire>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentWireWire {
    Websocket,
    Jsonl,
}

#[derive(Debug, Deserialize)]
struct AgentPresentationWire {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    is_beta: bool,
    #[serde(default)]
    sort_order: i32,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AgentCapabilitiesWire {
    #[serde(default)]
    locks_reasoning_effort_after_activity: bool,
    #[serde(default)]
    visible_modes: Option<Vec<String>>,
    #[serde(default)]
    supports_ssh_bridge: bool,
    #[serde(default)]
    uses_direct_codex_port: bool,
    #[serde(default)]
    supports_thread_permission_overrides: Option<bool>,
    #[serde(default)]
    reports_effective_thread_permissions: Option<bool>,
}

impl From<AgentPresentationWire> for AgentPresentation {
    fn from(value: AgentPresentationWire) -> Self {
        AgentPresentation {
            title: value.title,
            is_beta: value.is_beta,
            sort_order: value.sort_order,
            description: value.description,
            aliases: value.aliases,
        }
    }
}

impl From<AgentCapabilitiesWire> for AgentCapabilities {
    fn from(value: AgentCapabilitiesWire) -> Self {
        AgentCapabilities {
            locks_reasoning_effort_after_activity: value.locks_reasoning_effort_after_activity,
            visible_modes: value.visible_modes,
            supports_ssh_bridge: value.supports_ssh_bridge,
            uses_direct_codex_port: value.uses_direct_codex_port,
            // Legacy alleycat/kittylitter daemons did not advertise these fields.
            // Preserve the old client behaviour unless a daemon explicitly says
            // permission overrides/effective-permission reporting are unsupported.
            supports_thread_permission_overrides: value
                .supports_thread_permission_overrides
                .unwrap_or(true),
            reports_effective_thread_permissions: value
                .reports_effective_thread_permissions
                .unwrap_or(true),
        }
    }
}

pub fn parse_pair_payload(json: &str) -> Result<ParsedPairPayload, AlleycatError> {
    let wire: PairPayloadWire = serde_json::from_str(json)
        .map_err(|error| AlleycatError::InvalidPayload(format!("malformed JSON: {error}")))?;
    if wire.v != ALLEYCAT_PROTOCOL_VERSION {
        return Err(AlleycatError::ProtocolMismatch {
            payload: wire.v,
            client: ALLEYCAT_PROTOCOL_VERSION,
        });
    }
    if wire.node_id.trim().is_empty() {
        return Err(AlleycatError::InvalidPayload("empty node_id".into()));
    }
    EndpointId::from_str(&wire.node_id)
        .map_err(|error| AlleycatError::InvalidPayload(format!("invalid node_id: {error}")))?;
    if wire.token.trim().is_empty() {
        return Err(AlleycatError::InvalidPayload("empty token".into()));
    }
    if let Some(relay) = wire.relay.as_deref() {
        RelayUrl::from_str(relay).map_err(|error| {
            AlleycatError::InvalidPayload(format!("invalid relay URL: {error}"))
        })?;
    }
    Ok(ParsedPairPayload {
        version: wire.v,
        node_id: wire.node_id,
        token: wire.token,
        relay: wire.relay,
        host_name: normalize_optional_host_name(wire.host_name),
    })
}

fn normalize_optional_host_name(host_name: Option<String>) -> Option<String> {
    host_name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

pub async fn list_agents(
    endpoint: &Endpoint,
    params: ParsedPairPayload,
) -> Result<Vec<AgentInfo>, AlleycatError> {
    let (conn, mut send, mut recv) = open_stream_on(endpoint, &params).await?;
    write_json_frame(
        &mut send,
        &Request::ListAgents {
            v: ALLEYCAT_PROTOCOL_VERSION,
            token: params.token.clone(),
        },
    )
    .await?;
    let response: Response = read_json_frame(&mut recv).await?;
    validate_response(&response)?;
    // The probe connection is one-shot — close it gracefully so the host
    // doesn't have to wait on its idle timeout to drop the entry.
    conn.close(VarInt::from_u32(0), b"list_agents complete");
    Ok(response
        .agents
        .into_iter()
        .map(|agent| AgentInfo {
            name: agent.name,
            display_name: agent.display_name,
            wire: agent.wire.into(),
            available: agent.available,
            presentation: agent.presentation.map(Into::into),
            capabilities: agent.capabilities.map(Into::into),
        })
        .collect())
}

pub async fn restart_agent(
    endpoint: &Endpoint,
    params: ParsedPairPayload,
    agent: String,
) -> Result<(), AlleycatError> {
    let (conn, mut send, mut recv) = open_stream_on(endpoint, &params).await?;
    write_json_frame(
        &mut send,
        &Request::RestartAgent {
            v: ALLEYCAT_PROTOCOL_VERSION,
            token: params.token.clone(),
            agent,
        },
    )
    .await?;
    let response: Response = read_json_frame(&mut recv).await?;
    validate_response(&response)?;
    conn.close(VarInt::from_u32(0), b"restart_agent complete");
    Ok(())
}

pub async fn connect_app_server_client(
    endpoint: &Endpoint,
    params: ParsedPairPayload,
    agent: String,
    wire: AgentWire,
    seq_tracker: Option<Arc<AtomicU64>>,
    resume_from: Option<u64>,
) -> Result<(AppServerClient, Arc<AlleycatSession>), AlleycatError> {
    let (connection, mut send, mut recv) = open_stream_on(endpoint, &params).await?;
    write_json_frame(
        &mut send,
        &Request::Connect {
            v: ALLEYCAT_PROTOCOL_VERSION,
            token: params.token.clone(),
            agent: agent.clone(),
            resume: resume_from.map(|last_seq| Resume { last_seq }),
        },
    )
    .await?;
    let response: Response = read_json_frame(&mut recv).await?;
    validate_response(&response)?;
    log_session_info(&params, &agent, response.session.as_ref(), resume_from);
    let label = format!("alleycat://{}/{}", params.node_id, agent);
    let args = RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: format!("ws://alleycat/{agent}"),
            auth_token: None,
        },
        client_name: "Litter".to_string(),
        client_version: "1.0".to_string(),
        experimental_api: true,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 256,
    };
    let stream = AlleycatStream::new(send, recv, seq_tracker);
    let remote = match wire {
        AgentWire::Websocket => {
            RemoteAppServerClient::connect_websocket_stream(stream, args, label)
                .await
                .map_err(|error| AlleycatError::Transport(error.to_string()))?
        }
        AgentWire::Jsonl => {
            codex_slingshot::json_line_wire::connect_json_line_stream(stream, args, label)
                .await
                .map_err(|error| AlleycatError::Transport(error.to_string()))?
        }
    };
    let session = Arc::new(AlleycatSession {
        connection,
        params,
        agent,
        wire,
    });
    Ok((AppServerClient::Remote(remote), session))
}

pub(crate) async fn connect_jsonl_agent_stream(
    endpoint: &Endpoint,
    params: ParsedPairPayload,
    agent: String,
) -> Result<(AlleycatStream, Arc<AlleycatSession>), AlleycatError> {
    let (connection, mut send, mut recv) = open_stream_on(endpoint, &params).await?;
    write_json_frame(
        &mut send,
        &Request::Connect {
            v: ALLEYCAT_PROTOCOL_VERSION,
            token: params.token.clone(),
            agent: agent.clone(),
            resume: None,
        },
    )
    .await?;
    let response: Response = read_json_frame(&mut recv).await?;
    validate_response(&response)?;
    log_session_info(&params, &agent, response.session.as_ref(), None);
    let session = Arc::new(AlleycatSession {
        connection,
        params,
        agent,
        wire: AgentWire::Jsonl,
    });
    Ok((AlleycatStream::new(send, recv, None), session))
}

/// Build the app-wide alleycat iroh `Endpoint`. Called exactly once per
/// process via `MobileClient::alleycat_endpoint()` — every alleycat
/// operation thereafter reuses the resulting handle.
///
/// `secret_key_bytes` is the persisted-or-fresh device key bytes from
/// the platform keychain. When `None`, this function generates a fresh
/// key; the caller (`MobileClient::alleycat_endpoint`) reads back the
/// actually-used bytes from the returned endpoint and persists them to
/// the platform keychain so subsequent launches reuse the same
/// `EndpointId`.
///
/// We intentionally do NOT override the QUIC `max_idle_timeout`. iroh's
/// default `keep_alive_interval` keeps healthy idle connections alive
/// indefinitely (peer ACKs reset the timer), while the default 30s
/// connection idle timeout means dead paths — e.g. after iOS suspended
/// the process and the host's NAT entry expired — surface as an error
/// within ~30s instead of hanging on the previous 600s override. The
/// session worker drives a fresh `AlleycatReconnectTransport::reconnect()`
/// automatically when the connection times out.
///
/// We DO override `keep_alive_interval` from iroh's 5s default up to 15s
/// to reduce cellular radio wakes when the app sits foregrounded but
/// idle. 15s is still well under typical 30–60s NAT UDP timeouts, and
/// `default_path_max_idle_timeout` stays at iroh's 15s so dead paths
/// still surface fast on resume.
pub async fn bind_alleycat_endpoint(
    secret_key_bytes: Option<[u8; 32]>,
) -> Result<Endpoint, AlleycatError> {
    let transport = QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(15))
        .build();
    let secret_key = match secret_key_bytes {
        Some(bytes) => {
            info!("alleycat: using persisted device secret key");
            SecretKey::from_bytes(&bytes)
        }
        None => {
            info!("alleycat: generating fresh device secret key");
            SecretKey::generate()
        }
    };
    let endpoint_builder = Endpoint::builder(iroh::endpoint::presets::N0)
        .transport_config(transport)
        .secret_key(secret_key);
    // iroh-on-Android can't use the system DNS resolver / system CA
    // roots from inside a packaged app — fall back to public DNS +
    // embedded CA roots there. iOS/macOS pick these up natively.
    #[cfg(target_os = "android")]
    let endpoint_builder = endpoint_builder
        .dns_resolver(iroh::dns::DnsResolver::with_nameserver(
            std::net::SocketAddr::from(([8, 8, 8, 8], 53)),
        ))
        .ca_roots_config(iroh::tls::CaRootsConfig::embedded());
    info!("alleycat: binding shared iroh endpoint");
    endpoint_builder
        .bind()
        .await
        .map_err(|error| AlleycatError::Transport(format!("binding iroh endpoint: {error}")))
}

/// Open a fresh QUIC connection + bidirectional stream to the alleycat
/// peer described by `params`, on the supplied (shared) endpoint.
async fn open_stream_on(
    endpoint: &Endpoint,
    params: &ParsedPairPayload,
) -> Result<(Connection, SendStream, RecvStream), AlleycatError> {
    let id = EndpointId::from_str(&params.node_id)
        .map_err(|error| AlleycatError::InvalidPayload(format!("invalid node_id: {error}")))?;
    let mut addr = EndpointAddr::new(id);
    if let Some(relay) = params.relay.as_deref() {
        let relay = RelayUrl::from_str(relay).map_err(|error| {
            AlleycatError::InvalidPayload(format!("invalid relay URL: {error}"))
        })?;
        addr = addr.with_relay_url(relay);
    }
    info!("alleycat: connecting node_id={}", params.node_id);
    let conn = endpoint
        .connect(addr, ALLEYCAT_ALPN)
        .await
        .map_err(|error| AlleycatError::Transport(format!("connecting iroh endpoint: {error}")))?;
    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|error| AlleycatError::Transport(format!("opening iroh stream: {error}")))?;
    Ok((conn, send, recv))
}

async fn read_json_frame<T, R>(reader: &mut R) -> Result<T, AlleycatError>
where
    T: for<'de> Deserialize<'de>,
    R: AsyncRead + Unpin,
{
    let len = reader
        .read_u32()
        .await
        .map_err(|error| AlleycatError::Transport(format!("reading frame length: {error}")))?
        as usize;
    if len > MAX_FRAME_BYTES {
        return Err(AlleycatError::Transport(format!(
            "frame too large: {len} bytes"
        )));
    }
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .map_err(|error| AlleycatError::Transport(format!("reading frame body: {error}")))?;
    serde_json::from_slice(&buf)
        .map_err(|error| AlleycatError::Transport(format!("decoding frame JSON: {error}")))
}

async fn write_json_frame<T, W>(writer: &mut W, value: &T) -> Result<(), AlleycatError>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let buf = serde_json::to_vec(value)
        .map_err(|error| AlleycatError::Transport(format!("encoding frame JSON: {error}")))?;
    if buf.len() > MAX_FRAME_BYTES {
        return Err(AlleycatError::Transport(format!(
            "frame too large: {} bytes",
            buf.len()
        )));
    }
    writer
        .write_u32(buf.len() as u32)
        .await
        .map_err(|error| AlleycatError::Transport(format!("writing frame length: {error}")))?;
    writer
        .write_all(&buf)
        .await
        .map_err(|error| AlleycatError::Transport(format!("writing frame body: {error}")))?;
    writer
        .flush()
        .await
        .map_err(|error| AlleycatError::Transport(format!("flushing frame: {error}")))?;
    Ok(())
}

fn log_session_info(
    params: &ParsedPairPayload,
    agent: &str,
    session: Option<&SessionInfoWire>,
    resume_from: Option<u64>,
) {
    let Some(session) = session else {
        debug!(
            node_id = %params.node_id,
            agent = %agent,
            resume_from = ?resume_from,
            "alleycat connect response did not include session info"
        );
        return;
    };

    match session.attached {
        AttachKindWire::DriftReload => warn!(
            node_id = %params.node_id,
            agent = %agent,
            resume_from = ?resume_from,
            current_seq = session.current_seq,
            floor_seq = session.floor_seq,
            "alleycat session replay drift detected; client should reload authoritative state"
        ),
        _ => info!(
            node_id = %params.node_id,
            agent = %agent,
            attached = ?session.attached,
            resume_from = ?resume_from,
            current_seq = session.current_seq,
            floor_seq = session.floor_seq,
            "alleycat session attached"
        ),
    }
}

fn validate_response(response: &Response) -> Result<(), AlleycatError> {
    if response.v != ALLEYCAT_PROTOCOL_VERSION {
        return Err(AlleycatError::ProtocolMismatch {
            payload: response.v,
            client: ALLEYCAT_PROTOCOL_VERSION,
        });
    }
    if !response.ok {
        return Err(AlleycatError::Transport(
            response
                .error
                .clone()
                .unwrap_or_else(|| "host rejected request".to_string()),
        ));
    }
    Ok(())
}

impl From<AgentWireWire> for AgentWire {
    fn from(value: AgentWireWire) -> Self {
        match value {
            AgentWireWire::Websocket => Self::Websocket,
            AgentWireWire::Jsonl => Self::Jsonl,
        }
    }
}

#[derive(Debug)]
pub(crate) struct AlleycatStream {
    send: SendStream,
    recv: RecvStream,
    seq_tracker: Option<Arc<AtomicU64>>,
    seq_line_buf: Vec<u8>,
}

impl AlleycatStream {
    fn new(send: SendStream, recv: RecvStream, seq_tracker: Option<Arc<AtomicU64>>) -> Self {
        Self {
            send,
            recv,
            seq_tracker,
            seq_line_buf: Vec::with_capacity(4096),
        }
    }

    fn observe_read_bytes(&mut self, bytes: &[u8]) {
        let Some(tracker) = self.seq_tracker.as_ref().map(Arc::clone) else {
            return;
        };
        for &byte in bytes {
            if byte == b'\n' {
                self.observe_json_line(&tracker);
                self.seq_line_buf.clear();
            } else if self.seq_line_buf.len() < MAX_FRAME_BYTES {
                self.seq_line_buf.push(byte);
            } else {
                self.seq_line_buf.clear();
            }
        }
    }

    fn observe_json_line(&self, tracker: &AtomicU64) {
        observe_alleycat_seq_json_line(&self.seq_line_buf, tracker);
    }
}

fn observe_alleycat_seq_json_line(line: &[u8], tracker: &AtomicU64) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return;
    };
    let Some(seq) = value.get("_alleycat_seq").and_then(|v| v.as_u64()) else {
        return;
    };
    tracker.fetch_max(seq, Ordering::Relaxed);
}

impl AsyncRead for AlleycatStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.recv).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let after = buf.filled().len();
                if after > before {
                    this.observe_read_bytes(&buf.filled()[before..after]);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl AsyncWrite for AlleycatStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        AsyncWrite::poll_write(Pin::new(&mut this.send), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        AsyncWrite::poll_flush(Pin::new(&mut this.send), cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        AsyncWrite::poll_shutdown(Pin::new(&mut this.send), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_request_omits_resume_until_sequence_seen() {
        let request = Request::Connect {
            v: ALLEYCAT_PROTOCOL_VERSION,
            token: "token".into(),
            agent: "pi".into(),
            resume: None,
        };
        let value = serde_json::to_value(request).expect("serialize");
        assert_eq!(value["op"], "connect");
        assert!(value.get("resume").is_none());
    }

    #[test]
    fn connect_request_serializes_resume_cursor() {
        let request = Request::Connect {
            v: ALLEYCAT_PROTOCOL_VERSION,
            token: "token".into(),
            agent: "pi".into(),
            resume: Some(Resume { last_seq: 42 }),
        };
        let value = serde_json::to_value(request).expect("serialize");
        assert_eq!(value["resume"]["last_seq"], 42);
    }

    #[test]
    fn observe_alleycat_seq_tracks_highest_seen_sequence() {
        let tracker = AtomicU64::new(0);
        observe_alleycat_seq_json_line(br#"{"jsonrpc":"2.0","_alleycat_seq":7}"#, &tracker);
        observe_alleycat_seq_json_line(br#"{"jsonrpc":"2.0","_alleycat_seq":3}"#, &tracker);
        observe_alleycat_seq_json_line(br#"{"jsonrpc":"2.0","method":"noop"}"#, &tracker);
        assert_eq!(tracker.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn parse_pair_payload_happy_path() {
        let key = iroh::SecretKey::generate();
        let json = format!(
            r#"{{"v":1,"node_id":"{}","token":"deadbeef","relay":"https://relay.example.com","host_name":"studio.local"}}"#,
            key.public()
        );
        let parsed = parse_pair_payload(&json).expect("parse");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.node_id, key.public().to_string());
        assert_eq!(parsed.token, "deadbeef");
        assert_eq!(parsed.relay.as_deref(), Some("https://relay.example.com"));
        assert_eq!(parsed.host_name.as_deref(), Some("studio.local"));
    }

    #[test]
    fn parse_pair_payload_accepts_legacy_hostname_alias() {
        let key = iroh::SecretKey::generate();
        let json = format!(
            r#"{{"v":1,"node_id":"{}","token":"deadbeef","hostname":"studio"}}"#,
            key.public()
        );
        let parsed = parse_pair_payload(&json).expect("parse");
        assert_eq!(parsed.host_name.as_deref(), Some("studio"));
    }

    #[test]
    fn parse_pair_payload_rejects_bad_node_id() {
        let err = parse_pair_payload(r#"{"v":1,"node_id":"nope","token":"deadbeef"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid node_id"));
    }

    #[test]
    fn agent_runtime_kind_maps_known_agents() {
        assert_eq!(
            agent_runtime_kind("codex", "Codex"),
            Some("codex".to_string())
        );
        assert_eq!(agent_runtime_kind("pi.dev", "Pi"), Some("pi".to_string()));
        assert_eq!(agent_runtime_kind("amp", "Amp"), Some("amp".to_string()));
        assert_eq!(
            agent_runtime_kind("open-code", "opencode"),
            Some("opencode".to_string())
        );
        assert_eq!(
            agent_runtime_kind("claude-code", "Claude"),
            Some("claude".to_string())
        );
        assert_eq!(
            agent_runtime_kind("factory-droid", "Factory Droid"),
            Some("droid".to_string())
        );
    }

    #[test]
    fn agent_runtime_kind_passes_through_unknown_agents() {
        assert_eq!(
            agent_runtime_kind("devin", "Devin"),
            Some("devin".to_string())
        );
        assert_eq!(
            agent_runtime_kind("Custom-Agent", "Custom Agent"),
            Some("custom-agent".to_string())
        );
        assert_eq!(agent_runtime_kind("", ""), None);
    }

    #[test]
    fn response_decodes_legacy_capabilities_as_permission_authoritative() {
        let response: Response = serde_json::from_str(
            r#"{"v":1,"ok":true,"agents":[{"name":"codex","display_name":"Codex","wire":"websocket","available":true,"capabilities":{"supports_ssh_bridge":true,"uses_direct_codex_port":true}}]}"#,
        )
        .expect("decode response");
        let capabilities: AgentCapabilities = response
            .agents
            .into_iter()
            .next()
            .and_then(|agent| agent.capabilities)
            .expect("capabilities")
            .into();

        assert!(capabilities.supports_thread_permission_overrides);
        assert!(capabilities.reports_effective_thread_permissions);
    }

    #[test]
    fn response_decodes_explicit_permission_capability_false() {
        let response: Response = serde_json::from_str(
            r#"{"v":1,"ok":true,"agents":[{"name":"pi","display_name":"Pi","wire":"jsonl","available":true,"capabilities":{"supports_ssh_bridge":true,"uses_direct_codex_port":false,"supports_thread_permission_overrides":false,"reports_effective_thread_permissions":false}}]}"#,
        )
        .expect("decode response");
        let capabilities: AgentCapabilities = response
            .agents
            .into_iter()
            .next()
            .and_then(|agent| agent.capabilities)
            .expect("capabilities")
            .into();

        assert!(!capabilities.supports_thread_permission_overrides);
        assert!(!capabilities.reports_effective_thread_permissions);
    }

    #[test]
    fn response_decodes_amp_jsonl_agent() {
        let response: Response = serde_json::from_str(
            r#"{"v":1,"ok":true,"agents":[{"name":"amp","display_name":"Amp","wire":"jsonl","available":true}]}"#,
        )
        .expect("decode response");
        let agent = response.agents.first().expect("agent");

        assert_eq!(
            agent_runtime_kind(&agent.name, &agent.display_name),
            Some("amp".to_string())
        );
        assert!(agent.available);
        assert_eq!(AgentWire::from(agent.wire), AgentWire::Jsonl);
    }

    /// `AlleycatReconnectTransport` must coerce to `Arc<dyn RemoteTransport>`
    /// — that's how the worker's reconnect plumbing receives it. This is a
    /// pure type-check test: it compiles iff the trait impl stays object-safe.
    /// Building a real Endpoint would require a tokio runtime + network, so
    /// we lean on the `#[allow(dead_code)]` static-check function below
    /// instead — `cargo check` exercises the trait bounds without needing
    /// to instantiate the type at runtime.
    #[allow(dead_code)]
    fn alleycat_reconnect_transport_coerces_to_trait_object(transport: AlleycatReconnectTransport) {
        let _erased: Arc<dyn RemoteTransport> = Arc::new(transport);
    }

    #[test]
    fn agent_runtime_kind_pi_roundtrip() {
        let json = serde_json::to_value(AlleycatAgentRuntimeKind::Pi).expect("serialize");
        assert_eq!(json, serde_json::json!("pi"));

        let parsed: AlleycatAgentRuntimeKind =
            serde_json::from_value(json).expect("deserialize");
        assert_eq!(parsed, AlleycatAgentRuntimeKind::Pi);

        // The typed enum must align with the canonicalized stringly-typed
        // id produced by `agent_runtime_kind()` for the same agent names.
        assert_eq!(AlleycatAgentRuntimeKind::Pi.as_id(), "pi");
        assert_eq!(
            agent_runtime_kind("pi", "pi"),
            Some(AlleycatAgentRuntimeKind::Pi.into_id())
        );
    }

    fn sample_pair_payload(host_name: Option<&str>) -> ParsedPairPayload {
        let key = iroh::SecretKey::generate();
        ParsedPairPayload {
            version: ALLEYCAT_PROTOCOL_VERSION,
            node_id: key.public().to_string(),
            token: "deadbeef".to_string(),
            relay: None,
            host_name: host_name.map(str::to_string),
        }
    }

    fn pi_agent_info(available: bool) -> AgentInfo {
        AgentInfo {
            name: "pi".to_string(),
            display_name: "Pi".to_string(),
            wire: AgentWire::Jsonl,
            available,
            presentation: None,
            capabilities: None,
        }
    }

    #[test]
    fn build_manifest_pi_entry_has_required_capability_flags() {
        let params = sample_pair_payload(Some("studio.local"));
        let manifest = build_litter_manifest(&params, &[pi_agent_info(true)]);

        assert_eq!(manifest.version, LITTER_ALLEYCAT_MANIFEST_VERSION);
        assert_eq!(manifest.host_id, params.node_id);
        assert_eq!(manifest.host_name.as_deref(), Some("studio.local"));
        assert_eq!(manifest.hosts.len(), 1);

        let entry = &manifest.hosts[0];
        assert_eq!(entry.agent_runtime_kind, LitterManifestRuntimeKind::Pi);
        assert_eq!(entry.host_id, params.node_id);
        assert_eq!(entry.wire, LitterManifestWire::Jsonl);
        assert!(entry.available);
        let caps = entry.capabilities;
        assert!(!caps.voice, "pi voice must be false");
        assert!(!caps.plans, "pi plans must be false");
        assert!(caps.tool_exec, "pi tool_exec must be true");
        assert!(
            !caps.uses_direct_codex_port,
            "pi uses_direct_codex_port must be false"
        );
    }

    #[test]
    fn build_manifest_pi_capabilities_ignore_host_overrides() {
        // Even if the alleycat host advertised aggressive capability
        // flags for the pi agent, the litter manifest must coerce them
        // to the VAL-REM-009 canonical set.
        let params = sample_pair_payload(None);
        let mut agent = pi_agent_info(true);
        agent.capabilities = Some(AgentCapabilities {
            locks_reasoning_effort_after_activity: false,
            visible_modes: None,
            supports_ssh_bridge: true,
            uses_direct_codex_port: true,
            supports_thread_permission_overrides: true,
            reports_effective_thread_permissions: true,
        });
        let manifest = build_litter_manifest(&params, &[agent]);
        let caps = manifest.hosts[0].capabilities;
        assert!(!caps.voice);
        assert!(!caps.plans);
        assert!(caps.tool_exec);
        assert!(
            !caps.uses_direct_codex_port,
            "pi entry must not opt into the direct codex port even when host advertises it"
        );
    }

    #[test]
    fn build_manifest_serializes_pi_runtime_kind_as_pi() {
        let params = sample_pair_payload(None);
        let manifest = build_litter_manifest(&params, &[pi_agent_info(true)]);
        let json = serde_json::to_value(&manifest).expect("serialize manifest");
        assert_eq!(
            json["hosts"][0]["agent_runtime_kind"], "Pi",
            "validators jq for agent_runtime_kind==\"Pi\""
        );
        assert_eq!(json["hosts"][0]["capabilities"]["voice"], false);
        assert_eq!(json["hosts"][0]["capabilities"]["plans"], false);
        assert_eq!(json["hosts"][0]["capabilities"]["tool_exec"], true);
        assert_eq!(
            json["hosts"][0]["capabilities"]["uses_direct_codex_port"],
            false
        );
    }

    #[test]
    fn litter_manifest_runtime_kind_from_agent_handles_known_and_unknown() {
        assert_eq!(
            LitterManifestRuntimeKind::from_agent("pi", "Pi"),
            LitterManifestRuntimeKind::Pi
        );
        assert_eq!(
            LitterManifestRuntimeKind::from_agent("codex", "Codex"),
            LitterManifestRuntimeKind::Codex
        );
        assert_eq!(
            LitterManifestRuntimeKind::from_agent("devin", "Devin"),
            LitterManifestRuntimeKind::Other,
            "unknown agents collapse to Other so the manifest stays self-describing"
        );
    }
}
