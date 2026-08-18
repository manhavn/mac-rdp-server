use core::fmt;
use core::net::SocketAddr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::time::Duration;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use ironrdp_acceptor::{Acceptor, AcceptorResult, BeginResult, DesktopSize};
use ironrdp_async::Framed;
use ironrdp_cliprdr::CliprdrServer;
use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_core::{decode, encode_vec, impl_as_any};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_displaycontrol::server::{DisplayControlHandler, DisplayControlServer};
use ironrdp_dvc as dvc;
use ironrdp_pdu::input::InputEventPdu;
use ironrdp_pdu::input::fast_path::{FastPathInput, FastPathInputEvent};
use ironrdp_pdu::mcs::{SendDataIndication, SendDataRequest};
use ironrdp_pdu::rdp::capability_sets::{
    BitmapCodecs, CapabilitySet, CmdFlags, CodecProperty, GeneralExtraFlags,
};
pub use ironrdp_pdu::rdp::client_info::Credentials;
use ironrdp_pdu::rdp::headers::{ServerDeactivateAll, ShareControlPdu};
use ironrdp_pdu::rdp::server_error_info::{
    ErrorInfo, ProtocolIndependentCode, ServerSetErrorInfoPdu,
};
use ironrdp_pdu::x224::X224;
use ironrdp_pdu::{Action, PduResult, decode_err, mcs, nego, rdp};
use ironrdp_rdpsnd as rdpsnd;
use ironrdp_svc::{
    ChannelFlags, StaticChannelId, StaticChannelSet, SvcProcessor, server_encode_svc_messages,
};
use ironrdp_tokio::{
    FramedRead, FramedWrite, TokioFramed, split_tokio_framed, unsplit_tokio_framed,
};
use rdpsnd::server::{RdpsndServer, RdpsndServerMessage};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpSocket;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, trace, warn};

use crate::autodetect::{AutoDetectManager, RttSnapshot};
use crate::clipboard::CliprdrServerFactory;
use crate::display::{DisplayUpdate, RdpServerDisplay};
use crate::echo::{EchoDvcBridge, EchoServerHandle, EchoServerMessage, build_echo_request};
use crate::encoder::{UpdateEncoder, UpdateEncoderCodecs};
#[cfg(feature = "egfx")]
use crate::gfx::{EgfxServerMessage, GfxServerFactory};
use crate::handler::RdpServerInputHandler;
use crate::{SoundServerFactory, builder, capabilities};

/// TCP listen backlog size for the RDP server socket.
const LISTENER_BACKLOG: u32 = 1024;

/// Action to take after a client disconnects.
///
/// Returned by [`ConnectionHandler::on_disconnected`] to control whether
/// the server continues accepting new connections or shuts down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostConnectionAction {
    /// Continue accepting new connections.
    Continue,
    /// Stop the accept loop and return from [`RdpServer::run`].
    Stop,
}

/// Hooks for connection lifecycle events in [`RdpServer::run`].
///
/// Implement this trait to add pre-accept filtering (rate limiting,
/// IP allowlists) and post-disconnect logic (cleanup, session validity
/// checks, metrics).
///
/// All methods have default implementations that accept all connections
/// and continue unconditionally.
pub trait ConnectionHandler: Send + Sync {
    /// Called after `accept()` returns but before `run_connection()`.
    ///
    /// Return `false` to reject the connection (the TCP stream is dropped).
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        let _ = peer;
        true
    }

    /// Called after `run_connection()` completes (successfully or with error).
    ///
    /// `duration` is the wall-clock time the connection was active.
    /// `error` is `Some` if the connection ended with an error.
    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&anyhow::Error>,
    ) -> PostConnectionAction {
        let _ = (peer, duration, error);
        PostConnectionAction::Continue
    }
}

/// Outcome of a successful [`CredentialValidator::validate`] call.
///
/// A rejection from a working validator is not an error: the validator did
/// its job and decided the credentials do not authenticate. Backend failures
/// (LDAP unreachable, PAM transport broken, database connection lost) are
/// reported via [`CredentialValidationError`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialDecision {
    /// Credentials accepted; the connection proceeds.
    Accept,
    /// Credentials rejected; the connection is closed.
    Reject,
}

/// Error returned by a [`CredentialValidator`] when the validator backend
/// itself fails (rather than the credentials being invalid).
///
/// Wraps any [`core::error::Error`] from the backend (LDAP/PAM/DB/etc.) so
/// the trait does not require a particular error library in implementors or
/// consumers.
#[derive(Debug)]
pub struct CredentialValidationError {
    source: Box<dyn core::error::Error + Send + Sync>,
}

impl CredentialValidationError {
    /// Wrap a backend error as a credential-validation failure.
    pub fn new<E>(source: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self {
            source: Box::new(source),
        }
    }
}

impl fmt::Display for CredentialValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("credential validator backend failure")
    }
}

impl core::error::Error for CredentialValidationError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        Some(&*self.source)
    }
}

/// Server-side credential validator for TLS-mode connections.
///
/// Called during connection setup when the server receives client credentials
/// via `ClientInfoPdu`. Not used for CredSSP/Hybrid connections (those use
/// pre-loaded credentials for NTLM challenge-response).
///
/// Implement this trait to validate credentials against external systems
/// (PAM, LDAP, database, etc.). For blocking backends, wrap the call in
/// `tokio::task::spawn_blocking` to avoid stalling the async runtime.
///
/// # Example
///
/// ```ignore
/// use ironrdp_server::{CredentialDecision, CredentialValidationError, CredentialValidator, Credentials};
///
/// struct StaticValidator {
///     expected_user: String,
///     expected_password: String,
/// }
///
/// #[async_trait::async_trait]
/// impl CredentialValidator for StaticValidator {
///     async fn validate(
///         &self,
///         creds: &Credentials,
///     ) -> Result<CredentialDecision, CredentialValidationError> {
///         if creds.username == self.expected_user && creds.password == self.expected_password {
///             Ok(CredentialDecision::Accept)
///         } else {
///             Ok(CredentialDecision::Reject)
///         }
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait CredentialValidator: Send + Sync {
    /// Validate credentials received from the client.
    ///
    /// Return `Ok(CredentialDecision::Accept)` to permit the connection,
    /// `Ok(CredentialDecision::Reject)` to refuse it. Return
    /// `Err(CredentialValidationError::new(_))` only when the validator
    /// itself could not produce a decision (backend system error).
    ///
    /// Implementors backed by blocking systems (PAM, libldap, a synchronous
    /// database driver) should offload the work, for example with
    /// `tokio::task::spawn_blocking`, so the returned future does not stall the
    /// caller's executor. Native-async backends can simply `.await`.
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError>;
}

/// A built-in [`CredentialValidator`] that accepts exactly one fixed set of credentials.
///
/// This is the validation-policy equivalent of the acceptor's pre-loaded
/// exact-match: it keeps the common "one known account" case a one-liner while
/// going through the same hook as PAM, LDAP, or database-backed validators.
pub struct ExactMatchCredentialValidator {
    expected: Credentials,
}

impl ExactMatchCredentialValidator {
    /// Build a validator that accepts only `expected` and rejects everything else.
    pub fn new(mut expected: Credentials) -> Self {
        if let Some(d) = &expected.domain {
            if d.trim().is_empty() {
                expected.domain = None;
            }
        }
        Self { expected }
    }
}

#[async_trait::async_trait]
impl CredentialValidator for ExactMatchCredentialValidator {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        // 1. Password check
        if credentials.password != self.expected.password {
            return Ok(CredentialDecision::Reject);
        }

        // 2. Normalize and extract potential domain embedded in username (e.g. DOMAIN\username or username@DOMAIN)
        let (client_dom_from_user, clean_username) =
            if let Some((dom, user)) = credentials.username.split_once('\\') {
                (Some(dom), user)
            } else if let Some((user, dom)) = credentials.username.split_once('@') {
                (Some(dom), user)
            } else {
                (None, credentials.username.as_str())
            };

        // 3. Username check (allow exact match or case-insensitive match)
        let username_matches = clean_username == self.expected.username
            || clean_username.eq_ignore_ascii_case(&self.expected.username);

        if !username_matches {
            return Ok(CredentialDecision::Reject);
        }

        // 4. Domain check
        // Determine client domain if any (ignore empty, whitespace, or local dot ".")
        let effective_client_domain = credentials
            .domain
            .as_deref()
            .or(client_dom_from_user)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && *s != ".");

        match (&self.expected.domain, effective_client_domain) {
            // When server does not require / configure a domain:
            // Accept connection regardless of client domain (None, empty, ".", "WORKGROUP", etc.)
            (None, _) => Ok(CredentialDecision::Accept),

            // When server specifies a domain:
            (Some(expected_dom), Some(client_dom)) => {
                if expected_dom.trim().eq_ignore_ascii_case(client_dom) {
                    Ok(CredentialDecision::Accept)
                } else {
                    Ok(CredentialDecision::Reject)
                }
            }

            // Server expects domain, but client did not provide one
            (Some(_), None) => Ok(CredentialDecision::Reject),
        }
    }
}

#[derive(Clone)]
#[non_exhaustive]
pub struct RdpServerOptions {
    pub addr: SocketAddr,
    pub security: RdpServerSecurity,
    pub codecs: BitmapCodecs,
    pub max_request_size: u32,
    /// When `true`, each connection's acceptor adopts the desktop size the
    /// client requests in its Client Core Data (instead of the size reported
    /// by the display handler), negotiating that size from the start without a
    /// Deactivation-Reactivation resize. Defaults to `false`. Set via
    /// [`RdpServerBuilder::with_honor_client_desktop_size`](crate::RdpServerBuilder::with_honor_client_desktop_size).
    pub honor_client_desktop_size: bool,
}

impl RdpServerOptions {
    /// Default [MultifragmentUpdate] max reassembly buffer size (8 MB).
    ///
    /// Advertised to the client during capability exchange as the largest
    /// reassembled Fast-Path Update the server can accept.
    /// Values that are too large cause certain clients (notably mstsc)
    /// to reject the connection.
    ///
    /// [MultifragmentUpdate]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/01717954-716a-424d-af35-28fb2b86df89
    pub(crate) const DEFAULT_MAX_REQUEST_SIZE: u32 = 8 * 1024 * 1024;

    fn has_image_remote_fx(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::ImageRemoteFx(_)))
    }

    fn has_remote_fx(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::RemoteFx(_)))
    }

    #[cfg(feature = "qoi")]
    fn has_qoi(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::Qoi))
    }

    #[cfg(feature = "qoiz")]
    fn has_qoiz(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::QoiZ))
    }

    #[cfg(feature = "nscodec")]
    fn has_nscodec(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::NsCodec(_)))
    }
}

#[derive(Clone)]
pub enum RdpServerSecurity {
    None,
    Tls(TlsAcceptor),
    /// Used for both hybrid + hybrid-ex.
    Hybrid((TlsAcceptor, Vec<u8>)),
}

impl RdpServerSecurity {
    pub fn flag(&self) -> nego::SecurityProtocol {
        match self {
            RdpServerSecurity::None => nego::SecurityProtocol::empty(),
            RdpServerSecurity::Tls(_) => nego::SecurityProtocol::SSL,
            RdpServerSecurity::Hybrid(_) => {
                nego::SecurityProtocol::HYBRID | nego::SecurityProtocol::HYBRID_EX
            }
        }
    }
}

struct AInputHandler {
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
}

impl_as_any!(AInputHandler);

impl dvc::DvcProcessor for AInputHandler {
    fn channel_name(&self) -> &str {
        ironrdp_ainput::CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<dvc::DvcMessage>> {
        use ironrdp_ainput::{ServerPdu, VersionPdu};

        let pdu = ServerPdu::Version(VersionPdu::default());

        Ok(vec![Box::new(pdu)])
    }

    fn close(&mut self, _channel_id: u32) {}

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<dvc::DvcMessage>> {
        use ironrdp_ainput::ClientPdu;

        match decode(payload).map_err(|e| decode_err!(e))? {
            ClientPdu::Mouse(pdu) => {
                let handler = Arc::clone(&self.handler);
                task::spawn_blocking(move || {
                    handler.blocking_lock().mouse(pdu.into());
                });
            }
        }

        Ok(Vec::new())
    }
}

impl dvc::DvcServerProcessor for AInputHandler {}

struct DisplayControlBackend {
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
}

impl DisplayControlBackend {
    fn new(display: Arc<Mutex<Box<dyn RdpServerDisplay>>>) -> Self {
        Self { display }
    }
}

impl DisplayControlHandler for DisplayControlBackend {
    fn monitor_layout(&self, layout: DisplayControlMonitorLayout) {
        let display = Arc::clone(&self.display);
        task::spawn_blocking(move || display.blocking_lock().request_layout(layout));
    }
}

/// Selects who performs the TLS handshake for a connection accepted via
/// [`RdpServer::run_connection_with`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum TransportTls {
    /// IronRDP performs the TLS accept on the stream (standard TCP+TLS).
    Managed,
    /// The stream is already past TLS, terminated by a lower layer (e.g. a WSS
    /// terminator). IronRDP skips the TLS handshake. The caller MUST guarantee
    /// the transport is already encrypted; see the preconditions on
    /// [`RdpServer::run_connection_with`].
    AlreadyDone,
}

/// RDP Server
///
/// A server is created to listen for connections.
/// After the connection sequence is finalized using the provided security mechanism, the server can:
///  - receive display updates from a [`RdpServerDisplay`] and forward them to the client
///  - receive input events from a client and forward them to an [`RdpServerInputHandler`]
///
/// # Example
///
/// ```
/// use ironrdp_server::{RdpServer, RdpServerInputHandler, RdpServerDisplay, RdpServerDisplayUpdates};
///
///# use anyhow::Result;
///# use ironrdp_server::{DisplayUpdate, DesktopSize, KeyboardEvent, MouseEvent};
///# use tokio_rustls::TlsAcceptor;
///# struct NoopInputHandler;
///# impl RdpServerInputHandler for NoopInputHandler {
///#     fn keyboard(&mut self, _: KeyboardEvent) {}
///#     fn mouse(&mut self, _: MouseEvent) {}
///# }
///# struct NoopDisplay;
///# #[async_trait::async_trait]
///# impl RdpServerDisplay for NoopDisplay {
///#     async fn size(&mut self) -> DesktopSize {
///#         todo!()
///#     }
///#     async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
///#         todo!()
///#     }
///# }
///# async fn stub() -> Result<()> {
/// fn make_tls_acceptor() -> TlsAcceptor {
///    /* snip */
///#    todo!()
/// }
///
/// fn make_input_handler() -> impl RdpServerInputHandler {
///    /* snip */
///#    NoopInputHandler
/// }
///
/// fn make_display_handler() -> impl RdpServerDisplay {
///    /* snip */
///#    NoopDisplay
/// }
///
/// let tls_acceptor = make_tls_acceptor();
/// let input_handler = make_input_handler();
/// let display_handler = make_display_handler();
///
/// let mut server = RdpServer::builder()
///     .with_addr(([127, 0, 0, 1], 3389))
///     .with_tls(tls_acceptor)
///     .with_input_handler(input_handler)
///     .with_display_handler(display_handler)
///     .build();
///
/// server.run().await;
/// Ok(())
///# }
/// ```
pub struct RdpServer {
    opts: RdpServerOptions,
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
    sound_factory: Option<Arc<dyn SoundServerFactory>>,
    cliprdr_factory: Option<Arc<dyn CliprdrServerFactory>>,
    echo_handle: EchoServerHandle,
    #[cfg(feature = "egfx")]
    gfx_factory: Option<Arc<dyn GfxServerFactory>>,
    #[cfg(feature = "egfx")]
    gfx_handle: Option<crate::gfx::GfxServerHandle>,
    ev_sender: mpsc::UnboundedSender<ServerEvent>,
    ev_receiver: Arc<Mutex<mpsc::UnboundedReceiver<ServerEvent>>>,
    creds: Option<Credentials>,
    credential_validator: Option<Arc<dyn CredentialValidator>>,
    local_addr: Option<SocketAddr>,
    autodetect: Option<AutoDetectManager>,
    connection_handler: Option<Arc<Mutex<Box<dyn ConnectionHandler>>>>,
    display_suppressed: Arc<AtomicBool>,
    autodetect_rtt: Arc<AtomicU32>,
}

pub struct RdpServerSession {
    opts: RdpServerOptions,
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
    static_channels: StaticChannelSet,
    sound_factory: Option<Arc<dyn SoundServerFactory>>,
    cliprdr_factory: Option<Arc<dyn CliprdrServerFactory>>,
    echo_handle: EchoServerHandle,
    #[cfg(feature = "egfx")]
    gfx_factory: Option<Arc<dyn GfxServerFactory>>,
    #[cfg(feature = "egfx")]
    gfx_handle: Option<crate::gfx::GfxServerHandle>,
    ev_sender: mpsc::UnboundedSender<ServerEvent>,
    ev_receiver: Arc<Mutex<mpsc::UnboundedReceiver<ServerEvent>>>,
    creds: Option<Credentials>,
    credential_validator: Option<Arc<dyn CredentialValidator>>,
    local_addr: Option<SocketAddr>,
    autodetect: Option<AutoDetectManager>,
    display_suppressed: Arc<AtomicBool>,
    autodetect_rtt: Arc<AtomicU32>,
}

#[derive(Debug)]
pub enum ServerEvent {
    Quit(String),
    Clipboard(ClipboardMessage),
    Rdpsnd(RdpsndServerMessage),
    Echo(EchoServerMessage),
    SetCredentials(Credentials),
    GetLocalAddr(oneshot::Sender<Option<SocketAddr>>),
    #[cfg(feature = "egfx")]
    Egfx(EgfxServerMessage),
    /// Trigger an RTT measurement probe (requires auto-detect enabled).
    AutoDetectRttRequest,
}

pub trait ServerEventSender {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>);
}

impl ServerEvent {
    pub fn create_channel() -> (mpsc::UnboundedSender<Self>, mpsc::UnboundedReceiver<Self>) {
        mpsc::unbounded_channel()
    }
}

#[derive(Debug, PartialEq)]
enum RunState {
    Continue,
    Disconnect,
    DeactivationReactivation { desktop_size: DesktopSize },
}

impl RdpServer {
    #[expect(
        clippy::too_many_arguments,
        reason = "called via the builder; positional parameters are an internal detail"
    )]
    pub(crate) fn new(
        opts: RdpServerOptions,
        handler: Box<dyn RdpServerInputHandler>,
        display: Box<dyn RdpServerDisplay>,
        mut sound_factory: Option<Box<dyn SoundServerFactory>>,
        mut cliprdr_factory: Option<Box<dyn CliprdrServerFactory>>,
        connection_handler: Option<Box<dyn ConnectionHandler>>,
        #[cfg(feature = "egfx")] mut gfx_factory: Option<Box<dyn GfxServerFactory>>,
        display_suppressed: Option<Arc<AtomicBool>>,
        autodetect_rtt: Option<Arc<AtomicU32>>,
    ) -> Self {
        let (ev_sender, ev_receiver) = ServerEvent::create_channel();
        if let Some(cliprdr) = cliprdr_factory.as_mut() {
            cliprdr.set_sender(ev_sender.clone());
        }
        if let Some(snd) = sound_factory.as_mut() {
            snd.set_sender(ev_sender.clone());
        }
        #[cfg(feature = "egfx")]
        if let Some(gfx) = gfx_factory.as_mut() {
            gfx.set_sender(ev_sender.clone());
        }
        Self {
            opts,
            handler: Arc::new(Mutex::new(handler)),
            display: Arc::new(Mutex::new(display)),
            sound_factory: sound_factory.map(Arc::from),
            cliprdr_factory: cliprdr_factory.map(Arc::from),
            echo_handle: EchoServerHandle::new(ev_sender.clone()),
            #[cfg(feature = "egfx")]
            gfx_factory: gfx_factory.map(Arc::from),
            #[cfg(feature = "egfx")]
            gfx_handle: None,
            ev_sender,
            ev_receiver: Arc::new(Mutex::new(ev_receiver)),
            creds: None,
            credential_validator: None,
            local_addr: None,
            autodetect: None,
            connection_handler: connection_handler.map(|h| Arc::new(Mutex::new(h))),
            display_suppressed: display_suppressed
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
            autodetect_rtt: {
                // Reset to the sentinel: an injected handle must not expose a stale value before the first measurement.
                let handle = autodetect_rtt.unwrap_or_else(|| Arc::new(AtomicU32::new(u32::MAX)));
                handle.store(u32::MAX, Ordering::Relaxed);
                handle
            },
        }
    }

    pub fn builder() -> builder::RdpServerBuilder<builder::WantsAddr> {
        builder::RdpServerBuilder::new()
    }

    /// Set or clear the credential validator for TLS-mode connections.
    pub fn set_credential_validator(&mut self, validator: Option<Arc<dyn CredentialValidator>>) {
        self.credential_validator = validator;
    }

    pub fn event_sender(&self) -> &mpsc::UnboundedSender<ServerEvent> {
        &self.ev_sender
    }

    pub fn display_suppressed_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.display_suppressed)
    }

    pub fn autodetect_rtt_handle(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.autodetect_rtt)
    }

    pub fn echo_handle(&self) -> &EchoServerHandle {
        &self.echo_handle
    }

    pub fn enable_autodetect(&mut self) {
        self.autodetect = Some(AutoDetectManager::new());
    }

    pub fn rtt_snapshot(&self) -> Option<RttSnapshot> {
        self.autodetect.as_ref().and_then(|ad| ad.snapshot())
    }

    #[cfg(feature = "egfx")]
    pub fn gfx_handle(&self) -> Option<&crate::gfx::GfxServerHandle> {
        self.gfx_handle.as_ref()
    }

    pub fn set_credentials(&mut self, creds: Option<Credentials>) {
        debug!(?creds, "Changing credentials");
        self.creds = creds;
    }

    pub fn create_session(&self) -> RdpServerSession {
        let (ev_sender, ev_receiver) = ServerEvent::create_channel();
        RdpServerSession {
            opts: self.opts.clone(),
            handler: Arc::clone(&self.handler),
            display: Arc::clone(&self.display),
            static_channels: StaticChannelSet::new(),
            sound_factory: self.sound_factory.clone(),
            cliprdr_factory: self.cliprdr_factory.clone(),
            echo_handle: EchoServerHandle::new(ev_sender.clone()),
            #[cfg(feature = "egfx")]
            gfx_factory: self.gfx_factory.clone(),
            #[cfg(feature = "egfx")]
            gfx_handle: None,
            ev_sender,
            ev_receiver: Arc::new(Mutex::new(ev_receiver)),
            creds: self.creds.clone(),
            credential_validator: self.credential_validator.clone(),
            local_addr: self.local_addr,
            autodetect: None,
            display_suppressed: Arc::new(AtomicBool::new(false)),
            autodetect_rtt: Arc::new(AtomicU32::new(u32::MAX)),
        }
    }

    pub async fn run_connection<S>(&mut self, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
    {
        self.create_session().run_connection(stream).await
    }

    pub async fn run_connection_with<S>(&mut self, stream: S, tls: TransportTls) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
    {
        self.create_session().run_connection_with(stream, tls).await
    }

    pub async fn run(&mut self) -> Result<()> {
        let socket = match self.opts.addr {
            SocketAddr::V4(_) => TcpSocket::new_v4().context("create IPv4 socket")?,
            SocketAddr::V6(_) => TcpSocket::new_v6().context("create IPv6 socket")?,
        };

        #[cfg(unix)]
        socket.set_reuseaddr(true).context("set SO_REUSEADDR")?;

        socket.bind(self.opts.addr).context("bind listen address")?;

        let listener = socket.listen(LISTENER_BACKLOG).context("start listener")?;
        let local_addr = listener.local_addr()?;

        debug!("Listening for connections on {local_addr}");
        self.local_addr = Some(local_addr);

        let connection_handler = self.connection_handler.clone();

        loop {
            let ev_receiver = Arc::clone(&self.ev_receiver);
            let mut ev_receiver = ev_receiver.lock().await;
            tokio::select! {
                Some(event) = ev_receiver.recv() => {
                    match event {
                        ServerEvent::Quit(reason) => {
                            debug!("Got quit event {reason}");
                            break;
                        }
                        ServerEvent::GetLocalAddr(tx) => {
                            let _ = tx.send(self.local_addr);
                        }
                        ServerEvent::SetCredentials(creds) => {
                            self.set_credentials(Some(creds));
                        }
                        ev => {
                            debug!("Unexpected event {:?}", ev);
                        }
                    }
                },
                Ok((stream, peer)) = listener.accept() => {
                    let _ = stream.set_nodelay(true);
                    info!(?peer, "🚀 [MULTI-CLIENT] Received incoming RDP connection");
                    drop(ev_receiver);

                    let accepted = if let Some(ref handler) = connection_handler {
                        handler.lock().await.on_accept(peer)
                    } else {
                        true
                    };

                    if !accepted {
                        warn!(?peer, "Connection rejected by handler");
                        drop(stream);
                    } else {
                        let mut session = self.create_session();
                        let handler_clone = connection_handler.clone();
                        tokio::spawn(async move {
                            info!(?peer, "⚡ [SESSION STARTED] Concurrent RDP session running for client");
                            let started = tokio::time::Instant::now();
                            let result = session.run_connection(stream).await;
                            let duration = started.elapsed();

                            if let Err(ref error) = result {
                                error!(?peer, ?error, "Connection error");
                            } else {
                                info!(?peer, ?duration, "Connection disconnected cleanly");
                            }

                            if let Some(handler) = handler_clone {
                                let mut guard = handler.lock().await;
                                let err = result.as_ref().err();
                                let _action = guard.on_disconnected(
                                    peer,
                                    duration,
                                    err,
                                );
                            }
                        });
                    }
                }
                else => break,
            }
        }

        Ok(())
    }
}

impl RdpServerSession {
    fn attach_channels(&mut self, acceptor: &mut Acceptor) {
        if let Some(cliprdr_factory) = self.cliprdr_factory.as_deref() {
            let backend = cliprdr_factory.build_cliprdr_backend();

            let cliprdr = CliprdrServer::new(backend);

            acceptor.attach_static_channel(cliprdr);
        }

        if let Some(factory) = self.sound_factory.as_deref() {
            let backend = factory.build_backend();

            acceptor.attach_static_channel(RdpsndServer::new(backend));
        }

        let dcs_backend = DisplayControlBackend::new(Arc::clone(&self.display));
        let dvc = dvc::DrdynvcServer::new()
            .with_dynamic_channel(AInputHandler {
                handler: Arc::clone(&self.handler),
            })
            .with_dynamic_channel(DisplayControlServer::new(Box::new(dcs_backend)));

        let dvc = {
            let echo_handle = self.echo_handle.clone();
            dvc.with_dynamic_channel(EchoDvcBridge::new(echo_handle))
        };

        #[cfg(feature = "egfx")]
        let dvc = {
            let mut dvc = dvc;
            if let Some(gfx_factory) = self.gfx_factory.as_deref() {
                if let Some((bridge, handle)) = gfx_factory.build_server_with_handle() {
                    self.gfx_handle = Some(handle);
                    dvc = dvc.with_dynamic_channel(bridge);
                } else {
                    let handler = gfx_factory.build_gfx_handler();
                    let gfx_server = ironrdp_egfx::server::GraphicsPipelineServer::new(handler);
                    dvc = dvc.with_dynamic_channel(gfx_server);
                }
            }
            dvc
        };

        acceptor.attach_static_channel(dvc);
    }

    pub fn event_sender(&self) -> &mpsc::UnboundedSender<ServerEvent> {
        &self.ev_sender
    }

    pub async fn run_connection<S>(&mut self, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
    {
        self.run_connection_with(stream, TransportTls::Managed)
            .await
    }

    pub async fn run_connection_with<S>(&mut self, stream: S, tls: TransportTls) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
    {
        self.display_suppressed.store(false, Ordering::Relaxed);

        let framed = TokioFramed::new(stream);

        let size = self.display.lock().await.size().await;
        let capabilities = capabilities::capabilities(&self.opts, size);
        let mut acceptor = Acceptor::new(
            self.opts.security.flag(),
            size,
            capabilities,
            self.creds.clone(),
        );
        acceptor.set_honor_client_desktop_size(self.opts.honor_client_desktop_size);

        self.attach_channels(&mut acceptor);

        let res = ironrdp_acceptor::accept_begin(framed, &mut acceptor)
            .await
            .context("accept_begin failed")?;

        match res {
            BeginResult::ShouldUpgrade(stream) => match tls {
                TransportTls::Managed => {
                    let tls_acceptor = match &self.opts.security {
                        RdpServerSecurity::Tls(acceptor) => acceptor,
                        RdpServerSecurity::Hybrid((acceptor, _)) => acceptor,
                        RdpServerSecurity::None => unreachable!(),
                    };
                    let accept = match tls_acceptor.accept(stream).await {
                        Ok(accept) => accept,
                        Err(e) => {
                            warn!("Failed to TLS accept: {}", e);
                            return Ok(());
                        }
                    };
                    self.finalize_after_upgrade(
                        TokioFramed::new(accept),
                        acceptor,
                        "TLS connection",
                    )
                    .await?;
                }
                TransportTls::AlreadyDone => {
                    self.finalize_after_upgrade(
                        TokioFramed::new(stream),
                        acceptor,
                        "TLS-offloaded stream",
                    )
                    .await?;
                }
            },

            BeginResult::Continue(framed) => {
                self.accept_finalize(framed, acceptor).await?;
            }
        };

        Ok(())
    }

    async fn finalize_after_upgrade<S>(
        &mut self,
        mut framed: TokioFramed<S>,
        mut acceptor: Acceptor,
        shutdown_label: &str,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Sync + Send + Unpin + 'static,
    {
        acceptor.mark_security_upgrade_as_done();

        if let RdpServerSecurity::Hybrid((_, pub_key)) = &self.opts.security {
            let client_name = "rdp-client".to_owned();

            ironrdp_acceptor::accept_credssp(
                &mut framed,
                &mut acceptor,
                &mut ironrdp_tokio::reqwest::ReqwestNetworkClient::new(),
                client_name.into(),
                pub_key.clone(),
                None,
            )
            .await?;
        }

        let framed = self.accept_finalize(framed, acceptor).await?;
        debug!("Shutting down {}", shutdown_label);
        let (mut inner, _) = framed.into_inner();
        if let Err(e) = inner.shutdown().await {
            debug!(?e, "{} shutdown error", shutdown_label);
        }

        Ok(())
    }

    pub fn get_svc_processor<T: SvcProcessor + 'static>(&mut self) -> Option<&mut T> {
        self.static_channels
            .get_by_type_mut::<T>()
            .and_then(|svc| svc.channel_processor_downcast_mut())
    }

    pub fn get_channel_id_by_type<T: SvcProcessor + 'static>(&self) -> Option<StaticChannelId> {
        self.static_channels.get_channel_id_by_type::<T>()
    }

    async fn dispatch_pdu(
        &mut self,
        action: Action,
        bytes: bytes::BytesMut,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
        message_channel_id: Option<u16>,
    ) -> Result<RunState> {
        match action {
            Action::FastPath => {
                let input = decode(&bytes)?;
                self.handle_fastpath(input).await;
            }

            Action::X224 => {
                if self
                    .handle_x224(
                        writer,
                        io_channel_id,
                        user_channel_id,
                        message_channel_id,
                        &bytes,
                    )
                    .await
                    .context("X224 input error")?
                {
                    debug!("Got disconnect request");
                    return Ok(RunState::Disconnect);
                }
            }
        }

        Ok(RunState::Continue)
    }

    async fn dispatch_display_update(
        update: DisplayUpdate,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
        io_channel_id: u16,
        buffer: &mut Vec<u8>,
        mut encoder: UpdateEncoder,
    ) -> Result<(RunState, UpdateEncoder)> {
        if let DisplayUpdate::Resize(desktop_size) = update {
            debug!(?desktop_size, "Display resize");
            encoder.set_desktop_size(desktop_size);
            deactivate_all(io_channel_id, user_channel_id, writer).await?;
            return Ok((RunState::DeactivationReactivation { desktop_size }, encoder));
        }

        let mut encoder_iter = encoder.update(update);
        loop {
            let Some(fragmenter) = encoder_iter.next().await else {
                break;
            };

            let mut fragmenter = fragmenter.context("error while encoding")?;
            if fragmenter.size_hint() > buffer.len() {
                buffer.resize(fragmenter.size_hint(), 0);
            }

            while let Some(len) = fragmenter.next(buffer) {
                writer
                    .write_all(&buffer[..len])
                    .await
                    .context("failed to write display update")?;
            }
        }

        Ok((RunState::Continue, encoder))
    }

    async fn dispatch_server_events(
        &mut self,
        events: &mut Vec<ServerEvent>,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
        message_channel_id: Option<u16>,
    ) -> Result<RunState> {
        // Avoid wave messages queuing up and causing extra delay. When a
        // batch carries more than `WAVE_KEEP` waves, drop the OLDEST ones
        // and keep the most recent — playing stale audio just bakes the
        // latency in permanently, so a one-time dispatch stall (e.g. a video
        // encode holding the server lock) would otherwise become a permanent
        // audio offset.
        //
        // This is still a naive solution; better long-term: compute the
        // actual delay, add IO priority, encode audio, use UDP, etc. 4 frames
        // is roughly low hundreds of ms in regular setups.
        const WAVE_KEEP: usize = 4;
        let wave_total = events
            .iter()
            .filter(|e| matches!(e, ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(..))))
            .count();
        let mut wave_skip = wave_total.saturating_sub(WAVE_KEEP);
        for event in events.drain(..) {
            trace!(?event, "Dispatching");
            match event {
                ServerEvent::Quit(reason) => {
                    debug!("Got quit event: {reason}");
                    return Ok(RunState::Disconnect);
                }
                ServerEvent::GetLocalAddr(tx) => {
                    let _ = tx.send(self.local_addr);
                }
                ServerEvent::SetCredentials(creds) => {
                    self.set_credentials(Some(creds));
                }
                ServerEvent::Rdpsnd(s) => {
                    let Some(rdpsnd) = self.get_svc_processor::<RdpsndServer>() else {
                        warn!("No rdpsnd channel, dropping event");
                        continue;
                    };
                    let msgs = match s {
                        RdpsndServerMessage::Wave(data, ts) => {
                            if wave_skip > 0 {
                                wave_skip -= 1;
                                debug!("Dropping stale wave");
                                continue;
                            }
                            rdpsnd.wave(data, ts)
                        }
                        RdpsndServerMessage::SetVolume { left, right } => {
                            rdpsnd.set_volume(left, right)
                        }
                        RdpsndServerMessage::Close => rdpsnd.close(),
                        RdpsndServerMessage::Error(error) => {
                            error!(?error, "Handling rdpsnd event");
                            continue;
                        }
                    }
                    .context("failed to send rdpsnd event")?;
                    let channel_id = self
                        .get_channel_id_by_type::<RdpsndServer>()
                        .context("SVC channel not found")?;
                    let data =
                        server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
                ServerEvent::Clipboard(c) => {
                    let Some(cliprdr) = self.get_svc_processor::<CliprdrServer>() else {
                        warn!("No clipboard channel, dropping event");
                        continue;
                    };
                    let msgs = match c {
                        ClipboardMessage::SendInitiateCopy(formats) => {
                            cliprdr.initiate_copy(&formats)
                        }
                        ClipboardMessage::SendInitiateFileCopy(files) => {
                            cliprdr.initiate_file_copy(files)
                        }
                        ClipboardMessage::SendFormatData(data) => cliprdr.submit_format_data(data),
                        ClipboardMessage::SendInitiatePaste(format) => {
                            cliprdr.initiate_paste(format)
                        }
                        ClipboardMessage::SendFileContentsRequest(request) => {
                            cliprdr.request_file_contents(request)
                        }
                        ClipboardMessage::SendFileContentsResponse(response) => {
                            cliprdr.submit_file_contents(response)
                        }
                        ClipboardMessage::Error(error) => {
                            error!(?error, "Handling clipboard event");
                            continue;
                        }
                    }
                    .context("failed to send clipboard event")?;
                    let channel_id = self
                        .get_channel_id_by_type::<CliprdrServer>()
                        .context("SVC channel not found")?;
                    let data =
                        server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
                ServerEvent::Echo(msg) => match msg {
                    EchoServerMessage::SendRequest { payload } => {
                        let Some(drdynvc) = self.get_svc_processor::<dvc::DrdynvcServer>() else {
                            warn!("No drdynvc channel, dropping ECHO request");
                            continue;
                        };

                        let Some(echo_channel_id) =
                            drdynvc.get_channel_id_by_type::<EchoDvcBridge>()
                        else {
                            warn!("No ECHO dynamic channel, dropping ECHO request");
                            continue;
                        };

                        if !drdynvc.is_channel_opened(echo_channel_id) {
                            warn!("ECHO dynamic channel not yet opened, dropping ECHO request");
                            continue;
                        }

                        self.echo_handle.on_request_sent(&payload);

                        let request = build_echo_request(payload)?;
                        let messages = dvc::encode_dvc_messages(
                            echo_channel_id,
                            vec![request],
                            ChannelFlags::SHOW_PROTOCOL,
                        )?;

                        let drdynvc_channel_id = self
                            .get_channel_id_by_type::<dvc::DrdynvcServer>()
                            .context("DRDYNVC channel not found")?;

                        let data = server_encode_svc_messages(
                            messages,
                            drdynvc_channel_id,
                            user_channel_id,
                        )?;
                        writer.write_all(&data).await?;
                    }
                },
                #[cfg(feature = "egfx")]
                ServerEvent::Egfx(msg) => match msg {
                    EgfxServerMessage::SendMessages { messages } => {
                        let drdynvc_channel_id = self
                            .get_channel_id_by_type::<dvc::DrdynvcServer>()
                            .context("DRDYNVC channel not found")?;
                        let data = server_encode_svc_messages(
                            messages,
                            drdynvc_channel_id,
                            user_channel_id,
                        )?;
                        writer.write_all(&data).await?;
                    }
                },
                ServerEvent::AutoDetectRttRequest => {
                    // Auto-detect requests ride the MCS message channel
                    // ([MS-RDPBCGR] 2.2.14.3). With none negotiated (the client
                    // did not request it), there is nowhere to send them.
                    if let (Some(ad), Some(message_channel_id)) =
                        (self.autodetect.as_mut(), message_channel_id)
                    {
                        ad.expire_stale_probes(crate::autodetect::RTT_PROBE_MAX_AGE);
                        let request = ad.send_rtt_request();
                        let data = encode_autodetect_request(
                            request,
                            message_channel_id,
                            user_channel_id,
                        )?;
                        writer.write_all(&data).await?;
                    }
                }
            }
        }

        Ok(RunState::Continue)
    }

    async fn client_loop<R, W>(
        &mut self,
        reader: &mut Framed<R>,
        writer: &mut Framed<W>,
        io_channel_id: u16,
        user_channel_id: u16,
        message_channel_id: Option<u16>,
        mut encoder: UpdateEncoder,
    ) -> Result<RunState>
    where
        R: FramedRead + Send + 'static,
        W: FramedWrite + Send + 'static,
    {
        debug!("Starting client loop");
        let mut display_updates = self.display.lock().await.updates().await?;
        let mut buffer = vec![0u8; 4096];
        let mut events = Vec::with_capacity(100);
        let ev_receiver = Arc::clone(&self.ev_receiver);
        let mut ev_receiver = ev_receiver.lock().await;

        let state = loop {
            tokio::select! {
                pdu_res = reader.read_pdu() => {
                    let (action, bytes) = pdu_res?;
                    match self
                        .dispatch_pdu(
                            action,
                            bytes,
                            writer,
                            io_channel_id,
                            user_channel_id,
                            message_channel_id,
                        )
                        .await?
                    {
                        RunState::Continue => continue,
                        state => break state,
                    }
                }
                update_res = display_updates.next_update() => {
                    match update_res {
                        Ok(Some(update)) => {
                            match Self::dispatch_display_update(
                                update,
                                writer,
                                user_channel_id,
                                io_channel_id,
                                &mut buffer,
                                encoder,
                            )
                            .await?
                            {
                                (RunState::Continue, enc) => {
                                    encoder = enc;
                                    continue;
                                }
                                (state, _) => break state,
                            }
                        }
                        Ok(None) => break RunState::Disconnect,
                        Err(error) => {
                            warn!(error = format!("{error:#}"), "next_update failed");
                        }
                    }
                }
                nevents = ev_receiver.recv_many(&mut events, 100) => {
                    if nevents == 0 {
                        debug!("No server events.. stopping");
                        break RunState::Disconnect;
                    }
                    while let Ok(ev) = ev_receiver.try_recv() {
                        events.push(ev);
                    }
                    match self
                        .dispatch_server_events(
                            &mut events,
                            writer,
                            user_channel_id,
                            message_channel_id,
                        )
                        .await?
                    {
                        RunState::Continue => continue,
                        state => break state,
                    }
                }
            }
        };

        debug!("End of client loop: {state:?}");
        Ok(state)
    }

    async fn client_accepted<R, W>(
        &mut self,
        reader: &mut Framed<R>,
        writer: &mut Framed<W>,
        result: AcceptorResult,
    ) -> Result<RunState>
    where
        R: FramedRead + Send + 'static,
        W: FramedWrite + Send + 'static,
    {
        debug!("Client accepted");

        // Validate credentials if a validator is configured. The validator runs here, in the
        // async server layer, rather than in the sans-I/O acceptor, because real validators
        // (PAM/LDAP/DB) are I/O-bound. On rejection, deny with a ServerSetErrorInfoPdu before
        // closing, matching the acceptor's exact-match denial path.
        if let Some(validator) = self.credential_validator.clone() {
            if let Some(creds) = &result.credentials {
                match validator.validate(creds).await {
                    Ok(CredentialDecision::Accept) => {
                        debug!("Credential validation accepted");
                    }
                    Ok(CredentialDecision::Reject) => {
                        warn!("Credential validation rejected");
                        send_access_denied(result.io_channel_id, result.user_channel_id, writer)
                            .await?;
                        bail!("credential validation rejected");
                    }
                    Err(e) => {
                        error!(error = %e, "Credential validator backend error");
                        send_access_denied(result.io_channel_id, result.user_channel_id, writer)
                            .await?;
                        bail!("credential validation backend error");
                    }
                }
            } else {
                debug!("Skipping credential validation (no credentials in AcceptorResult)");
            }
        }

        if !result.input_events.is_empty() {
            debug!("Handling input event backlog from acceptor sequence");
            self.handle_input_backlog(
                writer,
                result.io_channel_id,
                result.user_channel_id,
                result.message_channel_id,
                result.input_events,
            )
            .await?;
        }

        self.static_channels = result.static_channels;
        if !result.reactivation {
            for (_type_id, channel, channel_id) in self.static_channels.iter_mut() {
                debug!(?channel, ?channel_id, "Start");
                let Some(channel_id) = channel_id else {
                    continue;
                };
                let svc_responses = channel.start()?;
                let response =
                    server_encode_svc_messages(svc_responses, channel_id, result.user_channel_id)?;
                writer.write_all(&response).await?;
            }
        }

        let mut update_codecs = UpdateEncoderCodecs::new();
        let mut surface_flags = CmdFlags::empty();
        for c in result.capabilities {
            match c {
                CapabilitySet::General(c) => {
                    let fastpath = c
                        .extra_flags
                        .contains(GeneralExtraFlags::FASTPATH_OUTPUT_SUPPORTED);
                    if !fastpath {
                        bail!("Fastpath output not supported!");
                    }
                }
                CapabilitySet::Bitmap(b) => {
                    if !b.desktop_resize_flag {
                        debug!("Desktop resize is not supported by the client");
                        continue;
                    }

                    let client_size = DesktopSize {
                        width: b.desktop_width,
                        height: b.desktop_height,
                    };
                    let display_size = self
                        .display
                        .lock()
                        .await
                        .request_initial_size(client_size)
                        .await;

                    // It's problematic when the client didn't resize, as we send bitmap updates that don't fit.
                    // The client will likely drop the connection.
                    if client_size.width < display_size.width
                        || client_size.height < display_size.height
                    {
                        // TODO: we may have different behaviour instead, such as clipping or scaling?
                        warn!(
                            "Client size doesn't fit the server size: {:?} < {:?}",
                            client_size, display_size
                        );
                    }
                }
                CapabilitySet::SurfaceCommands(c) => {
                    surface_flags = c.flags;
                }
                CapabilitySet::BitmapCodecs(BitmapCodecs(codecs)) => {
                    for codec in codecs {
                        match codec.property {
                            // FIXME: The encoder operates in image mode only.
                            //
                            // See [MS-RDPRFX] 3.1.1.1 "State Machine" for
                            // implementation of the video mode. which allows to
                            // skip sending Header for each image.
                            //
                            // We should distinguish parameters for both modes,
                            // and somehow choose the "best", instead of picking
                            // the last parsed here.
                            CodecProperty::RemoteFx(
                                rdp::capability_sets::RemoteFxContainer::ClientContainer(c),
                            ) if self.opts.has_remote_fx() => {
                                for caps in c.caps_data.0.0 {
                                    update_codecs.set_remotefx(Some((caps.entropy_bits, codec.id)));
                                }
                            }
                            CodecProperty::ImageRemoteFx(
                                rdp::capability_sets::RemoteFxContainer::ClientContainer(c),
                            ) if self.opts.has_image_remote_fx() => {
                                for caps in c.caps_data.0.0 {
                                    update_codecs.set_remotefx(Some((caps.entropy_bits, codec.id)));
                                }
                            }
                            #[cfg(feature = "nscodec")]
                            CodecProperty::NsCodec(client_ns) if self.opts.has_nscodec() => {
                                // Re-use the client's confirmed color-loss
                                // level so the server encodes at the same
                                // shift the client decodes against.
                                update_codecs
                                    .set_nscodec(Some((codec.id, client_ns.color_loss_level)));
                            }
                            CodecProperty::NsCodec(_) => (),
                            #[cfg(feature = "qoi")]
                            CodecProperty::Qoi if self.opts.has_qoi() => {
                                update_codecs.set_qoi(Some(codec.id));
                            }
                            #[cfg(feature = "qoiz")]
                            CodecProperty::QoiZ if self.opts.has_qoiz() => {
                                update_codecs.set_qoiz(Some(codec.id));
                            }
                            _ => (),
                        }
                    }
                }
                _ => {}
            }
        }

        let desktop_size = self.display.lock().await.size().await;
        let encoder = UpdateEncoder::new(
            desktop_size,
            surface_flags,
            update_codecs,
            self.opts.max_request_size,
        )
        .context("failed to initialize update encoder")?;

        let state = self
            .client_loop(
                reader,
                writer,
                result.io_channel_id,
                result.user_channel_id,
                result.message_channel_id,
                encoder,
            )
            .await
            .context("client loop failure")?;

        Ok(state)
    }

    async fn handle_input_backlog(
        &mut self,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
        message_channel_id: Option<u16>,
        frames: Vec<Vec<u8>>,
    ) -> Result<()> {
        for frame in frames {
            match Action::from_fp_output_header(frame[0]) {
                Ok(Action::FastPath) => {
                    let input = decode(&frame)?;
                    self.handle_fastpath(input).await;
                }

                Ok(Action::X224) => {
                    let _ = self
                        .handle_x224(
                            writer,
                            io_channel_id,
                            user_channel_id,
                            message_channel_id,
                            &frame,
                        )
                        .await;
                }

                // the frame here is always valid, because otherwise it would
                // have failed during the acceptor loop
                Err(_) => unreachable!(),
            }
        }

        Ok(())
    }

    async fn handle_fastpath(&mut self, input: FastPathInput) {
        for event in input.input_events().iter().copied() {
            let mut handler = self.handler.lock().await;
            match event {
                FastPathInputEvent::KeyboardEvent(flags, key) => {
                    handler.keyboard((key, flags).into());
                }

                FastPathInputEvent::UnicodeKeyboardEvent(flags, key) => {
                    handler.keyboard((key, flags).into());
                }

                FastPathInputEvent::SyncEvent(flags) => {
                    handler.keyboard(flags.into());
                }

                FastPathInputEvent::MouseEvent(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::MouseEventEx(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::MouseEventRel(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::QoeEvent(quality) => {
                    warn!("Received QoE: {}", quality);
                }
            }
        }
    }

    async fn handle_io_channel_data(&mut self, data: SendDataRequest<'_>) -> Result<bool> {
        let control: rdp::headers::ShareControlHeader = decode(data.user_data.as_ref())?;

        match control.share_control_pdu {
            ShareControlPdu::Data(header) => match header.share_data_pdu {
                rdp::headers::ShareDataPdu::Input(pdu) => {
                    self.handle_input_event(pdu).await;
                }

                rdp::headers::ShareDataPdu::ShutdownRequest => {
                    return Ok(true);
                }

                // Client requests the server stop or resume sending display
                // updates. mstsc sends `desktop_rect: None` on minimize and
                // `desktop_rect: Some(rect)` on refocus. Without honoring
                // this, the server keeps streaming high-bitrate EGFX/H.264
                // frames into a minimized client; on refocus the client
                // must chew through the accumulated backlog before it can
                // present the current frame, locking up its input dispatch
                // for seconds. Flagging the shared `display_suppressed`
                // lets the display backend skip frame emission while it's
                // set.
                rdp::headers::ShareDataPdu::SuppressOutput(pdu) => {
                    let suppress = pdu.desktop_rect.is_none();
                    self.display_suppressed.store(suppress, Ordering::Relaxed);
                    debug!(suppress, "client suppress-output state changed");
                }

                // Client asks the server to redraw a rectangle — typical on
                // refocus after a minimize. Clear the suppress flag so the
                // backend resumes emission and treat this as "client wants
                // updates again." (The flag would also be cleared by the
                // `SuppressOutput { Some(rect) }` that usually accompanies
                // this; clearing here is belt-and-braces against clients
                // that send only one of the two.)
                rdp::headers::ShareDataPdu::RefreshRectangle(_) => {
                    if self.display_suppressed.swap(false, Ordering::Relaxed) {
                        debug!("client RefreshRectangle cleared suppress-output state");
                    }
                }

                unexpected => {
                    warn!(?unexpected, "Unexpected share data pdu");
                }
            },

            unexpected => {
                warn!(?unexpected, "Unexpected share control");
            }
        }

        Ok(false)
    }

    fn handle_message_channel_data(&mut self, data: SendDataRequest<'_>) {
        // The MCS message channel currently carries only the auto-detect
        // response. It is framed by a Basic Security Header (SEC_AUTODETECT_RSP),
        // not a Share Control header.
        match decode::<rdp::autodetect::AutoDetectRspPdu>(data.user_data.as_ref()) {
            Ok(pdu) => {
                if let Some(ref mut ad) = self.autodetect {
                    if let Some(rtt_ms) = ad.handle_response(&pdu.response) {
                        self.autodetect_rtt.store(rtt_ms, Ordering::Relaxed);
                        debug!(rtt_ms, seq = pdu.response.sequence_number(), "RTT measured");
                    } else {
                        trace!(
                            seq = pdu.response.sequence_number(),
                            "Unmatched auto-detect response"
                        );
                    }
                }
            }
            Err(error) => {
                warn!(
                    error = format!("{error:#}"),
                    "Unhandled MCS message channel PDU"
                );
            }
        }
    }

    async fn handle_x224(
        &mut self,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
        message_channel_id: Option<u16>,
        frame: &[u8],
    ) -> Result<bool> {
        let message = decode::<X224<mcs::McsMessage<'_>>>(frame)?;
        match message.0 {
            mcs::McsMessage::SendDataRequest(data) => {
                debug!(
                    initiator_id = data.initiator_id,
                    channel_id = data.channel_id,
                    user_data_len = data.user_data.len(),
                    "McsMessage::SendDataRequest"
                );
                if data.channel_id == io_channel_id {
                    return self.handle_io_channel_data(data).await;
                }

                if message_channel_id == Some(data.channel_id) {
                    self.handle_message_channel_data(data);
                    return Ok(false);
                }

                if let Some(svc) = self.static_channels.get_by_channel_id_mut(data.channel_id) {
                    let response_pdus = svc.process(&data.user_data)?;
                    let response = server_encode_svc_messages(
                        response_pdus,
                        data.channel_id,
                        user_channel_id,
                    )?;
                    writer.write_all(&response).await?;
                } else {
                    warn!(
                        channel_id = data.channel_id,
                        "Unexpected channel received: ID",
                    );
                }
            }

            mcs::McsMessage::DisconnectProviderUltimatum(disconnect) => {
                if disconnect.reason == mcs::DisconnectReason::UserRequested {
                    return Ok(true);
                }
            }

            _ => {
                warn!(
                    name = ironrdp_core::name(&message),
                    "Unexpected mcs message"
                );
            }
        }

        Ok(false)
    }

    async fn handle_input_event(&mut self, input: InputEventPdu) {
        for event in input.0 {
            let mut handler = self.handler.lock().await;
            match event {
                ironrdp_pdu::input::InputEvent::ScanCode(key) => {
                    handler.keyboard((key.key_code, key.flags).into());
                }

                ironrdp_pdu::input::InputEvent::Unicode(key) => {
                    handler.keyboard((key.unicode_code, key.flags).into());
                }

                ironrdp_pdu::input::InputEvent::Sync(sync) => {
                    handler.keyboard(sync.flags.into());
                }

                ironrdp_pdu::input::InputEvent::Mouse(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::MouseX(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::MouseRel(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::Unused(_) => {}
            }
        }
    }

    async fn accept_finalize<S>(
        &mut self,
        mut framed: TokioFramed<S>,
        mut acceptor: Acceptor,
    ) -> Result<TokioFramed<S>>
    where
        S: AsyncRead + AsyncWrite + Sync + Send + Unpin + 'static,
    {
        loop {
            let (new_framed, result) = ironrdp_acceptor::accept_finalize(framed, &mut acceptor)
                .await
                .context("failed to accept client during finalize")?;

            let (mut reader, mut writer) = split_tokio_framed(new_framed);

            match self
                .client_accepted(&mut reader, &mut writer, result)
                .await?
            {
                RunState::Continue => {
                    unreachable!();
                }
                RunState::DeactivationReactivation { desktop_size } => {
                    // No description of such behavior was found in the
                    // specification, but apparently, we must keep the channel
                    // state as they were during reactivation. This fixes
                    // various state issues during client resize.
                    acceptor = Acceptor::new_deactivation_reactivation(
                        acceptor,
                        core::mem::take(&mut self.static_channels),
                        desktop_size,
                    )?;
                    framed = unsplit_tokio_framed(reader, writer);
                    continue;
                }
                RunState::Disconnect => {
                    let final_framed = unsplit_tokio_framed(reader, writer);
                    return Ok(final_framed);
                }
            }
        }
    }

    pub fn set_credentials(&mut self, creds: Option<Credentials>) {
        debug!(?creds, "Changing credentials");
        self.creds = creds
    }
}

/// Encode a server-initiated Auto-Detect Request PDU for the MCS message channel.
///
/// The request is framed by a Basic Security Header (SEC_AUTODETECT_REQ) per
/// [MS-RDPBCGR] 2.2.14.3 and carried in an MCS Send Data Indication on the
/// negotiated message channel, not as a Share Data PDU on the I/O channel.
fn encode_autodetect_request(
    request: rdp::autodetect::AutoDetectRequest,
    message_channel_id: u16,
    user_channel_id: u16,
) -> Result<Vec<u8>> {
    // Auto-detect rides the MCS message channel framed by a Basic Security
    // Header (SEC_AUTODETECT_REQ), not a Share Control / Share Data header.
    let pdu = rdp::autodetect::AutoDetectReqPdu::new(request);
    let user_data = encode_vec(&pdu)?.into();
    let mcs_pdu = SendDataIndication {
        initiator_id: user_channel_id,
        channel_id: message_channel_id,
        user_data,
    };
    Ok(encode_vec(&X224(mcs_pdu))?)
}

async fn deactivate_all(
    io_channel_id: u16,
    user_channel_id: u16,
    writer: &mut impl FramedWrite,
) -> Result<(), anyhow::Error> {
    let pdu = ShareControlPdu::ServerDeactivateAll(ServerDeactivateAll);
    let pdu = rdp::headers::ShareControlHeader {
        share_id: 0,
        pdu_source: io_channel_id,
        share_control_pdu: pdu,
    };
    let user_data = encode_vec(&pdu)?.into();
    let pdu = SendDataIndication {
        initiator_id: user_channel_id,
        channel_id: io_channel_id,
        user_data,
    };
    let msg = encode_vec(&X224(pdu))?;
    writer.write_all(&msg).await?;
    Ok(())
}

/// Send a `ServerSetErrorInfoPdu(ServerDeniedConnection)` to the client, then return.
///
/// Used to deny a connection after credential validation rejects it, mirroring the
/// acceptor's exact-match denial so both paths refuse the same spec-defined way.
async fn send_access_denied(
    io_channel_id: u16,
    user_channel_id: u16,
    writer: &mut impl FramedWrite,
) -> Result<(), anyhow::Error> {
    let info = ServerSetErrorInfoPdu(ErrorInfo::ProtocolIndependentCode(
        ProtocolIndependentCode::ServerDeniedConnection,
    ));
    let user_data = encode_vec(&info)?.into();
    let pdu = SendDataIndication {
        initiator_id: user_channel_id,
        channel_id: io_channel_id,
        user_data,
    };
    let msg = encode_vec(&X224(pdu))?;
    writer.write_all(&msg).await?;
    Ok(())
}



#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_exact_match_no_domain_configured() {
        let validator = ExactMatchCredentialValidator::new(Credentials {
            username: "dev".to_string(),
            password: "secretpassword".to_string(),
            domain: None,
        });

        // 1. Exact match with None domain
        let res = validator
            .validate(&Credentials {
                username: "dev".to_string(),
                password: "secretpassword".to_string(),
                domain: None,
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 2. Client sends empty string domain
        let res = validator
            .validate(&Credentials {
                username: "dev".to_string(),
                password: "secretpassword".to_string(),
                domain: Some("".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 3. Client sends whitespace domain
        let res = validator
            .validate(&Credentials {
                username: "dev".to_string(),
                password: "secretpassword".to_string(),
                domain: Some("   ".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 4. Client sends local dot "." domain
        let res = validator
            .validate(&Credentials {
                username: "dev".to_string(),
                password: "secretpassword".to_string(),
                domain: Some(".".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 5. Client sends WORKGROUP / default domain
        let res = validator
            .validate(&Credentials {
                username: "dev".to_string(),
                password: "secretpassword".to_string(),
                domain: Some("WORKGROUP".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 6. Client sends username as .\dev
        let res = validator
            .validate(&Credentials {
                username: ".\\dev".to_string(),
                password: "secretpassword".to_string(),
                domain: None,
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 7. Client sends username as \dev
        let res = validator
            .validate(&Credentials {
                username: "\\dev".to_string(),
                password: "secretpassword".to_string(),
                domain: None,
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 8. Wrong password
        let res = validator
            .validate(&Credentials {
                username: "dev".to_string(),
                password: "wrong".to_string(),
                domain: None,
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Reject);

        // 9. Wrong username
        let res = validator
            .validate(&Credentials {
                username: "other".to_string(),
                password: "secretpassword".to_string(),
                domain: None,
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Reject);
    }

    #[tokio::test]
    async fn test_exact_match_with_domain_configured() {
        let validator = ExactMatchCredentialValidator::new(Credentials {
            username: "admin".to_string(),
            password: "pass".to_string(),
            domain: Some("CORP".to_string()),
        });

        // 1. Correct domain in domain field
        let res = validator
            .validate(&Credentials {
                username: "admin".to_string(),
                password: "pass".to_string(),
                domain: Some("CORP".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 2. Case-insensitive domain match
        let res = validator
            .validate(&Credentials {
                username: "admin".to_string(),
                password: "pass".to_string(),
                domain: Some("corp".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 3. Domain provided via DOMAIN\user
        let res = validator
            .validate(&Credentials {
                username: "CORP\\admin".to_string(),
                password: "pass".to_string(),
                domain: None,
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 4. Domain provided via user@domain
        let res = validator
            .validate(&Credentials {
                username: "admin@corp".to_string(),
                password: "pass".to_string(),
                domain: None,
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Accept);

        // 5. Missing domain should be rejected when server expects domain
        let res = validator
            .validate(&Credentials {
                username: "admin".to_string(),
                password: "pass".to_string(),
                domain: None,
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Reject);

        // 6. Empty domain should be rejected when server expects domain
        let res = validator
            .validate(&Credentials {
                username: "admin".to_string(),
                password: "pass".to_string(),
                domain: Some("".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Reject);

        // 7. Wrong domain
        let res = validator
            .validate(&Credentials {
                username: "admin".to_string(),
                password: "pass".to_string(),
                domain: Some("OTHER".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(res, CredentialDecision::Reject);
    }
}
