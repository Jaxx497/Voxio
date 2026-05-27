//! Probe: does cpal 0.17.3's WASAPI backend deliver device-loss to the error callback?
//!
//! Run on Windows, then while it's running switch your default output device
//! (Sound settings → Output → pick a different device) or unplug your USB DAC.
//!
//! What to watch for:
//!   - [error] lines = cpal called our error callback. Good — we can react.
//!   - "no callbacks for Ns" warnings = audio thread stopped invoking us but no
//!     error was reported. Means cpal silently zombified the stream.
//!   - A panic with no [error] line before it = cpal's own thread blew up before
//!     it could notify us. That's the worst case; a cpal upgrade is the only fix.
//!
//! Stop with Ctrl-C.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

fn main() {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("no default output device");

    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let config = device
        .default_output_config()
        .expect("no default config");
    println!("[probe] initial device: {}", name);
    println!("[probe] initial config: {:?}", config);

    let stream_config: cpal::StreamConfig = config.into();
    let callback_count = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));

    let cb_clone = Arc::clone(&callback_count);
    let err_clone = Arc::clone(&error_count);

    let stream = device
        .build_output_stream(
            &stream_config,
            move |data: &mut [f32], _info| {
                cb_clone.fetch_add(1, Ordering::Relaxed);
                // Output silence — we don't want to blast audio during a probe.
                data.fill(0.0);
            },
            move |e| {
                err_clone.fetch_add(1, Ordering::Relaxed);
                eprintln!("[error] cpal stream error: {:?}", e);
            },
            None,
        )
        .expect("failed to build stream");

    stream.play().expect("failed to start stream");
    println!("[probe] stream started. Switch your default output device now.");
    println!("[probe] heartbeat every 2s; Ctrl-C to stop.\n");

    let start = Instant::now();
    let mut last_cb_count = 0u64;
    let mut last_cb_seen_at = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let cb = callback_count.load(Ordering::Relaxed);
        let err = error_count.load(Ordering::Relaxed);
        let delta = cb - last_cb_count;
        let elapsed = start.elapsed().as_secs();

        if delta > 0 {
            last_cb_seen_at = Instant::now();
        }
        let stale_for = last_cb_seen_at.elapsed().as_secs();

        let stale_marker = if delta == 0 && stale_for >= 2 {
            format!("  ⚠ no callbacks for {stale_for}s")
        } else {
            String::new()
        };

        println!(
            "[heartbeat t={elapsed}s] callbacks={cb} (+{delta}) errors={err}{stale_marker}",
        );
        last_cb_count = cb;

        // Re-query the default device every tick so we can see if the system
        // default has changed under us, even when cpal stays quiet.
        if let Some(d) = host.default_output_device() {
            if let Ok(desc) = d.description() {
                let now_name = desc.name();
                if now_name != name {
                    println!("[probe] system default is now '{}' (was '{}')", now_name, name);
                }
            }
        }
    }
}
