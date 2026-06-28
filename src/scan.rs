//! Resolution of inaccurate track durations.
//!
//! A reported duration can exceed the audio that actually decodes (a headerless
//! MP3 with unreconstructable frames, or a truncated/mis-tagged file). `position`
//! counts only decoded audio, so a progress bar would never fill. Two paths fix
//! it, both via [`publish_correction`]:
//! - **proactive** ([`spawn`]): a headerless MP3 is scanned off the audio path at
//!   start to count its renderable frames;
//! - **reactive** (the worker): on a seek-free playthrough, the frames decoded
//!   during playback are reconciled against the reported duration at track end.

use crate::{decoder::VoxDecoder, event::VoxEvent, state::SharedState};
use crossbeam_channel::Sender;
use std::{path::PathBuf, sync::Arc, thread, time::Duration};

/// Re-check the epoch and shutdown flag every this many packets so a track change
/// or shutdown aborts the scan promptly.
const EPOCH_CHECK_INTERVAL: u64 = 512;

/// Smallest scan-vs-reported difference worth publishing; below it the scan just
/// confirms the estimate and emitting `DurationResolved` would be noise.
const MIN_CHANGE_SECS: f64 = 0.5;

/// Spawn a detached background scan for `path` belonging to track `epoch`; its
/// result is discarded if the epoch has moved on by the time it finishes.
pub(crate) fn spawn(path: String, epoch: u64, state: Arc<SharedState>, events: Sender<VoxEvent>) {
    let _ = thread::Builder::new()
        .name("voxio-duration-scan".into())
        .spawn(move || run(path, epoch, &state, &events));
}

fn run(path: String, epoch: u64, state: &SharedState, events: &Sender<VoxEvent>) {
    let Some(secs) = resolve_duration(&path, epoch, state) else {
        return;
    };
    // Discard if a newer track took over (so the comparison reads this track's
    // reported duration, not a successor's).
    if state.is_shutdown() || state.track_epoch() != epoch {
        return;
    }
    publish_correction(state, events, PathBuf::from(path), secs);
}

/// Store `secs` and emit [`VoxEvent::DurationResolved`], but only when it differs
/// enough from the reported value to matter. Shared by the background scan and
/// the worker's end-of-playthrough reconciliation.
pub(crate) fn publish_correction(
    state: &SharedState,
    events: &Sender<VoxEvent>,
    path: PathBuf,
    secs: f64,
) {
    if (secs - state.get_duration_secs()).abs() < MIN_CHANGE_SECS {
        return;
    }
    state.set_duration_secs(secs);
    let _ = events.try_send(VoxEvent::DurationResolved {
        path,
        duration: Duration::from_secs_f64(secs),
    });
}

/// Renderable duration in seconds: decode the whole file and count good frames
/// (`position`'s basis, so the two agree). `None` keeps the estimate.
fn resolve_duration(path: &str, epoch: u64, state: &SharedState) -> Option<f64> {
    let mut decoder = VoxDecoder::open(path).ok()?;
    let sample_rate = decoder.info.sample_rate.max(1) as f64;
    let channels = decoder.info.channels.max(1);

    let mut frames = 0u64;
    let mut since_check = 0u64;
    loop {
        if since_check >= EPOCH_CHECK_INTERVAL {
            if state.is_shutdown() || state.track_epoch() != epoch {
                return None;
            }
            since_check = 0;
        }
        match decoder.next_packet() {
            Ok(Some(buf)) => {
                frames += (buf.len() / channels) as u64;
                since_check += 1;
            }
            // Clean/graceful end, or undecodable from the start (frames == 0).
            Ok(None) | Err(_) => break,
        }
    }

    (frames > 0).then(|| frames as f64 / sample_rate)
}
