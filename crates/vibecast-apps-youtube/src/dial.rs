//! Certificate-free DIAL discovery for the YouTube phone app.
//!
//! A phone keeps a screen as a cast target only while it can also *see* it on
//! the local network. A screen it knows solely from a typed TV code is treated
//! as a one-off: the moment the user leaves the watch page, the session ends
//! with `DISCONNECTED_BY_USER_PLAY_ON_PHONE` and the next video plays locally.
//!
//! DIAL supplies the missing half, and needs no Cast certificate: answer SSDP
//! searches, serve a device description, and accept the launch request whose
//! body carries the phone's own pairing code. Registering that code is what
//! joins the two on the Lounge — see [`PairingRegistrar`].

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as BlockingUdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;

use crate::lounge::PairingRegistrar;

const SSDP_PORT: u16 = 1900;
const SSDP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const DIAL_SEARCH_TARGET: &str = "urn:dial-multiscreen-org:service:dial:1";
/// Non-standard, but the header every DIAL client reads to find `/apps/`.
const APPLICATION_URL: HeaderName = HeaderName::from_static("application-url");
const SSDP_BUFFER: usize = 2048;

/// What this screen tells the network about itself.
pub(crate) struct Screen {
    pub(crate) friendly_name: String,
    pub(crate) model: String,
    /// Stable across restarts: it is the receiver's own device id.
    pub(crate) uuid: String,
    pub(crate) registrar: PairingRegistrar,
    /// Set by the first launch request; see [`app_status`].
    pub(crate) launched: AtomicBool,
}

/// Serves DIAL until `cancel` fires.
pub(crate) async fn serve(screen: Arc<Screen>, cancel: watch::Receiver<bool>) {
    let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(%error, "DIAL HTTP listener failed; local discovery is off");
            return;
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            tracing::warn!(%error, "DIAL HTTP port unknown; local discovery is off");
            return;
        }
    };
    tracing::info!(port, name = %screen.friendly_name, "DIAL server started");

    let router = Router::new()
        .route("/dd.xml", get(device_description))
        .route("/apps/YouTube", get(app_status).post(launch))
        .with_state(screen.clone());
    let mut http_cancel = cancel.clone();
    let http = tokio::spawn(async move {
        let shutdown = async move {
            while http_cancel.changed().await.is_ok() {
                if *http_cancel.borrow() {
                    return;
                }
            }
        };
        if let Err(error) = axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::warn!(%error, "DIAL HTTP server stopped");
        }
    });

    answer_searches(&screen, port, cancel).await;
    http.abort();
}

// --- SSDP ----------------------------------------------------------------

async fn answer_searches(screen: &Screen, http_port: u16, mut cancel: watch::Receiver<bool>) {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, SSDP_PORT)).await {
        Ok(socket) => socket,
        Err(error) => {
            tracing::warn!(%error, "SSDP port unavailable; the phone cannot discover this screen");
            return;
        }
    };
    if let Err(error) = socket.join_multicast_v4(SSDP_GROUP, Ipv4Addr::UNSPECIFIED) {
        tracing::warn!(%error, "SSDP multicast join failed; searches may not arrive");
    }

    let mut buffer = vec![0_u8; SSDP_BUFFER];
    loop {
        tokio::select! {
            result = cancel.changed() => {
                if result.is_err() || *cancel.borrow() {
                    return;
                }
            }
            received = socket.recv_from(&mut buffer) => {
                let Ok((len, from)) = received else { continue };
                let request = String::from_utf8_lossy(&buffer[..len]);
                if !is_dial_search(&request) {
                    continue;
                }
                let Some(local) = local_address_towards(from) else { continue };
                let response = search_response(screen, local, http_port);
                if let Err(error) = socket.send_to(response.as_bytes(), from).await {
                    tracing::warn!(%error, %from, "SSDP reply failed");
                } else {
                    tracing::info!(%from, "answered a DIAL search");
                }
            }
        }
    }
}

/// True for an `M-SEARCH` asking for DIAL services (or for everything).
fn is_dial_search(request: &str) -> bool {
    if !request.starts_with("M-SEARCH") {
        return false;
    }
    request.lines().any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case("st")
            && matches!(value.trim(), DIAL_SEARCH_TARGET | "ssdp:all")
    })
}

/// This host's address on the interface that reaches `peer`.
///
/// The reply has to carry a URL the phone can open, and a screen with several
/// interfaces has no single answer; asking the routing table for the one it
/// would use costs a socket and sends nothing.
fn local_address_towards(peer: SocketAddr) -> Option<Ipv4Addr> {
    let probe = BlockingUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect(peer).ok()?;
    match probe.local_addr().ok()?.ip() {
        IpAddr::V4(address) => Some(address),
        IpAddr::V6(_) => None,
    }
}

fn search_response(screen: &Screen, local: Ipv4Addr, http_port: u16) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age=1800\r\n\
         EXT:\r\n\
         LOCATION: http://{local}:{http_port}/dd.xml\r\n\
         SERVER: Linux/4.9 UPnP/1.1 vibecast/1.0\r\n\
         ST: {DIAL_SEARCH_TARGET}\r\n\
         USN: uuid:{uuid}::{DIAL_SEARCH_TARGET}\r\n\
         BOOTID.UPNP.ORG: 1\r\n\
         CONFIGID.UPNP.ORG: 1\r\n\
         \r\n",
        uuid = screen.uuid,
    )
}

// --- HTTP ----------------------------------------------------------------

/// The URL the client used to reach us, rebuilt from its own `Host` header.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("http://{host}")
}

async fn device_description(State(screen): State<Arc<Screen>>, headers: HeaderMap) -> Response {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" configId="1">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:dial-multiscreen-org:device:dial:1</deviceType>
    <friendlyName>{name}</friendlyName>
    <manufacturer>vibecast</manufacturer>
    <modelName>{model}</modelName>
    <UDN>uuid:{uuid}</UDN>
  </device>
</root>
"#,
        name = screen.friendly_name,
        model = screen.model,
        uuid = screen.uuid,
    );
    (
        [
            (header::CONTENT_TYPE, "application/xml".to_string()),
            (APPLICATION_URL, format!("{}/apps/", base_url(&headers))),
        ],
        body,
    )
        .into_response()
}

/// `stopped` until the phone launches, `running` from then on.
///
/// Both halves matter. Reporting `stopped` first is what makes the phone send
/// the launch request carrying its pairing code, the only way it reaches this
/// screen's Lounge. Reporting `running` afterwards is what keeps it there: the
/// phone polls this resource during the session and ends it with
/// `SCREEN_APP_STOPPED` the moment the answer says the app is gone.
async fn app_status(State(screen): State<Arc<Screen>>) -> Response {
    let state = if screen.launched.load(Ordering::Relaxed) {
        "<state>running</state>\n  <link rel=\"run\" href=\"run\"/>"
    } else {
        "<state>stopped</state>"
    };
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<service xmlns="urn:dial-multiscreen-org:schemas:dial" dialVer="1.7">
  <name>YouTube</name>
  <options allowStop="false"/>
  {state}
</service>
"#
    );
    ([(header::CONTENT_TYPE, "application/xml")], body).into_response()
}

async fn launch(State(screen): State<Arc<Screen>>, headers: HeaderMap, body: String) -> Response {
    let Some(code) = form_value(&body, "pairingCode") else {
        tracing::warn!("DIAL launch without a pairing code");
        return StatusCode::BAD_REQUEST.into_response();
    };
    if let Err(error) = screen.registrar.register(&code).await {
        tracing::warn!(%error, "DIAL pairing code registration failed");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    screen.launched.store(true, Ordering::Relaxed);
    tracing::info!("registered a phone's DIAL pairing code");
    (
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!("{}/apps/YouTube/run", base_url(&headers)),
        )],
    )
        .into_response()
}

fn form_value(body: &str, key: &str) -> Option<String> {
    url::form_urlencoded::parse(body.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dial_searches_are_recognised_and_others_ignored() {
        let search = |target: &str| {
            format!(
                "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nST: {target}\r\nMX: 1\r\n\r\n"
            )
        };
        assert!(is_dial_search(&search(DIAL_SEARCH_TARGET)));
        assert!(is_dial_search(&search("ssdp:all")));
        assert!(!is_dial_search(&search(
            "urn:schemas-upnp-org:device:MediaRenderer:1"
        )));
        assert!(!is_dial_search("NOTIFY * HTTP/1.1\r\nST: ssdp:all\r\n\r\n"));
    }

    #[test]
    fn the_launch_body_yields_the_phones_pairing_code() {
        let body = "pairingCode=abc-123&theme=cl&dialLaunch=watch";
        assert_eq!(form_value(body, "pairingCode").as_deref(), Some("abc-123"));
        assert_eq!(form_value(body, "dialLaunch").as_deref(), Some("watch"));
        assert_eq!(form_value(body, "missing"), None);
    }
}
