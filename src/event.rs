use crate::{VoxError, state::SharedState};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use std::{path::PathBuf, sync::Arc, time::Duration};

/// Events emitted by the engine during playback lifecycle.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum VoxEvent {
    /// A track began playing.
    TrackStarted {
        path: PathBuf,
        duration: Duration,
        sample_rate: u32,
        channels: usize,
        reason: StartReason,
    },
    /// Always emitted *before* `is_active()` turns false.
    TrackEnded { path: PathBuf, reason: EndReason },
    /// Engine idle. Nothing playing, nothing queued.
    Stopped,
    /// A `set_next` call was successful.
    NextReady {
        path: PathBuf,
        duration: Duration,
        sample_rate: u32,
        channels: usize,
    },
    /// A more accurate duration replaced the initial estimate (a headerless MP3
    /// resolved by a background scan, or a track that decoded short of its
    /// reported length). [`Vox::duration`](crate::Vox::duration) reflects it.
    DurationResolved { path: PathBuf, duration: Duration },
    /// An error occurred. `recoverable` is `true` if playback continues.
    Error { error: VoxError, recoverable: bool },
    /// The output stream was rebuilt against a (possibly new) device.
    DeviceChanged {
        name: String,
        sample_rate: u32,
        channels: usize,
        reason: RebindReason,
    },
    /// A rebuild failed; the engine keeps retrying.
    DeviceLost { name: String },
    /// Playback was paused or resumed.
    StateChanged { paused: bool },
}

/// Why the output stream was rebuilt.
///
/// Note: on Windows (WASAPI), switching the default output device invalidates
/// the active stream, so the error callback fires before the default-device
/// change is observed. Such switches are therefore usually reported as
/// [`HardLoss`](Self::HardLoss) rather than [`DefaultChanged`](Self::DefaultChanged).
/// The rebuild itself is correct regardless of which reason is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebindReason {
    /// The stream reported an error (e.g. device unplugged). On Windows this
    /// also covers most default-device switches — see the note above.
    HardLoss,
    /// The OS default output device changed.
    DefaultChanged,
    /// The stream silently stopped invoking its callback.
    StreamZombie,
}

/// Why a track started playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartReason {
    /// Started by a [`play`](crate::Vox::play) call.
    Play,
    /// Started by a gapless handoff from the previous track.
    Gapless,
}

/// Why a track ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// Played through to the end.
    EndOfStream,
    /// Cut short by a stop or a new track.
    Interrupted,
    /// Ended because of an error.
    Failed,
}

/// Receiving half of the engine's event stream. Wraps the channel so callers
/// never need `crossbeam` in their own `Cargo.toml`.
pub struct VoxEvents {
    rx: Receiver<VoxEvent>,
    state: Arc<SharedState>,
}

impl VoxEvents {
    pub(crate) fn new(rx: Receiver<VoxEvent>, state: Arc<SharedState>) -> Self {
        VoxEvents { rx, state }
    }

    /// Returns the next pending event without blocking, or `None`.
    pub fn try_recv(&self) -> Option<VoxEvent> {
        self.rx.try_recv().ok()
    }

    /// Blocks until the next event, returning `None` once the engine shuts down.
    pub fn recv(&self) -> Option<VoxEvent> {
        self.rx.recv().ok()
    }

    /// Blocking receive with a timeout.
    pub fn recv_timeout(&self, d: Duration) -> Option<VoxEvent> {
        self.rx.recv_timeout(d).ok()
    }

    /// Blocking receive that gives up once playback is over: `None` when the
    /// engine is inactive with nothing queued.
    pub fn recv_active(&self, tick: Duration) -> Option<VoxEvent> {
        loop {
            match self.rx.recv_timeout(tick) {
                Ok(ev) => return Some(ev),
                Err(RecvTimeoutError::Timeout) if self.state.is_active() => {}
                _ => return self.rx.try_recv().ok(),
            }
        }
    }

    /// The raw event receiver, for use with `crossbeam_channel::select!` or
    /// other multi-channel primitives. Callers that use this directly should
    /// avoid mixing it with `recv`, `try_recv`, `recv_timeout`, or `recv_active`
    /// on the same `VoxEvents` — the receiver is shared and races are possible
    pub fn receiver(&self) -> &Receiver<VoxEvent> {
        &self.rx
    }
}
