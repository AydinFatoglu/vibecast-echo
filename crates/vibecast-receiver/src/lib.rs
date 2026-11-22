//! Generic Cast receiver composition.
//!
//! [`spawn`] assembles and supervises **one** receiver instance. The device hub
//! — bound to a single [`Player`] and app [`AppRegistry`] — is always started.
//! The Cast-facing surface around it (the CastV2 TLS server, the eureka
//! HTTP/HTTPS endpoints, and the mDNS advertisement) is started only when
//! [`ReceiverParams::cast`] carries a [`CastSurface`]. The composition is
//! deliberately independent of any particular app set, the player bridge, or
//! vibecast branding: the caller injects the identity, ports, certificate
//! material, app registry, player command sink, and proxy registrar.
//!
//! Leaving [`ReceiverParams::cast`] `None` runs the receiver **cert-free**: no
//! certificate is loaded and nothing Cast-facing is bound, so the hub is driven
//! by [`ReceiverParams::auto_launch_app_id`] — an app launched locally and
//! paired by hand — instead of by a Cast sender.
//!
//! Multiple receivers can be spawned on one host (each with its own identity and
//! ports); certificate rotation is intentionally *not* owned here (the active
//! certificate is shared across receivers) — instead each receiver with a Cast
//! surface exposes a [`RotationHandle`] so a single external rotation loop can
//! hot-swap the device-auth material of every receiver.

#![forbid(unsafe_code)]

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use vibecast_cast::{AuthMaterial, CastServer, ServerEvent};
use vibecast_core::{AppRegistry, DeviceHub, DeviceHubHandle, DeviceIdentity, HubConfig};
use vibecast_discovery::{CastAdvertisement, DiscoveryError, EurekaIdentity, EurekaServer};
use vibecast_messages::Volume;
use vibecast_player_api::{Player, PlayerReport, ProxyRegistrar};
use vibecast_sdk::{PairingObserver, PlayerCapabilities};
use vibecast_security::{server_config, CertResolver, CertificateBundle, SecurityError};
use vibecast_settings::PlayerSettings;

/// The Cast-facing half of a receiver: the CastV2 TLS server, the eureka
/// endpoints, the mDNS advertisement, and the certificate material all three
/// require.
///
/// Supplying this is what makes a receiver reachable by a Cast sender. Omitting
/// it (see [`ReceiverParams::cast`]) is the cert-free path, where nothing here
/// is needed because no sender is involved.
pub struct CastSurface {
    /// Eureka `/setup/eureka_info` identity.
    pub eureka_identity: EurekaIdentity,
    /// Shared TLS cert resolver (rotation updates this once for all receivers).
    pub resolver: Arc<CertResolver>,
    /// Active certificate bundle (device-auth material + digest source).
    pub bundle: CertificateBundle,
    /// Device-auth CRL, if any.
    pub crl: Option<Vec<u8>>,
    /// Host to bind listeners on.
    pub bind_host: String,
    /// CastV2 TLS port (`0` binds an OS-assigned port).
    pub cast_port: u16,
    /// Eureka HTTP port (`0` binds an OS-assigned port).
    pub eureka_http_port: u16,
    /// Eureka HTTPS port (`0` binds an OS-assigned port).
    pub eureka_https_port: u16,
    /// Advertise over mDNS from Rust (desktop `true`; frontend-owned discovery `false`).
    pub advertise_mdns: bool,
}

/// Everything needed to compose one receiver instance.
pub struct ReceiverParams {
    /// Cast device identity (friendly name, model, device id).
    pub identity: DeviceIdentity,
    /// App registry (which Cast apps this receiver serves).
    pub registry: AppRegistry,
    /// Player command sink for this receiver.
    pub player: Arc<dyn Player>,
    /// Session proxy registrar for this receiver.
    pub proxy: Arc<dyn ProxyRegistrar>,
    /// Player reports for this receiver (state/error from its player).
    pub reports: mpsc::Receiver<PlayerReport>,
    /// Shared HTTP client.
    pub http: reqwest::Client,
    /// Base data directory.
    pub data_dir: PathBuf,
    /// Initial receiver volume.
    pub volume: Volume,
    /// User-Agent for app sessions.
    pub user_agent: String,
    /// `CAST-DEVICE-CAPABILITIES` header value for app sessions.
    pub cast_device_capabilities: String,
    /// Capabilities of the player bound to this receiver.
    pub capabilities: PlayerCapabilities,
    /// Live settings scoped to the player bound to this receiver.
    pub player_settings: PlayerSettings,
    /// The Cast-facing surface and its certificate material. `None` runs this
    /// receiver cert-free — hub and apps only, nothing a sender can reach.
    pub cast: Option<CastSurface>,
    /// Cast app id to launch automatically — without a Cast sender — once the
    /// hub is running (cert-free path, e.g. YouTube `"233637DE"` driven by a
    /// manual "Link with TV code" pairing). `None` on platforms that only
    /// launch apps through a real Cast sender.
    pub auto_launch_app_id: Option<String>,
    /// Observer that receives the manual pairing code an auto-launched app
    /// publishes (cert-free path). Pointless without
    /// [`auto_launch_app_id`](Self::auto_launch_app_id), and `None` on the Cast
    /// path, where a sender — not a code — drives the launch.
    pub pairing: Option<Arc<dyn PairingObserver>>,
}

/// Errors composing or starting a receiver.
#[derive(Debug, thiserror::Error)]
pub enum ReceiverError {
    /// Building the TLS server config failed.
    #[error("certificate/TLS setup failed")]
    Certs(#[from] SecurityError),
    /// Building the eureka server or starting mDNS failed.
    #[error("discovery setup failed")]
    Discovery(#[from] DiscoveryError),
    /// Binding a listening socket failed.
    #[error("binding {what} on port {port}")]
    Bind {
        /// Which listener failed to bind.
        what: &'static str,
        /// The requested port.
        port: u16,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
}

/// The optional live mDNS responder advertising this receiver.
///
/// The responder exists only when this build both compiles the `mdns` feature
/// and was asked to advertise from Rust; otherwise a frontend that owns
/// discovery registers the reported instance/TXT itself.
struct Advertiser {
    #[cfg(feature = "mdns")]
    responder: Option<vibecast_discovery::MdnsResponder>,
}

impl Advertiser {
    /// Stop advertising (a no-op without a live responder).
    fn stop(&mut self) {
        #[cfg(feature = "mdns")]
        if let Some(responder) = &mut self.responder {
            responder.stop();
        }
    }
}

/// A cheaply-cloneable handle for pushing certificate-rotation updates into a
/// running receiver from an external rotation loop.
#[derive(Clone)]
pub struct RotationHandle {
    cast_server: Arc<CastServer>,
}

impl RotationHandle {
    /// Hot-swap the device-auth material for future connections.
    pub fn update_auth(&self, auth: AuthMaterial) {
        self.cast_server.update_auth(auth);
    }
}

/// Ports and mDNS facts of a running Cast surface, for a frontend that owns
/// discovery registration (e.g. Android `NsdManager`).
#[derive(Debug, Clone)]
pub struct ReceiverDiscovery {
    /// CastV2 TLS port actually bound.
    pub cast_port: u16,
    /// Eureka HTTP port actually bound.
    pub eureka_http_port: u16,
    /// Eureka HTTPS port actually bound.
    pub eureka_https_port: u16,
    /// mDNS service instance label, for frontend-owned registration.
    pub instance_name: String,
    /// Cast TXT record key/value pairs, for frontend-owned registration.
    pub txt: Vec<(String, String)>,
}

/// The live Cast surface of a receiver. Kept whole so its three parts can never
/// disagree about whether a surface exists.
struct CastRuntime {
    server: Arc<CastServer>,
    advertiser: Arc<tokio::sync::Mutex<Advertiser>>,
    discovery: ReceiverDiscovery,
}

/// The identity strings a receiver advertises, captured before the hub takes
/// ownership of the full [`DeviceIdentity`].
struct Advertised<'a> {
    name: &'a str,
    model: &'a str,
    device_id: &'a str,
}

/// A running receiver instance and the discovery facts a frontend needs.
pub struct RunningReceiver {
    shutdown: CancellationToken,
    tasks: TaskTracker,
    hub_handle: DeviceHubHandle,
    cast: Option<CastRuntime>,
}

impl RunningReceiver {
    /// Ports and mDNS facts to advertise, or `None` on the cert-free path —
    /// where no Cast surface was started, so there is no device to advertise.
    #[must_use]
    pub fn discovery(&self) -> Option<&ReceiverDiscovery> {
        self.cast.as_ref().map(|cast| &cast.discovery)
    }

    /// A handle for applying certificate-rotation updates to this receiver, or
    /// `None` cert-free: nothing here holds certificate material to rotate.
    #[must_use]
    pub fn rotation_handle(&self) -> Option<RotationHandle> {
        self.cast.as_ref().map(|cast| RotationHandle {
            cast_server: cast.server.clone(),
        })
    }

    /// Cooperatively tear this receiver down: stop advertising, stop app
    /// sessions cleanly, then cancel and await the supervised tasks.
    pub async fn shutdown(self) {
        if let Some(cast) = &self.cast {
            cast.advertiser.lock().await.stop();
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.hub_handle.shutdown()).await;
        self.shutdown.cancel();
        self.tasks.close();
        if tokio::time::timeout(Duration::from_secs(5), self.tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!("receiver background tasks did not stop within 5s; exiting anyway");
        }
    }
}

/// Assemble and start one receiver instance. Returns once every listener is
/// bound and (when `advertise_mdns`) advertising has started.
pub async fn spawn(params: ReceiverParams) -> Result<RunningReceiver, ReceiverError> {
    let ReceiverParams {
        identity,
        registry,
        player,
        proxy,
        reports,
        http,
        data_dir,
        volume,
        user_agent,
        cast_device_capabilities,
        capabilities,
        player_settings,
        cast,
        auto_launch_app_id,
        pairing,
    } = params;

    // Advertisement identity strings, captured before `identity` moves into the hub.
    let adv_name = identity.friendly_name.clone();
    let adv_model = identity.device_model.clone();
    let adv_id = identity.device_id.clone();

    // Device hub. Building it is infallible and starts nothing, so the Cast
    // surface below can still fail without leaving a task running.
    let hub = DeviceHub::new(HubConfig {
        identity,
        registry,
        player,
        proxy,
        http,
        data_dir,
        volume,
        user_agent,
        cast_device_capabilities,
        capabilities,
        player_settings,
        pairing,
    });
    let hub_handle = hub.handle();

    let shutdown = CancellationToken::new();
    let tasks = TaskTracker::new();

    // The only fallible step: binding the Cast-facing listeners and their TLS.
    let cast = match cast {
        Some(surface) => Some(
            start_cast_surface(
                surface,
                Advertised {
                    name: &adv_name,
                    model: &adv_model,
                    device_id: &adv_id,
                },
                &tasks,
                &shutdown,
                &hub_handle,
            )
            .await?,
        ),
        None => {
            tracing::info!("cert-free receiver: no CastV2/eureka listener, no advertisement");
            None
        }
    };

    // Player reports -> hub.
    {
        let hub_handle = hub_handle.clone();
        let shutdown = shutdown.clone();
        let mut reports = reports;
        tasks.spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    report = reports.recv() => match report {
                        Some(report) => {
                            if hub_handle.send_player_report(report).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
        });
    }

    tasks.spawn(hub.run());

    // Cert-free auto-launch: if configured, drive a LAUNCH with no Cast sender
    // (e.g. YouTube via a manual "Link with TV code" pairing) now that the hub
    // is running. The event queues on the hub's channel, so there is nothing to
    // wait for; a closed hub (immediate shutdown) is logged and ignored.
    if let Some(app_id) = auto_launch_app_id {
        let hub_handle = hub_handle.clone();
        tasks.spawn(async move {
            if let Err(error) = hub_handle.launch_app(app_id).await {
                tracing::warn!(%error, "cert-free auto-launch failed");
            }
        });
    }

    Ok(RunningReceiver {
        shutdown,
        tasks,
        hub_handle,
        cast,
    })
}

/// Bind and start the Cast-facing surface: CastV2 TLS, eureka HTTP/HTTPS, and
/// (when asked) the mDNS advertisement.
async fn start_cast_surface(
    surface: CastSurface,
    advertised: Advertised<'_>,
    tasks: &TaskTracker,
    shutdown: &CancellationToken,
    hub_handle: &DeviceHubHandle,
) -> Result<CastRuntime, ReceiverError> {
    let CastSurface {
        eureka_identity,
        resolver,
        bundle,
        crl,
        bind_host,
        cast_port,
        eureka_http_port,
        eureka_https_port,
        advertise_mdns,
    } = surface;

    // TLS configs backed by the shared resolver, so a single rotation updates all.
    let cast_tls = server_config(resolver.clone())?;
    let eureka_tls = server_config(resolver)?;

    // Bind all listeners up front. A `0` port binds an OS-assigned port, read
    // back so the advertisement announces the real port.
    let cast_listener = bind(&bind_host, cast_port, "cast").await?;
    let cast_port = local_port(&cast_listener, cast_port);
    let eureka = Arc::new(EurekaServer::new(&bundle, eureka_identity)?);
    let http_listener = bind(&bind_host, eureka_http_port, "eureka http").await?;
    let eureka_http_port = local_port(&http_listener, eureka_http_port);
    let https_listener = bind_blocking(&bind_host, eureka_https_port, "eureka https")?;
    let eureka_https_port = https_listener
        .local_addr()
        .map(|addr| addr.port())
        .unwrap_or(eureka_https_port);

    // CastV2 TLS server: its events drive the hub.
    let (events_tx, events_rx) = mpsc::channel(64);
    let server = Arc::new(CastServer::new(
        cast_tls,
        AuthMaterial { bundle, crl },
        events_tx,
    ));
    spawn_server_forward(tasks, shutdown.clone(), events_rx, hub_handle.clone());

    {
        let server = server.clone();
        serve(tasks, shutdown.clone(), "cast server", async move {
            server.serve(cast_listener).await
        });
    }
    {
        let eureka = eureka.clone();
        serve(tasks, shutdown.clone(), "eureka http", async move {
            eureka.serve_http(http_listener).await
        });
    }
    serve(tasks, shutdown.clone(), "eureka https", async move {
        eureka.serve_https(https_listener, eureka_tls).await
    });

    // Advertisement identity is always computed (its instance/TXT are reported
    // to a frontend that owns discovery). The mDNS responder is only started
    // when this build compiles the `mdns` feature and was asked to advertise.
    let advertisement = CastAdvertisement::new(
        advertised.name,
        advertised.model,
        advertised.device_id,
        cast_port,
    );

    #[cfg(feature = "mdns")]
    let responder = if advertise_mdns {
        Some(vibecast_discovery::MdnsResponder::start(&advertisement)?)
    } else {
        None
    };
    #[cfg(not(feature = "mdns"))]
    if advertise_mdns {
        tracing::debug!(
            "advertise_mdns is set but the `mdns` feature is not compiled in; \
             relying on frontend-owned discovery"
        );
    }

    Ok(CastRuntime {
        server,
        advertiser: Arc::new(tokio::sync::Mutex::new(Advertiser {
            #[cfg(feature = "mdns")]
            responder,
        })),
        discovery: ReceiverDiscovery {
            cast_port,
            eureka_http_port,
            eureka_https_port,
            instance_name: advertisement.instance().to_string(),
            txt: txt_pairs(&advertisement),
        },
    })
}

async fn bind(
    bind_host: &str,
    port: u16,
    what: &'static str,
) -> Result<TcpListener, ReceiverError> {
    TcpListener::bind((bind_host, port))
        .await
        .map_err(|source| ReceiverError::Bind { what, port, source })
}

/// Bind the one listener the eureka HTTPS server takes as a `std` socket.
fn bind_blocking(
    bind_host: &str,
    port: u16,
    what: &'static str,
) -> Result<std::net::TcpListener, ReceiverError> {
    let listener = std::net::TcpListener::bind((bind_host, port))
        .map_err(|source| ReceiverError::Bind { what, port, source })?;
    listener
        .set_nonblocking(true)
        .map_err(|source| ReceiverError::Bind { what, port, source })?;
    Ok(listener)
}

/// Supervise a serve loop: drop it when `shutdown` fires, log if it stops early.
fn serve<F, E>(tasks: &TaskTracker, shutdown: CancellationToken, what: &'static str, future: F)
where
    F: Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display,
{
    tasks.spawn(async move {
        tokio::select! {
            () = shutdown.cancelled() => {}
            result = future => {
                if let Err(error) = result {
                    tracing::error!(%error, "{what} stopped");
                }
            }
        }
    });
}

fn local_port(listener: &TcpListener, fallback: u16) -> u16 {
    listener
        .local_addr()
        .map(|addr| addr.port())
        .unwrap_or(fallback)
}

fn spawn_server_forward(
    tasks: &TaskTracker,
    shutdown: CancellationToken,
    mut events_rx: mpsc::Receiver<ServerEvent>,
    hub_handle: DeviceHubHandle,
) {
    tasks.spawn(async move {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                event = events_rx.recv() => match event {
                    Some(event) => {
                        if hub_handle.send_server_event(event).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
    });
}

/// The advertised Cast TXT record as owned key/value pairs.
fn txt_pairs(advertisement: &CastAdvertisement) -> Vec<(String, String)> {
    advertisement
        .txt()
        .pairs()
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}
