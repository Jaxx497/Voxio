//! A lightweight, responsive audio playback library.
//!
//! [`Vox`] is the engine handle: open it, play files, and drive playback
//! (pause, resume, seek, gapless next). Lifecycle [`VoxEvent`]s arrive on the
//! [`VoxEvents`] receiver returned alongside the handle, and the engine
//! transparently recovers from output-device changes.
//!
//! # Examples
//!
//! ```no_run
//! use std::time::Duration;
//! use voxio::{Vox, VoxEvent};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let (vox, events) = Vox::new()?;
//! vox.play("track.mp3")?;
//! vox.set_next("track2.flac")?; // prime the next track for a gapless handoff
//!
//! while let Some(event) = events.recv_active(Duration::from_millis(50)) {
//!     if let VoxEvent::TrackStarted { path, .. } = event {
//!         println!("Now playing: {}", path.display());
//!     }
//! }
//! # Ok(())
//! # }
//! ```

mod command;
mod config;
mod decoder;
mod error;
mod event;
mod output;
mod resampler;
mod state;
mod tap;
mod watchdog;

use crate::{
    command::{SeekPosition, VoxCommand},
    error::Result,
    output::OutputControl,
    state::SharedState,
};
pub use crate::{
    config::VoxConfig,
    decoder::ReplayGainMode,
    error::VoxError,
    event::{EndReason, RebindReason, StartReason, VoxEvent, VoxEvents},
    tap::TapHandle,
};
use crossbeam_channel::{self, Sender};
use std::{path::Path, sync::Arc, thread::JoinHandle, time::Duration};

const CHANNEL_COUNT: usize = 32;
const EVENT_CAPACITY: usize = 256;

/// The engine handle.
///
/// `Vox` is `Send` but not `Sync`: own it from a single thread (or move it
/// between threads), driving playback through it there. To work from other
/// threads, use the handles it hands out — the [`VoxEvents`] receiver and the
/// [`TapHandle`] from [`take_tap`](Self::take_tap) are both `Send` and can move
/// to their own threads.
pub struct Vox {
    state: Arc<SharedState>,
    commands: Sender<VoxCommand>,
    output_ctrl: Sender<OutputControl>,
    tap: Option<TapHandle>,
    threads: [Option<JoinHandle<()>>; 3],
}

// Fail build if Vox somehow becomes !Send
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Vox>();
};

impl Vox {
    /// Build the engine: spawns the audio supervisor, decoder worker, and
    /// watchdog threads. Returns a [`Vox`] handle and an [`VoxEvents`] receiver.
    fn init(config: VoxConfig) -> Result<(Vox, VoxEvents)> {
        let state = Arc::new(SharedState::default());

        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(CHANNEL_COUNT);
        let (event_tx, event_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
        let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
        let (binding_tx, binding_rx) = crossbeam_channel::unbounded();
        let (tap_tx, tap_rx) = crossbeam_channel::unbounded();
        let (init_tx, init_rx) = crossbeam_channel::bounded(1);

        let supervisor = output::spawn(
            Arc::clone(&state),
            ctrl_rx,
            ctrl_tx.clone(),
            event_tx.clone(),
            binding_tx,
            tap_tx,
            init_tx,
            config.buffer_ms,
            config.tap_capacity,
        );

        // The supervisor builds the first (!Send) stream on its thread; surface
        // its result here so device-init failures reach the caller.
        match init_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = supervisor.join();
                return Err(e);
            }
            Err(_) => {
                let _ = supervisor.join();
                return Err(VoxError::Device("output supervisor failed".into()));
            }
        }

        let tap = match tap_rx.recv() {
            Ok(tap) => tap,
            Err(_) => {
                let _ = supervisor.join();
                return Err(VoxError::Device("output supervisor failed".into()));
            }
        };

        let worker = command::spawn(cmd_rx, binding_rx, event_tx, Arc::clone(&state));
        let watchdog = watchdog::spawn(
            Arc::clone(&state),
            ctrl_tx.clone(),
            config.watchdog_tick,
            config.zombie_ticks,
        );
        let events = VoxEvents::new(event_rx, Arc::clone(&state));
        let tap = TapHandle::new(tap, tap_rx, Arc::clone(&state));

        Ok((
            Vox {
                state,
                commands: cmd_tx,
                output_ctrl: ctrl_tx,
                tap: Some(tap),
                threads: [Some(worker), Some(supervisor), Some(watchdog)],
            },
            events,
        ))
    }

    /// Builds the engine with default configuration.
    ///
    /// # Errors
    ///
    /// Returns [`VoxError::Device`] if the output device cannot be initialized.
    pub fn new() -> Result<(Vox, VoxEvents)> {
        Self::init(VoxConfig::default())
    }

    /// Builds the engine with custom configuration.
    ///
    /// # Errors
    ///
    /// Returns [`VoxError::Device`] if the output device cannot be initialized.
    pub fn new_with_config(config: VoxConfig) -> Result<(Vox, VoxEvents)> {
        Self::init(config)
    }

    /// Plays an audio track from a filesystem path.
    ///
    /// [`is_active`](Self::is_active) is `true` as soon as this returns `Ok`,
    /// so a `while vox.is_active()` wait loop can follow a `play` directly.
    ///
    /// Interrupting a playing track emits
    /// [`TrackEnded`](VoxEvent::TrackEnded) with
    /// [`EndReason::Interrupted`] for the old track *before*
    /// [`TrackStarted`](VoxEvent::TrackStarted) for the new one; calling `play`
    /// on an idle engine emits only `TrackStarted`. Consumers that wait for
    /// `TrackStarted` should skip the intervening `TrackEnded`.
    ///
    /// # Errors
    ///
    /// Returns [`VoxError::FileOpen`] if the path is not a readable file, or
    /// [`VoxError::ChannelClosed`] if the engine has shut down.
    pub fn play<S: AsRef<str>>(&self, s: S) -> Result<()> {
        let input = s.as_ref();

        if !Path::new(input).is_file() {
            return Err(VoxError::FileOpen(input.to_string()));
        }

        self.state.set_active(true);
        self.send(VoxCommand::Play(input.to_string()))
    }

    /// Pauses playback.
    pub fn pause(&self) {
        self.dispatch(VoxCommand::Pause);
    }

    /// Resumes playback.
    pub fn resume(&self) {
        self.dispatch(VoxCommand::Resume);
    }

    /// Stops all playback and clears any queued next track.
    pub fn stop(&self) {
        self.dispatch(VoxCommand::Stop);
    }

    /// Seeks to an absolute position, in seconds.
    ///
    /// Seeking past the end of the track ends it (EOF).
    pub fn seek_to(&self, secs: f64) {
        self.dispatch(VoxCommand::Seek(SeekPosition::Absolute(secs)));
    }

    /// Seeks relative to the current position, in seconds.
    ///
    /// Seeking past the end of the track ends it (EOF).
    pub fn seek_relative(&self, delta: f64) {
        self.dispatch(VoxCommand::Seek(SeekPosition::Relative(delta)));
    }

    /// Primes a track for gapless playback after the current one.
    ///
    /// Requires an active track: the primed track transitions from the current
    /// one, so [`is_active`](Self::is_active) must be `true` (playing or paused).
    ///
    /// This is **not** a queue: each call overwrites the previously primed
    /// track. Once a transition takes place the primed track is cleared, and
    /// starting a new track with [`play`](Self::play) also discards it.
    ///
    /// # Errors
    ///
    /// Returns [`VoxError::NotActive`] if no track is playing or paused,
    /// [`VoxError::FileOpen`] if the path is not a readable file, or
    /// [`VoxError::ChannelClosed`] if the engine has shut down.
    pub fn set_next<S: AsRef<str>>(&self, s: S) -> Result<()> {
        if !self.is_active() {
            return Err(VoxError::NotActive);
        }

        let path = s.as_ref();
        if !Path::new(path).is_file() {
            return Err(VoxError::FileOpen(path.to_string()));
        }

        self.send(VoxCommand::QueueNext(path.to_string()))
    }

    /// Clears any track primed via [`set_next`](Self::set_next).
    pub fn clear_next(&self) {
        self.dispatch(VoxCommand::ClearNext);
    }

    /// Sets the ReplayGain mode. Takes effect immediately for the current track and
    /// all subsequent tracks. Untagged tracks play at unity gain.
    pub fn set_replaygain(&self, mode: ReplayGainMode) {
        self.dispatch(VoxCommand::ReplayGain(mode));
    }

    /// Takes the visualization tap as a standalone [`TapHandle`].
    ///
    /// Returned once; `None` on subsequent calls. The handle is `Send`, so move
    /// it to your render/visualization thread and poll [`TapHandle::latest`].
    pub fn take_tap(&mut self) -> Option<TapHandle> {
        self.tap.take()
    }

    /// Returns `true` while a track is playing or paused.
    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    /// Returns `true` while playback is paused.
    pub fn is_paused(&self) -> bool {
        self.state.is_paused()
    }

    /// Returns the playback position as a [`Duration`].
    pub fn position(&self) -> Duration {
        let sps = self.state.output_rate() as f64 * self.state.output_channels() as f64;
        if sps > 0.0 {
            Duration::from_secs_f64(self.state.get_samples() as f64 / sps)
        } else {
            Duration::ZERO
        }
    }

    /// Playable duration of the current track, excluding decoder delay/padding.
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.state.get_duration_secs())
    }

    /// Returns the position as a raw sample count.
    ///
    /// The unit is **interleaved samples at the current output rate** — each
    /// channel counted separately, the same unit the sample tap delivers.
    /// Shares its unit with [`duration_samples`](Self::duration_samples), so
    /// `position_samples() as f64 / duration_samples() as f64` is the progress
    /// fraction. Divide by `sample_rate() * channels()` for seconds, or by
    /// `channels()` for per-channel frames.
    ///
    /// Tracks the current output device, so the raw value rescales after a
    /// device change (as does `duration_samples`, keeping the ratio stable).
    pub fn position_samples(&self) -> u64 {
        self.state.get_samples()
    }

    /// Playable duration as a raw sample count, in the same unit as
    /// [`position_samples`](Self::position_samples) — interleaved samples at
    /// the current output rate. `0` when no track is loaded.
    pub fn duration_samples(&self) -> u64 {
        let sps = self.state.output_rate() as f64 * self.state.output_channels() as f64;
        (self.state.get_duration_secs() * sps) as u64
    }

    /// Live output sample rate. Stays current across device rebuilds.
    pub fn sample_rate(&self) -> u32 {
        self.state.output_rate()
    }

    /// Number of interleaved channels in the audio output.
    /// Stays current across device rebuilds.
    pub fn channels(&self) -> usize {
        self.state.output_channels()
    }

    /// Dispatch a command, surfacing a dead engine to the caller. Used by
    /// `play`, where "couldn't reach the engine" is worth reporting.
    fn send(&self, cmd: VoxCommand) -> Result<()> {
        self.commands.send(cmd).map_err(|_| VoxError::ChannelClosed)
    }

    /// Fire-and-forget dispatch for state commands whose only failure mode is
    /// a dead engine — unreachable while `Vox` is alive, so nothing to report.
    fn dispatch(&self, cmd: VoxCommand) {
        let _ = self.commands.send(cmd);
    }
}

impl Drop for Vox {
    fn drop(&mut self) {
        self.state.set_shutdown(); // unblock busy loops first
        let _ = self.commands.send(VoxCommand::Shutdown);
        let _ = self.output_ctrl.send(OutputControl::Shutdown);
        for handle in &mut self.threads {
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Instant};

    const BAD_PATH: &str = "tests/test_suite/";
    const NONEXISTENT_FILE: &str = "tests/test_suite/nonexistant.flac";
    const TEXT_FILE: &str = "tests/test_suite/invalid_filetype.txt";
    const LEGAL_FILE: &str = "tests/test_suite/Preludes, Op. 28 - No. 16 'Hades'.mp3";

    fn fresh_vox() -> (Vox, VoxEvents) {
        Vox::new().unwrap()
    }

    fn await_event(events: &VoxEvents) -> Option<VoxEvent> {
        events.recv_timeout(Duration::from_millis(500))
    }

    /// Like `await_event`, but skips watchdog `DeviceChanged` events, which can
    /// fire spuriously under the test audio backend (e.g. WSLg) and otherwise
    /// race the events a test actually cares about.
    fn await_event_ignoring_device(events: &VoxEvents) -> Option<VoxEvent> {
        loop {
            match await_event(events) {
                Some(VoxEvent::DeviceChanged { .. }) => continue,
                other => return other,
            }
        }
    }

    /// Wait until `position()` lands near `target` (i.e. the worker processed the
    /// seek), or time out. Polling on "position changed" alone is racy: normal
    /// playback also advances position, so a forward seek can otherwise return a
    /// mid-playback value before the seek lands. Gating on target proximity — and
    /// the same 0.1s window the callers assert with — avoids that.
    fn await_seek(v: &Vox, target: f64) -> f64 {
        let deadline = Instant::now() + Duration::from_millis(2000);
        loop {
            let pos = v.position().as_secs_f64();
            if (pos - target).abs() < 0.1 || Instant::now() > deadline {
                return pos;
            }
            thread::sleep(Duration::from_micros(500));
        }
    }

    #[test]
    fn passed_directory() {
        let (v, _e) = fresh_vox();
        let res = v.play(BAD_PATH);

        assert_eq!(res, Err(VoxError::FileOpen(BAD_PATH.to_string())));
    }

    #[test]
    fn nonexistent_file() {
        let (v, _e) = fresh_vox();
        let res = v.play(NONEXISTENT_FILE);
        assert_eq!(res, Err(VoxError::FileOpen(NONEXISTENT_FILE.to_string())));
    }

    #[test]
    fn text_file() {
        let (v, events) = fresh_vox();
        let res = v.play(TEXT_FILE);
        // The file was opened successfully
        assert_eq!(res, Ok(()));

        let event = await_event_ignoring_device(&events);
        // File was not decoded
        assert!(
            matches!(
                event,
                Some(VoxEvent::Error {
                    error: VoxError::Decoder(_),
                    recoverable: true
                })
            ),
            "expected recoverable decode error, got {event:?}"
        );
    }

    #[test]
    fn empty_path() {
        let (v, _e) = fresh_vox();
        let res = v.play("");
        assert_eq!(res, Err(VoxError::FileOpen("".to_string())));
    }

    #[test]
    fn legal_file() {
        let (v, events) = fresh_vox();
        let res = v.play(LEGAL_FILE);
        assert_eq!(res, Ok(()));

        let event = await_event_ignoring_device(&events);
        assert!(
            matches!(event, Some(VoxEvent::TrackStarted { .. })),
            "expected TrackStarted, got {event:?}"
        );
    }

    #[test]
    fn pause_and_resume_events() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        v.pause();
        let event = await_event_ignoring_device(&events);
        assert!(matches!(
            event,
            Some(VoxEvent::StateChanged { paused: true })
        ));

        v.resume();
        let event = await_event_ignoring_device(&events);
        assert!(matches!(
            event,
            Some(VoxEvent::StateChanged { paused: false })
        ));
    }

    #[test]
    fn stop_emits_track_ended() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        v.stop();
        let event = await_event_ignoring_device(&events);
        assert!(
            matches!(
                event,
                Some(VoxEvent::TrackEnded {
                    reason: EndReason::Interrupted,
                    ..
                })
            ),
            "expected TrackEnded(Interrupted), got {event:?}"
        );
    }

    #[test]
    fn set_next_valid_file() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        v.set_next(LEGAL_FILE).unwrap();
        let event = await_event_ignoring_device(&events);
        assert!(
            matches!(event, Some(VoxEvent::NextReady { .. })),
            "expected NextReady, got {event:?}"
        );
    }

    #[test]
    fn set_next_when_idle_errors() {
        let (v, _e) = fresh_vox();
        // Nothing playing → set_next has no track to queue after.
        assert_eq!(v.set_next(LEGAL_FILE), Err(VoxError::NotActive));
    }

    #[test]
    fn set_next_invalid_file() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        v.set_next("tests/test_suite/invalid_filetype.txt").unwrap();
        let event = await_event_ignoring_device(&events);
        assert!(
            matches!(
                event,
                Some(VoxEvent::Error {
                    error: VoxError::Decoder(_),
                    recoverable: true
                })
            ),
            "expected recoverable Decoder error, got {event:?}"
        );
    }

    #[test]
    fn clear_next_suppresses_gapless() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        v.set_next(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard NextReady

        v.clear_next();

        // Seek past end — should end the track without a gapless transition.
        let dur = v.duration();
        v.seek_to(dur.as_secs_f64() + 1.0);
        let event = await_event_ignoring_device(&events);
        assert!(
            matches!(
                event,
                Some(VoxEvent::TrackEnded {
                    reason: EndReason::EndOfStream,
                    ..
                })
            ),
            "expected TrackEnded(EndOfStream), got {event:?}"
        );

        // Stopped fires after TrackEnded
        let stop = await_event_ignoring_device(&events);
        assert!(
            matches!(stop, Some(VoxEvent::Stopped)),
            "expected Stopped, got {stop:?}"
        );

        // No gapless transition: drain any remaining events (a watchdog rebind
        // may emit DeviceChanged) and assert none is a TrackStarted.
        loop {
            match await_event_ignoring_device(&events) {
                Some(VoxEvent::TrackStarted { .. }) => panic!("unexpected gapless transition"),
                Some(_) => continue,
                None => break,
            }
        }
        assert!(!v.is_active())
    }

    #[test]
    fn seek_past_end_triggers_gapless() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        v.set_next(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard NextReady

        let dur = v.duration();
        v.seek_to(dur.as_secs_f64() + 1.0);

        let event = await_event_ignoring_device(&events);
        assert!(
            matches!(
                event,
                Some(VoxEvent::TrackEnded {
                    reason: EndReason::EndOfStream,
                    ..
                })
            ),
            "expected TrackEnded(EndOfStream), got {event:?}"
        );

        let event = await_event_ignoring_device(&events);
        assert!(
            matches!(
                event,
                Some(VoxEvent::TrackStarted {
                    reason: StartReason::Gapless,
                    ..
                })
            ),
            "expected TrackStarted(Gapless), got {event:?}"
        );
    }

    #[test]
    fn seek_forward_5s() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        v.seek_to(5.0);
        let pos = await_seek(&v, 5.0);

        assert!(
            (4.9..5.1).contains(&pos),
            "expected position near 5.0, got {pos}"
        );
    }

    #[test]
    fn seek_negative_clamps_to_zero() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        v.seek_to(-5.0);
        let pos = await_seek(&v, 0.0);

        assert!(
            (0.0..0.1).contains(&pos),
            "expected position near 0.0, got {pos}"
        );
    }

    #[test]
    fn seek_relative_negative() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        // Start at position 0, seek relative by a negative amount → clamps to 0.
        v.seek_relative(-10.0);
        let pos = await_seek(&v, 0.0);

        assert!(
            (0.0..0.1).contains(&pos),
            "expected position near 0.0 after negative relative seek, got {pos}"
        );
    }

    #[test]
    fn seek_past_end_no_next() {
        let (v, events) = fresh_vox();
        v.play(LEGAL_FILE).unwrap();
        await_event_ignoring_device(&events); // discard TrackStarted

        let dur = v.duration();
        v.seek_to(dur.as_secs_f64() + 10.0);

        let event = await_event_ignoring_device(&events);
        assert!(
            matches!(
                event,
                Some(VoxEvent::TrackEnded {
                    reason: EndReason::EndOfStream,
                    ..
                })
            ),
            "expected TrackEnded(EndOfStream), got {event:?}"
        );

        // No next track → stop_playback, no TrackStarted (gapless).
        // Drain a few events (watchdog may emit DeviceChanged).
        loop {
            match await_event_ignoring_device(&events) {
                Some(VoxEvent::TrackStarted { .. }) => panic!("unexpected gapless transition"),
                Some(_) => continue,
                None => break,
            }
        }
    }

    #[test]
    fn monster_command_sequence() {
        let (v, events) = fresh_vox();

        // 1. Play
        v.play(LEGAL_FILE).unwrap();
        let e = await_event_ignoring_device(&events);
        assert!(matches!(e, Some(VoxEvent::TrackStarted { .. })));

        // 2. Pause
        v.pause();
        let e = await_event_ignoring_device(&events);
        assert!(matches!(e, Some(VoxEvent::StateChanged { paused: true })));

        // 3. Seek to 2 s while paused
        v.seek_to(12.0);
        let pos = await_seek(&v, 12.0);
        assert!(
            (11.9..12.1).contains(&pos),
            "seek after pause: expected ~12.0, got {pos}"
        );

        // 4. Queue next
        v.set_next(LEGAL_FILE).unwrap();
        let e = await_event_ignoring_device(&events);
        assert!(matches!(e, Some(VoxEvent::NextReady { .. })));

        // 5. Resume
        v.resume();
        let e = await_event_ignoring_device(&events);
        assert!(matches!(e, Some(VoxEvent::StateChanged { paused: false })));

        // 6. Relative seek back by 4 s (from ~12 → ~8)
        v.seek_relative(-4.0);
        let pos = await_seek(&v, 8.0);
        assert!(
            (7.9..8.2).contains(&pos),
            "second seek: expected ~8.0, got {pos}"
        );
    }
}
