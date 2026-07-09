use crate::{
    error::{Result, VoxError},
    event::{RebindReason, VoxEvent},
    state::SharedState,
    tap::{self, TapReader},
};
use cpal::{
    Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_channel::{Receiver, Sender};
use rtrb::{Producer, RingBuffer};
use std::{
    panic::{self, AssertUnwindSafe},
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

const SEEK_FADE_MS: usize = 30;
/// One-pole smoothing time for volume changes. Long enough that a slider drag
/// can't click, short enough to feel responsive.
const VOLUME_RAMP_MS: f32 = 15.0;
/// Retry backoff while the device is lost: start fast (a changed device usually
/// settles in a few hundred ms), double up to a gentle cap (don't hammer
/// enumeration when it's truly gone).
const REBUILD_RETRY_MIN: Duration = Duration::from_millis(50);
const REBUILD_RETRY_MAX: Duration = Duration::from_millis(300);

pub(crate) enum OutputControl {
    Rebuild(RebindReason),
    Shutdown,
}

/// Handoff from supervisor → worker when the stream (re)builds.
pub(crate) struct AudioBinding {
    pub producer: Producer<f32>,
    pub output_rate: u32,
    pub output_channels: usize,
}

struct StreamInfo {
    name: String,
    sample_rate: u32,
    channels: usize,
}

/// Owns the cpal stream for the engine's lifetime, rebuilding it on demand.
pub(crate) fn spawn(
    state: Arc<SharedState>,
    ctrl: Receiver<OutputControl>,
    ctrl_tx: Sender<OutputControl>,
    events: Sender<VoxEvent>,
    bindings: Sender<AudioBinding>,
    taps: Sender<TapReader>,
    init: Sender<Result<()>>,
    buffer_ms: usize,
    tap_capacity: usize,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let (stream, info) =
            match install_caught(&state, &ctrl_tx, &bindings, &taps, buffer_ms, tap_capacity) {
                Ok(built) => built,
                Err(e) => {
                    let _ = init.send(Err(e));
                    return;
                }
            };
        let _ = init.send(Ok(()));

        let mut current = Some(stream); // held only to keep the stream alive
        let mut name = info.name;

        while let Ok(msg) = ctrl.recv() {
            let OutputControl::Rebuild(reason) = msg else {
                drop_stream(current.take());
                return;
            };

            state.set_rebuilding(true);
            drop_stream(current.take()); // drop old stream first; cpal joins its audio thread

            // Drain duplicate Rebuilds queued by the old stream's error callback
            // (it can fire repeatedly before the join completes). Stop at, never
            // drop, Shutdown — else teardown hangs.
            while let Ok(msg) = ctrl.try_recv() {
                if let OutputControl::Shutdown = msg {
                    return;
                }
            }

            let mut last_err: Option<String> = None;
            let mut backoff = REBUILD_RETRY_MIN;
            loop {
                match install_caught(&state, &ctrl_tx, &bindings, &taps, buffer_ms, tap_capacity) {
                    Ok((stream, info)) => {
                        state.set_rebuilding(false);
                        let _ = events.try_send(VoxEvent::DeviceChanged {
                            name: info.name.clone(),
                            sample_rate: info.sample_rate,
                            channels: info.channels,
                            reason,
                        });
                        name = info.name;
                        current = Some(stream);
                        break;
                    }
                    Err(e) => {
                        // Once per distinct reason (re-announce on change), not per
                        // retry — surfaces a shifting failure without spamming.
                        let msg = e.to_string();
                        if last_err.as_deref() != Some(msg.as_str()) {
                            let _ = events.try_send(VoxEvent::DeviceLost {
                                name: name.clone(),
                                error: msg.clone(),
                            });
                            last_err = Some(msg);
                        }
                        if let Ok(OutputControl::Shutdown) = ctrl.recv_timeout(backoff) {
                            return;
                        }
                        backoff = (backoff * 2).min(REBUILD_RETRY_MAX);
                    }
                }
            }
        }

        // Control channel closed without a Shutdown (all senders dropped); guard
        // this teardown too — cpal can panic dropping a stream on a gone device.
        drop_stream(current.take());
    })
}

/// Run `f`, catching panics, with the panic hook silenced for its duration.
///
/// cpal `.unwrap()`s WASAPI calls on a lost device. `catch_unwind` stops the crash
/// but the hook still prints the backtrace first; we suppress it only for this
/// build/teardown window, then restore the host app's hook.
fn quietly<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    panic::set_hook(prev);
    result
}

/// `install`, but a cpal stream-setup panic (it `.unwrap()`s WASAPI calls that fail
/// once the device is gone) becomes a recoverable error — otherwise it unwinds the
/// supervisor and ends all recovery. A caught panic just retries like any failed build.
fn install_caught(
    state: &Arc<SharedState>,
    ctrl_tx: &Sender<OutputControl>,
    bindings: &Sender<AudioBinding>,
    taps: &Sender<TapReader>,
    buffer_ms: usize,
    tap_capacity: usize,
) -> Result<(Stream, StreamInfo)> {
    quietly(|| install(state, ctrl_tx, bindings, taps, buffer_ms, tap_capacity)).unwrap_or_else(
        |_| {
            Err(VoxError::Device(
                "stream setup panicked (device lost)".into(),
            ))
        },
    )
}

/// Drop a cpal stream without its teardown taking down the caller: `Stream::drop`
/// `.unwrap()`s a `SetEvent` that returns `E_HANDLE` once the device is unplugged.
/// We're discarding it anyway, so swallow the panic.
fn drop_stream(stream: Option<Stream>) {
    if let Some(s) = stream {
        let _ = quietly(move || drop(s));
    }
}

/// Resolve the system default output to a *concrete* enumerated device.
///
/// On Windows, cpal's `default_output_device()` is a virtual handle that activates via
/// `ActivateAudioInterfaceAsync` — MTA-only, so it fails with `RPC_E_CHANGED_MODE` on
/// cpal's STA threads and breaks rebuilds. A concrete enumerated device activates via
/// STA-safe `IMMDevice::Activate`. We match the default by id; the watchdog covers
/// default changes, so we don't need cpal's auto-rerouting. Falls back to the default.
fn default_output_concrete(host: &cpal::Host) -> Result<cpal::Device> {
    let default = host
        .default_output_device()
        .ok_or_else(|| VoxError::Device("no output device".into()))?;

    let default_id = default.id().ok().map(|d| d.id().to_string());
    if let Some(id) = default_id.as_deref()
        && let Ok(devices) = host.output_devices()
    {
        for d in devices {
            if d.id().ok().map(|x| x.id().to_string()).as_deref() == Some(id) {
                return Ok(d);
            }
        }
    }
    Ok(default)
}

/// Build and start a stream, then ship the fresh ring buffer and tap to their owners.
fn install(
    state: &Arc<SharedState>,
    ctrl_tx: &Sender<OutputControl>,
    bindings: &Sender<AudioBinding>,
    taps: &Sender<TapReader>,
    buffer_ms: usize,
    tap_capacity: usize,
) -> Result<(Stream, StreamInfo)> {
    let (stream, binding, tap, info) = build_stream(state, ctrl_tx, buffer_ms, tap_capacity)?;
    state.set_output_format(info.sample_rate, info.channels);
    state.bump_rebuild_generation(); // lets the watchdog re-baseline post-rebuild
    let _ = taps.send(tap);
    let _ = bindings.send(binding);
    Ok((stream, info))
}

fn build_stream(
    state: &Arc<SharedState>,
    ctrl_tx: &Sender<OutputControl>,
    buffer_ms: usize,
    tap_capacity: usize,
) -> Result<(Stream, AudioBinding, TapReader, StreamInfo)> {
    // Fresh host per (re)build: a reused host can keep handing back a stale device
    // snapshot, leaving the retry loop unable to recover. A fresh one re-enumerates.
    let host = cpal::default_host();
    let device = default_output_concrete(&host)?;

    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".into());

    let config = device
        .default_output_config()
        .map_err(|e| VoxError::Device(e.to_string()))?;

    let output_rate = config.sample_rate();
    let output_channels = config.channels() as usize;
    let stream_config: StreamConfig = config.into();

    let buffer_size = output_rate as usize * output_channels * buffer_ms / 1000;
    let (producer, mut consumer) = RingBuffer::<f32>::new(buffer_size);
    let (mut tap_writer, tap_reader) = tap::new_tap(tap_capacity);

    let cb_state = Arc::clone(state);
    let err_ctrl = ctrl_tx.clone();
    let mut was_seeking = false;
    let fade_total = output_rate as usize * output_channels * SEEK_FADE_MS / 1000;
    let mut fade_remaining: usize = 0;

    // Per-sample one-pole toward the live target gain. Seed from the current
    // volume so a rebuild mid-playback doesn't ramp up from silence.
    let vol_coeff = 1.0 - (-1.0 / (output_rate as f32 * VOLUME_RAMP_MS / 1000.0)).exp();
    let mut current_gain = state.volume_gain();

    let stream = device
        .build_output_stream(
            stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                cb_state.bump_callback(); // heartbeat for the watchdog

                let is_seeking = cb_state.is_seeking();
                let is_rebuilding = cb_state.is_rebuilding();
                let stopped = !cb_state.is_active();
                let is_inactive = cb_state.is_paused() || stopped || is_seeking || is_rebuilding;

                if is_inactive {
                    if is_seeking || is_rebuilding || stopped {
                        let epoch = cb_state.seek_epoch();
                        if !is_seeking || !cb_state.seek_acked() {
                            let available = consumer.slots();
                            if available > 0
                                && let Ok(chunk) = consumer.read_chunk(available)
                            {
                                chunk.commit_all();
                            }
                        }
                        cb_state.ack_seek(epoch);
                    }
                    data.fill(0.0);
                    was_seeking = is_seeking;
                    return;
                }

                if was_seeking && !is_seeking {
                    fade_remaining = fade_total;
                }
                was_seeking = is_seeking;

                let target_gain = cb_state.volume_gain();
                let available = consumer.slots();
                let to_read = available.min(data.len());
                if to_read > 0
                    && let Ok(chunk) = consumer.read_chunk(to_read)
                {
                    let (first, second) = chunk.as_slices();
                    for (i, &s) in first.iter().chain(second.iter()).enumerate() {
                        let mut sample = s;
                        if fade_remaining > 0 {
                            let progress = 1.0 - (fade_remaining as f32 / fade_total as f32);
                            sample *= progress;
                            fade_remaining -= 1;
                        }
                        current_gain += (target_gain - current_gain) * vol_coeff;
                        sample *= current_gain;
                        data[i] = sample;
                    }
                    chunk.commit_all();
                }
                data[to_read..].fill(0.0);

                if to_read > 0 {
                    cb_state.add_samples(to_read as u64);
                }

                tap_writer.push(data);
            },
            // ponytail: best-effort error-path send, not RT-critical. The error
            // callback can run on cpal's audio thread; unbounded send may
            // allocate, but this path is terminal/rare. Routes hard-loss
            // recovery directly instead of flagging an atomic the watchdog polls.
            move |_e| {
                let _ = err_ctrl.send(OutputControl::Rebuild(RebindReason::HardLoss));
            },
            None,
        )
        .map_err(|e| VoxError::Device(e.to_string()))?;
    stream.play().map_err(|e| VoxError::Device(e.to_string()))?;

    Ok((
        stream,
        AudioBinding {
            producer,
            output_rate,
            output_channels,
        },
        tap_reader,
        StreamInfo {
            name,
            sample_rate: output_rate,
            channels: output_channels,
        },
    ))
}
