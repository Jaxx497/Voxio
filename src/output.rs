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
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

const SEEK_FADE_MS: usize = 30;
/// How long the supervisor waits between rebuild attempts while the device is lost.
const REBUILD_RETRY: Duration = Duration::from_millis(500);

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
        let host = cpal::default_host();

        let (stream, info) = match install(
            &host,
            &state,
            &ctrl_tx,
            &bindings,
            &taps,
            buffer_ms,
            tap_capacity,
        ) {
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
                return;
            };

            state.set_rebuilding(true);
            current.take(); // drop old stream first; cpal joins its audio thread

            // Drain duplicate Rebuilds queued by the old stream's error callback
            // (it can fire repeatedly before the join completes). Stop at, never
            // drop, Shutdown — else teardown hangs.
            while let Ok(msg) = ctrl.try_recv() {
                if let OutputControl::Shutdown = msg {
                    return;
                }
            }

            let mut lost_announced = false;
            loop {
                match install(
                    &host,
                    &state,
                    &ctrl_tx,
                    &bindings,
                    &taps,
                    buffer_ms,
                    tap_capacity,
                ) {
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
                    Err(_) => {
                        // Announce loss once per episode, not per retry.
                        if !lost_announced {
                            let _ = events.try_send(VoxEvent::DeviceLost { name: name.clone() });
                            lost_announced = true;
                        }
                        if let Ok(OutputControl::Shutdown) = ctrl.recv_timeout(REBUILD_RETRY) {
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// Build and start a stream, then ship the fresh ring buffer and tap to their owners.
fn install(
    host: &cpal::Host,
    state: &Arc<SharedState>,
    ctrl_tx: &Sender<OutputControl>,
    bindings: &Sender<AudioBinding>,
    taps: &Sender<TapReader>,
    buffer_ms: usize,
    tap_capacity: usize,
) -> Result<(Stream, StreamInfo)> {
    let (stream, binding, tap, info) = build_stream(host, state, ctrl_tx, buffer_ms, tap_capacity)?;
    state.set_output_format(info.sample_rate, info.channels);
    let _ = taps.send(tap);
    let _ = bindings.send(binding);
    Ok((stream, info))
}

fn build_stream(
    host: &cpal::Host,
    state: &Arc<SharedState>,
    ctrl_tx: &Sender<OutputControl>,
    buffer_ms: usize,
    tap_capacity: usize,
) -> Result<(Stream, AudioBinding, TapReader, StreamInfo)> {
    let device = host
        .default_output_device()
        .ok_or_else(|| VoxError::Device("no output device".into()))?;

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
