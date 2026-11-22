//! The per-player orchestrator.
//!
//! The [`PlayerManager`] consumes [`PlayerEvent`]s from the shared player bridge
//! and, for each player that registers, spins up a dedicated Cast receiver with
//! its own identity (`<name> [vibecast]`), stable installation-scoped device id,
//! dynamically-assigned ports, and the player's reported capabilities. When a
//! player disconnects its receiver is torn down (ephemeral lifecycle). The
//! manager also owns the single certificate-rotation loop, hot-swapping every
//! live receiver on rotation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use vibecast_bridge::{PlayerBridge, PlayerEvent, PlayerRegistration};
use vibecast_cast::AuthMaterial;
use vibecast_core::{AppRegistry, DeviceIdentity};
use vibecast_discovery::{DeviceCapabilities, EurekaIdentity};
use vibecast_messages::Volume;
use vibecast_player_api::{Player, PlayerCapabilitiesPayload, PlayerReport};
use vibecast_receiver::{
    spawn as spawn_receiver, CastSurface, ReceiverDiscovery, ReceiverParams,
    RunningReceiver as ReceiverInstance,
};
use vibecast_sdk::{
    DrmCapability, DrmSecurityLevel, DrmSystem, PairingObserver, Platform, PlayerCapabilities,
    Resolution,
};
use vibecast_security::{CertResolver, CertificateBundle, CertificateStore};
use vibecast_settings::PlayerSettings;

/// Facts about a player's receiver, reported to a frontend that owns discovery
/// (e.g. Android `NsdManager`) so it can advertise the service per player.
pub struct PlayerStarted {
    /// The player's stable id (from its registration).
    pub player_id: String,
    /// The advertised friendly name (`<reported name> [vibecast]`).
    pub name: String,
    /// Ports and mDNS facts to register for this player, or `None` when the
    /// receiver started cert-free: no Cast surface was bound, so there is no
    /// Cast device to advertise. The callback still fires, because the player
    /// itself is live and its lifecycle still pairs with `on_player_stopped`.
    pub discovery: Option<ReceiverDiscovery>,
}

/// Per-player lifecycle callbacks for a frontend that owns discovery
/// registration. Desktop advertises from Rust and can ignore these.
pub trait PlayerObserver: Send + Sync {
    /// A player's receiver started and (unless Rust advertises) needs registering.
    fn on_player_started(&self, _started: PlayerStarted) {}
    /// A player's receiver stopped and should be unregistered.
    fn on_player_stopped(&self, _player_id: &str) {}
    /// An app on this player's receiver published a manual pairing code to put
    /// on screen. Only the certificate-free path reaches this: on the Cast path
    /// a sender drives the launch, so no code is ever produced.
    fn on_pairing_code(&self, _player_id: &str, _code: &str) {}
}

/// Forwards an app's manual pairing code to the frontend observer, tagged with
/// the player whose receiver produced it.
struct PairingBridge {
    observer: Arc<dyn PlayerObserver>,
    player_id: String,
}

#[async_trait]
impl PairingObserver for PairingBridge {
    async fn pairing_code(&self, code: &str) {
        self.observer.on_pairing_code(&self.player_id, code);
    }
}

/// The certificate material every per-player Cast surface is built from, plus
/// the store the rotation loop polls.
///
/// `None` in [`ManagerConfig::certs`] is the cert-free path: no certificate is
/// loaded, no receiver gets a Cast surface, and rotation has nothing to do.
pub(crate) struct CertMaterial {
    pub resolver: Arc<CertResolver>,
    pub store: CertificateStore,
    /// The active bundle; replaced in place when the store rotates.
    pub bundle: CertificateBundle,
    pub crl: Option<Vec<u8>>,
}

/// Eureka identity fields shared across every per-player receiver.
pub(crate) struct EurekaConfig {
    pub manufacturer: String,
    pub locale: String,
    pub country_code: String,
    pub build_version: String,
    pub build_revision: String,
    pub capabilities: DeviceCapabilities,
}

/// Everything the manager needs to compose per-player receivers.
pub(crate) struct ManagerConfig {
    pub bridge: Arc<PlayerBridge>,
    pub registry: AppRegistry,
    pub installation_id: uuid::Uuid,
    pub http: reqwest::Client,
    pub data_dir: PathBuf,
    pub model: String,
    pub volume: Volume,
    pub user_agent: String,
    pub cast_device_capabilities: String,
    /// Certificate material backing every per-player Cast surface. `None`
    /// starts each receiver cert-free (hub + apps only).
    pub certs: Option<CertMaterial>,
    pub bind_host: String,
    pub local_ip: String,
    pub eureka: EurekaConfig,
    pub advertise_mdns: bool,
    /// Cast app id each per-player receiver auto-launches without a Cast sender
    /// (cert-free). `None` disables it (desktop); Android sets YouTube's id.
    pub auto_launch_app_id: Option<String>,
    pub cert_rotation_poll: f64,
    pub observer: Option<Arc<dyn PlayerObserver>>,
}

/// A live per-player receiver tagged with the epoch of the connection that
/// spawned it. The epoch lets a stale/duplicate connection's `Disconnected`
/// (two browser tabs, or an overlapping reconnect) be ignored so it can't tear
/// down the newer receiver that replaced it.
struct TrackedReceiver {
    epoch: u64,
    receiver: ReceiverInstance,
}

pub(crate) struct PlayerManager {
    config: ManagerConfig,
    receivers: HashMap<String, TrackedReceiver>,
}

impl PlayerManager {
    pub(crate) fn new(config: ManagerConfig) -> Self {
        Self {
            config,
            receivers: HashMap::new(),
        }
    }

    /// Run the manager loop until `shutdown` is cancelled, then tear down all
    /// receivers.
    pub(crate) async fn run(
        mut self,
        mut events: mpsc::Receiver<PlayerEvent>,
        shutdown: CancellationToken,
    ) {
        let period = Duration::from_secs_f64(self.config.cert_rotation_poll.max(1.0));
        let mut ticker = tokio::time::interval(period);
        ticker.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = ticker.tick() => self.rotate_certificates().await,
                event = events.recv() => match event {
                    Some(PlayerEvent::Registered {
                        registration,
                        player,
                        reports,
                        settings,
                        epoch,
                    }) => {
                        self.on_registered(epoch, *registration, player, reports, settings).await;
                    }
                    Some(PlayerEvent::Disconnected { player_id, epoch }) => {
                        self.on_disconnected(&player_id, epoch).await;
                    }
                    None => break,
                },
            }
        }

        for (player_id, tracked) in self.receivers.drain() {
            tracked.receiver.shutdown().await;
            if let Some(observer) = &self.config.observer {
                observer.on_player_stopped(&player_id);
            }
        }
    }

    async fn on_registered(
        &mut self,
        epoch: u64,
        registration: PlayerRegistration,
        player: Arc<dyn Player>,
        reports: mpsc::Receiver<PlayerReport>,
        settings: PlayerSettings,
    ) {
        let player_id = registration.player_id.clone();

        // Reconnect / duplicate id: replace the previous receiver.
        if let Some(existing) = self.receivers.remove(&player_id) {
            existing.receiver.shutdown().await;
        }

        let device_id = player_device_id(&self.config.installation_id, &player_id);
        let friendly_name = format!("{} [vibecast]", registration.name);

        // The Cast surface exists only where certificate material does. Without
        // it the receiver still runs — it just has nothing a sender can reach,
        // which is exactly what the auto-launch path below drives instead.
        let cast = self.config.certs.as_ref().map(|certs| CastSurface {
            eureka_identity: self.eureka_identity(&friendly_name, &device_id),
            resolver: certs.resolver.clone(),
            bundle: certs.bundle.clone(),
            crl: certs.crl.clone(),
            bind_host: self.config.bind_host.clone(),
            // Dynamic ports: each per-player receiver binds OS-assigned ports.
            cast_port: 0,
            eureka_http_port: 0,
            eureka_https_port: 0,
            advertise_mdns: self.config.advertise_mdns,
        });

        // A pairing code exists only where an app is launched without a sender,
        // so the observer is wired up only alongside a cert-free auto-launch —
        // leaving the Cast path (and every frontend that does not set one)
        // exactly as before.
        let pairing = match (&self.config.observer, &self.config.auto_launch_app_id) {
            (Some(observer), Some(_)) => Some(Arc::new(PairingBridge {
                observer: observer.clone(),
                player_id: player_id.clone(),
            }) as Arc<dyn PairingObserver>),
            _ => None,
        };

        let params = ReceiverParams {
            identity: DeviceIdentity::new(
                friendly_name.clone(),
                self.config.model.clone(),
                device_id,
            ),
            registry: self.config.registry.clone(),
            player,
            proxy: self.config.bridge.clone(),
            reports,
            http: self.config.http.clone(),
            data_dir: self.config.data_dir.clone(),
            volume: self.config.volume.clone(),
            user_agent: self.config.user_agent.clone(),
            cast_device_capabilities: self.config.cast_device_capabilities.clone(),
            capabilities: player_capabilities(registration.capabilities),
            player_settings: settings,
            cast,
            auto_launch_app_id: self.config.auto_launch_app_id.clone(),
            pairing,
        };

        let receiver = match spawn_receiver(params).await {
            Ok(receiver) => receiver,
            Err(error) => {
                tracing::error!(%error, player_id = %player_id, "failed to start receiver for player");
                return;
            }
        };

        match receiver.discovery() {
            Some(discovery) => tracing::info!(
                player_id = %player_id,
                name = %friendly_name,
                cast_port = discovery.cast_port,
                instance = %discovery.instance_name,
                "started receiver for player"
            ),
            None => tracing::info!(
                player_id = %player_id,
                name = %friendly_name,
                "started cert-free receiver for player"
            ),
        }

        if let Some(observer) = &self.config.observer {
            observer.on_player_started(PlayerStarted {
                player_id: player_id.clone(),
                name: friendly_name,
                discovery: receiver.discovery().cloned(),
            });
        }

        self.receivers
            .insert(player_id, TrackedReceiver { epoch, receiver });
    }

    async fn on_disconnected(&mut self, player_id: &str, epoch: u64) {
        // A stale/duplicate connection's disconnect must not tear down a newer
        // receiver that already replaced it: only remove when the epoch matches
        // the currently-tracked connection.
        if self.receivers.get(player_id).map(|t| t.epoch) != Some(epoch) {
            return;
        }
        if let Some(tracked) = self.receivers.remove(player_id) {
            tracked.receiver.shutdown().await;
            tracing::info!(player_id = %player_id, "stopped receiver for player");
            if let Some(observer) = &self.config.observer {
                observer.on_player_stopped(player_id);
            }
        }
    }

    async fn rotate_certificates(&mut self) {
        // Cert-free: no store to poll, and no receiver holds material to swap.
        let Some(certs) = &mut self.config.certs else {
            return;
        };
        let rotated = match certs.store.rotate_if_needed(unix_now()) {
            Ok(Some(bundle)) => bundle.clone(),
            Ok(None) => return,
            Err(error) => {
                tracing::error!(%error, "certificate rotation check failed");
                return;
            }
        };
        if let Err(error) = certs.resolver.update(&rotated) {
            tracing::error!(%error, "failed to hot-swap TLS certificate");
            return;
        }
        // New receivers registered after this point use the rotated bundle.
        certs.bundle = rotated.clone();
        let crl = rotated.crl.clone().or_else(|| certs.crl.clone());
        for tracked in self.receivers.values() {
            if let Some(handle) = tracked.receiver.rotation_handle() {
                handle.update_auth(AuthMaterial {
                    bundle: rotated.clone(),
                    crl: crl.clone(),
                });
            }
        }
        tracing::info!("rotated active certificate (TLS + device-auth)");
    }

    fn eureka_identity(&self, friendly_name: &str, device_id: &str) -> EurekaIdentity {
        let mut identity = EurekaIdentity::new(
            friendly_name.to_string(),
            self.config.model.clone(),
            device_id.to_string(),
            self.config.local_ip.clone(),
        );
        identity.manufacturer = self.config.eureka.manufacturer.clone();
        identity.locale = self.config.eureka.locale.clone();
        identity.country_code = self.config.eureka.country_code.clone();
        identity.build_version = self.config.eureka.build_version.clone();
        identity.build_revision = self.config.eureka.build_revision.clone();
        identity.capabilities = Some(self.config.eureka.capabilities.clone());
        identity
    }
}

fn player_capabilities(payload: PlayerCapabilitiesPayload) -> PlayerCapabilities {
    let default = PlayerCapabilities::default();
    PlayerCapabilities {
        platform: payload.platform.map_or(default.platform, parse_platform),
        drm: payload
            .drm
            .into_iter()
            .filter_map(|drm| {
                let system = match drm.system.as_str() {
                    "com.widevine.alpha" => DrmSystem::Widevine,
                    "com.microsoft.playready" => DrmSystem::PlayReady,
                    "org.w3.clearkey" => DrmSystem::ClearKey,
                    "com.apple.fps" | "com.apple.fps.1_0" => DrmSystem::FairPlay,
                    _ => return None,
                };
                let security_level = drm.security_level.as_deref().and_then(|level| match level {
                    "L1" => Some(DrmSecurityLevel::L1),
                    "L2" => Some(DrmSecurityLevel::L2),
                    "L3" => Some(DrmSecurityLevel::L3),
                    _ => None,
                });
                Some(DrmCapability::new(system, security_level))
            })
            .collect(),
        video_codecs: non_empty_or(payload.video_codecs, default.video_codecs),
        audio_codecs: non_empty_or(payload.audio_codecs, default.audio_codecs),
        max_resolution: payload
            .max_resolution
            .map_or(default.max_resolution, |resolution| {
                Resolution::new(resolution.width, resolution.height)
            }),
        hdr_formats: payload.hdr_formats,
        frame_rates: non_empty_or(payload.frame_rates, default.frame_rates),
        subtitle_formats: payload.subtitle_formats,
        hdcp_level: payload.hdcp_level,
    }
}

fn parse_platform(raw: String) -> Platform {
    match raw.as_str() {
        "android" => Platform::Android,
        "linux" => Platform::Linux,
        "macos" => Platform::MacOs,
        "windows" => Platform::Windows,
        "browser" => Platform::Browser,
        _ => Platform::Other(raw),
    }
}

fn non_empty_or<T>(value: Vec<T>, fallback: Vec<T>) -> Vec<T> {
    if value.is_empty() {
        fallback
    } else {
        value
    }
}

fn player_device_id(installation_id: &uuid::Uuid, player_id: &str) -> String {
    uuid::Uuid::new_v5(installation_id, player_id.as_bytes()).to_string()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibecast_player_api::{DrmCapabilityPayload, ResolutionPayload};

    #[test]
    fn player_device_ids_are_stable_and_installation_scoped() {
        let installation_a = uuid::Uuid::parse_str("8ae23d30-9c4a-4f7b-a0ca-e775e40472bb").unwrap();
        let installation_b = uuid::Uuid::parse_str("af67ff60-6f9b-4338-bfe2-ae77f13fe615").unwrap();

        let first = player_device_id(&installation_a, "browser-deadbeef");
        assert_eq!(first, player_device_id(&installation_a, "browser-deadbeef"));
        assert_ne!(first, player_device_id(&installation_a, "kodi-deadbeef"));
        assert_ne!(first, player_device_id(&installation_b, "browser-deadbeef"));

        let parsed = uuid::Uuid::parse_str(&first).unwrap();
        assert_eq!(parsed.get_version(), Some(uuid::Version::Sha1));
        assert_eq!(parsed.to_string(), first);
    }

    #[test]
    fn capability_payload_uses_bridge_mapping_and_fallbacks() {
        let capabilities = player_capabilities(PlayerCapabilitiesPayload {
            platform: Some("android".to_owned()),
            drm: vec![
                DrmCapabilityPayload {
                    system: "com.widevine.alpha".to_owned(),
                    security_level: Some("L1".to_owned()),
                },
                DrmCapabilityPayload {
                    system: "unknown".to_owned(),
                    security_level: None,
                },
            ],
            max_resolution: Some(ResolutionPayload {
                width: 3840,
                height: 2160,
            }),
            ..PlayerCapabilitiesPayload::default()
        });

        assert_eq!(capabilities.platform, Platform::Android);
        assert_eq!(capabilities.drm.len(), 1);
        assert_eq!(
            capabilities.drm_level(DrmSystem::Widevine),
            Some(DrmSecurityLevel::L1)
        );
        assert_eq!(capabilities.video_codecs, ["h264", "hevc"]);
        assert_eq!(capabilities.audio_codecs, ["aac"]);
        assert_eq!(capabilities.max_resolution, Resolution::new(3840, 2160));
        assert_eq!(capabilities.frame_rates, [24, 25, 30, 50, 60]);
    }
}
