//! Certificate-free YouTube "Link with TV code" pairing demo (host).
//!
//! Proves the cert-free control path without a Cast session, device-auth
//! certificate, mDNS, or the Android device: it pairs a YouTube Lounge screen
//! over plain HTTPS, prints the manual code to enter at youtube.com/pair, then
//! logs every Lounge command (play/pause/seek/setPlaylist/...) as the phone
//! drives it.
//!
//! Run:
//!   cargo run -p vibecast-apps-youtube --example pair
//!
//! Then enter the printed code INSIDE the YouTube mobile app:
//!   profile -> Settings -> "Link with TV code"
//!   (older/newer builds: profile -> "Watch on TV" -> "Enter TV code").
//!
//! Do NOT use youtube.com/activate — that is the account sign-in / device-auth
//! flow (it 301-redirects to accounts.google.com/device) and expects a
//! different code, so it rejects this Lounge pairing code as "incorrect". The
//! Lounge code is entered in the app's "Link with TV code" screen only.
//!
//! Queue a video and watch the commands arrive. Ctrl-C to stop.

use std::path::PathBuf;

use tokio::sync::watch;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let http = reqwest::Client::new();
    let data_dir: PathBuf = std::env::temp_dir().join("vibecast-pair-demo");
    if let Err(error) = std::fs::create_dir_all(&data_dir) {
        eprintln!("could not create data dir {}: {error}", data_dir.display());
        return;
    }

    // The bind loop runs until the process is stopped (Ctrl-C kills it). The
    // sender must stay alive for the whole run — if it is dropped, the loop's
    // `cancel.changed()` errors and the demo exits immediately — so bind it to
    // a named local that lives through the await below.
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    println!("Pairing a cert-free YouTube Lounge screen...");
    let result = vibecast_apps_youtube::run_pairing_demo(
        http,
        data_dir,
        "vibecast",
        |code| {
            println!("\n==========================================================");
            println!("  Enter this code INSIDE the YouTube mobile app:");
            println!("    profile > Settings > \"Link with TV code\"");
            println!("    (or: profile > \"Watch on TV\" > \"Enter TV code\")");
            println!("  Do NOT use youtube.com/activate (that is account sign-in,");
            println!("  a different code).");
            println!();
            println!("      {code}");
            println!("==========================================================\n");
            println!("Waiting for the phone to pair and send commands (Ctrl-C to stop)...");
        },
        cancel_rx,
    )
    .await;

    if let Err(error) = result {
        eprintln!("pairing demo failed: {error}");
    }
}
