use crate::{
    decoder::{ReplayGainMode, VoxDecoder},
    error::{Result, VoxError},
    event::{EndReason, StartReason, VoxEvent},
    output::AudioBinding,
    resampler::VoxResampler,
    scan,
    state::SharedState,
};
use crossbeam_channel::{Receiver, RecvTimeoutError, Select, Sender, TryRecvError};
use rtrb::Producer;
use std::{
    path::PathBuf,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_MP3, CODEC_ID_OPUS, CODEC_ID_VORBIS,
};

/// How often the idle worker wakes to adopt pending rebinds.
const IDLE_POLL: Duration = Duration::from_millis(100);
const SEEK_PREFILL_MS: usize = 40;
const SEEK_POST_SKIP_MS: usize = 20;
const PENDING_CAPACITY: usize = 8192;

pub(crate) enum SeekPosition {
    Absolute(f64),
    Relative(f64),
}

pub(crate) enum VoxCommand {
    Play(String),
    QueueNext(String),
    ClearNext,
    Seek(SeekPosition),
    Pause,
    Resume,
    ReplayGain(ReplayGainMode),
    Stop,
    Shutdown,
}

enum Flow {
    Continue,
    Shutdown,
}

/// Whether `handle_play` should decode-and-fill the ring immediately.
enum Prefill {
    /// Track start with no pending seek — fill so playback begins at once.
    Now,
    /// A coalesced seek will drain+prefill at its target; filling now is
    /// wasted decode that gets thrown away.
    Defer,
}

/// `Now` unless a seek is coalesced in the same batch — then `Defer`, so the
/// seek drains+prefills at its target instead of decoding from track start.
fn seek_prefill(pending_seek: &Option<f64>) -> Prefill {
    match pending_seek {
        Some(_) => Prefill::Defer,
        None => Prefill::Now,
    }
}

struct QueuedSource {
    decoder: VoxDecoder,
    resampler: Option<VoxResampler>,
    input_channels: usize,
}

struct VoxWorker {
    rx: Receiver<VoxCommand>,
    bindings: Receiver<AudioBinding>,
    events: Sender<VoxEvent>,
    producer: Producer<f32>,
    state: Arc<SharedState>,
    output_rate: u32,
    output_channels: usize,

    current: Option<VoxDecoder>,
    next: Option<QueuedSource>,
    resampler: Option<VoxResampler>,
    pending: Vec<f32>,
    input_channels: usize,
    deferred_next: Option<String>,
    rg_mode: ReplayGainMode,
    gain: f32,
    /// Set by any seek on the current track. A seek invalidates the decoder's
    /// `frames_emitted` tally, so end-of-track duration reconciliation is skipped.
    seeked: bool,
}

pub(crate) fn spawn(
    rx: Receiver<VoxCommand>,
    bindings: Receiver<AudioBinding>,
    events: Sender<VoxEvent>,
    state: Arc<SharedState>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // The supervisor ships the first binding before we start.
        let Ok(binding) = bindings.recv() else {
            return;
        };
        VoxWorker::new(rx, bindings, events, state, binding).run();
    })
}

impl VoxWorker {
    fn new(
        rx: Receiver<VoxCommand>,
        bindings: Receiver<AudioBinding>,
        events: Sender<VoxEvent>,
        state: Arc<SharedState>,
        binding: AudioBinding,
    ) -> Self {
        VoxWorker {
            rx,
            bindings,
            events,
            producer: binding.producer,
            state,
            output_rate: binding.output_rate,
            output_channels: binding.output_channels,

            current: None,
            next: None,
            resampler: None,
            pending: Vec::with_capacity(PENDING_CAPACITY),
            input_channels: 2,
            deferred_next: None,
            rg_mode: ReplayGainMode::default(),
            gain: 1.0,
            seeked: false,
        }
    }

    fn run(&mut self) {
        loop {
            match self.step() {
                Ok(Flow::Continue) => {}
                Ok(Flow::Shutdown) => return,
                Err(e) => self.fail_current(e),
            }
        }
    }

    fn step(&mut self) -> Result<Flow> {
        if self.current.is_none() {
            return self.wait_for_command();
        }
        self.apply_bindings()?;
        if let Flow::Shutdown = self.poll_commands()? {
            return Ok(Flow::Shutdown);
        }
        // Don't decode while paused or while the stream is being rebuilt.
        if self.state.is_paused() || self.state.is_rebuilding() {
            self.park_until_activity();
        } else {
            self.decode_handler()?;
        }
        Ok(Flow::Continue)
    }

    /// Block while paused/rebuilding until a command or rebind binding is ready,
    /// or a short safety timeout elapses. Consumes nothing — the next loop
    /// iteration's `apply_bindings`/`poll_commands` drain whatever arrived.
    fn park_until_activity(&self) {
        let mut sel = Select::new();
        sel.recv(&self.rx);
        sel.recv(&self.bindings);
        let _ = sel.ready_timeout(IDLE_POLL);
    }

    fn emit(&self, event: VoxEvent) {
        let _ = self.events.try_send(event);
    }

    /// Mark a freshly-started track: clear the seek flag, bump the epoch (drops a
    /// prior track's in-flight scan), and scan for the true length if the reported
    /// duration is only an estimate.
    fn begin_track(&mut self, decoder: &VoxDecoder) {
        self.seeked = false;
        let epoch = self.state.bump_track_epoch();
        if decoder.duration_estimated() {
            scan::spawn(
                decoder.path().to_string(),
                epoch,
                Arc::clone(&self.state),
                self.events.clone(),
            );
        }
    }

    fn set_paused(&mut self, val: bool) {
        if self.state.is_paused() != val {
            self.state.set_paused(val);
            self.emit(VoxEvent::StateChanged { paused: val });
        }
    }

    fn fail_current(&mut self, error: VoxError) {
        self.emit(VoxEvent::Error {
            error,
            recoverable: false,
        });
        if let Some(current) = &self.current {
            self.emit(VoxEvent::TrackEnded {
                path: current.path().into(),
                reason: EndReason::Failed,
            });
        }
        self.stop_playback();
    }

    /// Recompute linear gain from `current`'s ReplayGain tags
    fn update_gain(&mut self) {
        self.gain = match &self.current {
            Some(d) => d.info.replaygain.linear_gain(self.rg_mode),
            None => 1.0,
        };
    }

    /// Adopt the latest stream rebind: swap in the new ring buffer and format,
    /// rebuild resamplers, and seek back to where playback was.
    fn apply_bindings(&mut self) -> Result<()> {
        let mut latest = None;
        while let Ok(b) = self.bindings.try_recv() {
            latest = Some(b);
        }
        let Some(binding) = latest else {
            return Ok(());
        };

        let resume_at = self.get_elapsed(); // convert with the OLD format first
        self.producer = binding.producer;
        self.output_rate = binding.output_rate;
        self.output_channels = binding.output_channels;
        self.pending.clear();

        if let Some(next) = self.next.as_mut() {
            next.resampler = VoxResampler::new(
                next.decoder.info.sample_rate,
                self.output_rate,
                next.input_channels,
            )?;
        }

        if let Some(current) = self.current.as_ref() {
            self.resampler = VoxResampler::new(
                current.info.sample_rate,
                self.output_rate,
                current.info.channels,
            )?;
            self.handle_seek(resume_at)?;
        }
        Ok(())
    }

    fn wait_for_command(&mut self) -> Result<Flow> {
        match self.rx.recv_timeout(IDLE_POLL) {
            Ok(cmd) => {
                self.apply_bindings()?;
                self.handle_command(cmd)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.apply_bindings()?;
                Ok(Flow::Continue)
            }
            Err(RecvTimeoutError::Disconnected) => Ok(Flow::Shutdown),
        }
    }

    fn poll_commands(&mut self) -> Result<Flow> {
        if let Some(path) = self.deferred_next.take() {
            self.set_next(path);
        }

        let mut pending_seek: Option<f64> = None;
        let mut pending_next: Option<String> = None;
        let mut pending_play: Option<String> = None;

        loop {
            match self.rx.try_recv() {
                Ok(VoxCommand::Play(path)) => {
                    pending_next = None;
                    pending_play = Some(path);
                }
                Ok(VoxCommand::Seek(pos)) => {
                    let base = pending_seek.unwrap_or_else(|| self.get_elapsed());
                    pending_seek = Some(match pos {
                        SeekPosition::Absolute(t) => t,
                        SeekPosition::Relative(d) => base + d,
                    });
                }
                Ok(VoxCommand::QueueNext(path)) => pending_next = Some(path),
                Ok(cmd) => {
                    if let Some(p) = pending_play.take() {
                        self.handle_play(p, seek_prefill(&pending_seek))?;
                    }
                    if let Some(t) = pending_seek.take() {
                        self.handle_seek(t)?;
                    }
                    if let Some(p) = pending_next.take() {
                        self.set_next(p);
                    }
                    if let Flow::Shutdown = self.handle_command(cmd)? {
                        return Ok(Flow::Shutdown);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(Flow::Shutdown),
            }
        }

        // Final coalescing operation
        if let Some(p) = pending_play {
            self.handle_play(p, seek_prefill(&pending_seek))?;
            if let Some(n) = pending_next.take() {
                self.deferred_next = Some(n);
            }
        }

        if let Some(t) = pending_seek {
            self.handle_seek(t)?;
        }

        if let Some(p) = pending_next {
            self.set_next(p);
        }

        Ok(Flow::Continue)
    }

    fn handle_command(&mut self, cmd: VoxCommand) -> Result<Flow> {
        match cmd {
            VoxCommand::Play(path) => self.handle_play(path, Prefill::Now)?,
            VoxCommand::QueueNext(path) => self.set_next(path),
            VoxCommand::ClearNext => self.next = None,
            VoxCommand::Seek(pos) => {
                let target = match pos {
                    SeekPosition::Absolute(t) => t,
                    SeekPosition::Relative(d) => self.get_elapsed() + d,
                };
                self.handle_seek(target)?;
            }
            VoxCommand::Pause => self.set_paused(true),
            VoxCommand::Resume => self.set_paused(false),
            VoxCommand::ReplayGain(mode) => {
                self.rg_mode = mode;
                self.update_gain();
            }
            VoxCommand::Stop => {
                if let Some(current) = &self.current {
                    self.emit(VoxEvent::TrackEnded {
                        path: current.path().into(),
                        reason: EndReason::Interrupted,
                    });
                }
                self.stop_playback();
            }
            VoxCommand::Shutdown => return Ok(Flow::Shutdown),
        }
        Ok(Flow::Continue)
    }

    fn handle_play(&mut self, path: String, prefill: Prefill) -> Result<()> {
        self.state.start_seek();
        self.state.reset_samples();
        self.pending.clear();

        self.next = None;
        self.deferred_next = None;

        if let Some(old) = self.current.take() {
            self.emit(VoxEvent::TrackEnded {
                path: old.path().into(),
                reason: EndReason::Interrupted,
            });
        }

        match VoxDecoder::open(&path) {
            Ok(decoder) => {
                let info = decoder.info.clone();
                let duration = decoder.playable_duration().unwrap_or(0.0);

                self.state.set_duration_secs(duration);
                self.begin_track(&decoder);
                self.input_channels = info.channels;
                self.resampler =
                    VoxResampler::new(info.sample_rate, self.output_rate, info.channels)?;
                self.current = Some(decoder);
                self.update_gain();

                self.state.set_active(true);
                self.set_paused(false);

                if matches!(prefill, Prefill::Now) {
                    self.prefill_after_seek()?;
                }
                self.emit(VoxEvent::TrackStarted {
                    path: path.into(),
                    duration: Duration::from_secs_f64(duration),
                    sample_rate: info.sample_rate,
                    channels: info.channels,
                    reason: StartReason::Play,
                });
            }
            Err(e) => {
                self.state.set_active(false);
                self.emit(VoxEvent::Error {
                    error: e,
                    recoverable: true,
                });
                self.stop_playback();
            }
        }
        self.state.finish_seek();
        Ok(())
    }

    fn set_next(&mut self, path: String) {
        let queued = VoxDecoder::open(&path).and_then(|decoder| {
            let resampler = VoxResampler::new(
                decoder.info.sample_rate,
                self.output_rate,
                decoder.info.channels,
            )?;
            Ok(QueuedSource {
                input_channels: decoder.info.channels,
                decoder,
                resampler,
            })
        });

        match queued {
            Ok(q) => {
                let info = q.decoder.info.clone();
                let duration = q.decoder.playable_duration().unwrap_or(0.0);
                self.emit(VoxEvent::NextReady {
                    path: q.decoder.path().into(),
                    duration: Duration::from_secs_f64(duration),
                    sample_rate: info.sample_rate,
                    channels: info.channels,
                });
                self.next = Some(q);
            }
            Err(e) => self.emit(VoxEvent::Error {
                error: e,
                recoverable: true,
            }),
        }
    }

    fn handle_seek(&mut self, target_secs: f64) -> Result<()> {
        self.seeked = true;
        self.state.start_seek();

        let (duration, input_rate, preroll_frames) = match &self.current {
            Some(d) => (
                d.playable_duration().unwrap_or(f64::MAX),
                d.info.sample_rate,
                d.seek_preroll_frames(),
            ),
            None => {
                self.state.finish_seek();
                return Ok(());
            }
        };

        if target_secs >= duration {
            // Discard stale ring audio (buffered from the pre-seek position)
            // before transitioning, else the listener hears mid-track samples
            // from the old track bleeding into the next one. Mirrors
            // handle_play's teardown: transition_to only clears pending and
            // rebuilds the resampler when the sample rate differs, so a
            // same-rate gapless handoff would otherwise keep stale tail.
            self.pending.clear();
            if let Some(r) = &mut self.resampler {
                r.reset();
            }
            self.wait_for_seek_drain();
            self.state.finish_seek();
            return self.handle_track_end();
        }

        let target_secs = target_secs.max(0.0);

        let decoder = self.current.as_mut().unwrap();
        let codec = decoder.info.codec;

        let landing = match decoder.seek(target_secs) {
            Ok(l) => l,
            Err(e) => {
                self.emit(VoxEvent::Error {
                    error: e,
                    recoverable: true,
                });
                self.state.finish_seek();
                return self.handle_track_end();
            }
        };

        // Lossy codecs need ~one frame of warm-up after a coarse seek to
        // reconverge; PCM-class codecs land exactly at target with no warm-up,
        // so skipping any frames just discards real audio and offsets position.
        // The linear-decode fallback (`needs_warmup == false`) already decoded
        // continuously up to the target, so it needs neither warm-up nor preroll.
        let skip_frames = if landing.needs_warmup {
            let warmup = if matches!(
                codec,
                CODEC_ID_OPUS | CODEC_ID_VORBIS | CODEC_ID_MP3 | CODEC_ID_AAC
            ) {
                (input_rate as u64 * SEEK_POST_SKIP_MS as u64) / 1000
            } else {
                0
            };
            warmup.max(preroll_frames)
        } else {
            0
        };
        let skipped = self.current.as_mut().unwrap().skip_samples(skip_frames)?;
        let actual_secs = landing.secs + skipped as f64 / input_rate as f64;

        self.pending.clear();
        if let Some(r) = &mut self.resampler {
            r.reset();
        }

        let output_samples =
            (actual_secs * self.output_rate as f64 * self.output_channels as f64) as u64;
        self.state.set_samples(output_samples);

        self.prefill_after_seek()?;
        self.state.finish_seek();
        Ok(())
    }

    fn decode_handler(&mut self) -> Result<()> {
        let Some(decoder) = self.current.as_mut() else {
            return Ok(());
        };
        let packet = decoder.next_packet().map(|opt| opt.map(|s| s.to_vec()));

        match packet? {
            Some(s) => self.process_samples(&s),
            None => self.handle_track_end(),
        }
    }

    fn process_samples(&mut self, samples: &[f32]) -> Result<()> {
        match &mut self.resampler {
            Some(r) => {
                self.pending.extend_from_slice(samples);
                let producer = &mut self.producer;
                let state = &self.state;
                let in_ch = self.input_channels;
                let out_ch = self.output_channels;
                let gain = self.gain;
                r.process(&mut self.pending, |s| {
                    push_samples_mapped(producer, state, s, in_ch, out_ch, gain)
                })?;
            }
            None => push_samples_mapped(
                &mut self.producer,
                &self.state,
                samples,
                self.input_channels,
                self.output_channels,
                self.gain,
            ),
        }
        Ok(())
    }

    /// At end of stream, reconcile the reported duration against what actually
    /// decoded — the reactive counterpart to the background scan, catching files
    /// that ended short of their stated length (truncated or over-tagged). Skipped
    /// after a seek, when `frames_emitted` is no longer the true length.
    fn reconcile_duration(&self) {
        if self.seeked {
            return;
        }
        let Some(decoder) = self.current.as_ref() else {
            return;
        };
        let secs = decoder.frames_emitted() as f64 / decoder.info.sample_rate.max(1) as f64;
        scan::publish_correction(&self.state, &self.events, decoder.path().into(), secs);
    }

    fn handle_track_end(&mut self) -> Result<()> {
        self.reconcile_duration();
        self.state.reset_samples();
        let ended = self.current.take().map(|d| PathBuf::from(d.path()));

        match self.next.take() {
            Some(queued) => {
                if let Some(path) = ended {
                    self.emit(VoxEvent::TrackEnded {
                        path,
                        reason: EndReason::EndOfStream,
                    });
                }
                self.transition_to(queued)
            }
            None => {
                self.flush_resampler()?;
                if let Some(path) = ended {
                    self.emit(VoxEvent::TrackEnded {
                        path,
                        reason: EndReason::EndOfStream,
                    });
                }
                self.stop_playback();
                Ok(())
            }
        }
    }

    fn transition_to(&mut self, queued: QueuedSource) -> Result<()> {
        // Potentially skip creating new resampler if current/next share same sample_rate
        let same_rate = match (&self.resampler, &queued.resampler) {
            (Some(curr), Some(next)) => curr.input_rate() == next.input_rate(),
            (None, None) => true,
            _ => false,
        };

        if !same_rate {
            self.flush_resampler()?;
            self.pending.clear();
            self.resampler = queued.resampler;
        }

        let info = queued.decoder.info.clone();
        let duration = queued.decoder.playable_duration().unwrap_or(0.0);
        let path = PathBuf::from(queued.decoder.path());

        self.state.set_duration_secs(duration);
        self.begin_track(&queued.decoder);
        self.input_channels = queued.input_channels;
        self.current = Some(queued.decoder);
        self.update_gain();

        self.emit(VoxEvent::TrackStarted {
            path,
            duration: Duration::from_secs_f64(duration),
            sample_rate: info.sample_rate,
            channels: info.channels,
            reason: StartReason::Gapless,
        });

        Ok(())
    }

    fn flush_resampler(&mut self) -> Result<()> {
        if let Some(r) = self.resampler.as_mut() {
            r.flush(&mut self.pending, |s| {
                push_samples_mapped(
                    &mut self.producer,
                    &self.state,
                    s,
                    self.input_channels,
                    self.output_channels,
                    self.gain,
                )
            })?;
        }
        Ok(())
    }

    fn prefill_after_seek(&mut self) -> Result<()> {
        self.wait_for_seek_drain();

        let target = (self.output_rate as usize * self.output_channels * SEEK_PREFILL_MS) / 1000;
        let mut prefilled = 0;

        while prefilled < target {
            let Some(decoder) = self.current.as_mut() else {
                break;
            };

            let packet = decoder.next_packet()?.map(|s| s.to_vec());

            match packet {
                Some(samples) => match &mut self.resampler {
                    Some(r) => {
                        self.pending.extend_from_slice(&samples);
                        let producer = &mut self.producer;
                        let in_ch = self.input_channels;
                        let out_ch = self.output_channels;
                        let gain = self.gain;
                        let mut local = 0;
                        r.process(&mut self.pending, |s| {
                            local += push_samples_mapped_count(producer, s, in_ch, out_ch, gain);
                        })?;
                        prefilled += local;
                    }
                    None => {
                        prefilled += push_samples_mapped_count(
                            &mut self.producer,
                            &samples,
                            self.input_channels,
                            self.output_channels,
                            self.gain,
                        );
                    }
                },
                None => break,
            }
        }
        Ok(())
    }

    /// Wait for audio callback to drain stale pre-seek audio.
    fn wait_for_seek_drain(&self) {
        if self.producer.slots() == self.producer.buffer().capacity() {
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(100);
        while !self.state.seek_acked() && !self.state.is_shutdown() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn stop_playback(&mut self) {
        self.state.bump_track_epoch(); // invalidate any in-flight duration scan
        self.state.reset_samples();
        self.state.set_active(false);
        self.state.finish_seek();
        self.resampler = None;
        self.pending.clear();
        self.current = None;
        self.next = None;
        self.emit(VoxEvent::Stopped);
    }

    fn get_elapsed(&self) -> f64 {
        self.state.get_samples() as f64 / (self.output_rate as f64 * self.output_channels as f64)
    }
}

#[inline]
fn get_mapped_sample(samples: &[f32], frame: usize, out_ch: usize, in_ch: usize) -> f32 {
    if out_ch < in_ch {
        samples[frame * in_ch + out_ch]
    } else if in_ch == 1 {
        samples[frame * in_ch]
    } else {
        0.0
    }
}

/// Push samples to producer with channel mapping.
/// Bails during shutdown.
fn push_samples_mapped(
    producer: &mut Producer<f32>,
    state: &SharedState,
    samples: &[f32],
    in_ch: usize,
    out_ch: usize,
    gain: f32,
) {
    let frames = samples.len() / in_ch;
    let total_out = frames * out_ch;
    let mut written = 0;

    while written < total_out {
        // No consumer drains during shutdown or a rebuild — don't block forever.
        if state.is_shutdown() || state.is_rebuilding() {
            return;
        }
        let available = producer.slots();
        if available == 0 {
            thread::sleep(Duration::from_micros(100));
            continue;
        }
        let n = (total_out - written).min(available);
        if let Ok(chunk) = producer.write_chunk_uninit(n) {
            let start = written;
            chunk.fill_from_iter((start..start + n).map(|idx| {
                let frame = idx / out_ch;
                let out_c = idx % out_ch;
                get_mapped_sample(samples, frame, out_c, in_ch) * gain
            }));
        }
        written += n;
    }
}

/// Push samples to producer with channel mapping, returning count pushed.
fn push_samples_mapped_count(
    producer: &mut Producer<f32>,
    samples: &[f32],
    in_ch: usize,
    out_ch: usize,
    gain: f32,
) -> usize {
    let frames = samples.len() / in_ch;
    let total_out = frames * out_ch;
    let available = producer.slots();
    if available == 0 {
        return 0;
    }
    let to_write = total_out.min(available);
    if let Ok(chunk) = producer.write_chunk_uninit(to_write) {
        chunk.fill_from_iter((0..to_write).map(|idx| {
            let frame = idx / out_ch;
            let out_c = idx % out_ch;
            get_mapped_sample(samples, frame, out_c, in_ch) * gain
        }));
    }
    to_write
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::{Consumer, RingBuffer};

    /// Read everything currently in the ring into a Vec.
    fn drain(consumer: &mut Consumer<f32>) -> Vec<f32> {
        let n = consumer.slots();
        let mut out = Vec::with_capacity(n);
        if let Ok(chunk) = consumer.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out.extend_from_slice(a);
            out.extend_from_slice(b);
            chunk.commit_all();
        }
        out
    }

    // -- Channel mapping (get_mapped_sample) -----------------------

    #[test]
    fn maps_stereo_down_to_mono_left() {
        // out_ch < in_ch picks channel `out_c` of each frame — left for mono.
        let stereo = [1.0, 2.0, 3.0, 4.0]; // (L0,R0, L1,R1)
        assert_eq!(get_mapped_sample(&stereo, 0, 0, 2), 1.0);
        assert_eq!(get_mapped_sample(&stereo, 1, 0, 2), 3.0);
    }

    #[test]
    fn maps_mono_up_to_stereo_duplicates() {
        // in_ch == 1 → same sample on every output channel.
        let mono = [5.0, 6.0];
        assert_eq!(get_mapped_sample(&mono, 0, 0, 1), 5.0);
        assert_eq!(get_mapped_sample(&mono, 0, 1, 1), 5.0);
        assert_eq!(get_mapped_sample(&mono, 1, 1, 1), 6.0);
    }

    #[test]
    fn maps_missing_upper_channel_to_silence() {
        // out_ch >= in_ch with multichannel input → silence the extra channel.
        assert_eq!(get_mapped_sample(&[1.0, 2.0], 0, 2, 2), 0.0);
    }

    // -- push_samples_mapped_count (best-effort, returns count) ----

    #[test]
    fn push_count_downmixes_and_applies_gain() {
        let (mut p, mut c) = RingBuffer::<f32>::new(16);
        // stereo [L,R, L,R] → mono [L0,L1], scaled by gain 0.5.
        let n = push_samples_mapped_count(&mut p, &[1.0, 9.0, 2.0, 9.0], 2, 1, 0.5);
        assert_eq!(n, 2);
        assert_eq!(drain(&mut c), [0.5, 1.0]);
    }

    #[test]
    fn push_count_upmixes_mono_to_stereo() {
        let (mut p, mut c) = RingBuffer::<f32>::new(16);
        let n = push_samples_mapped_count(&mut p, &[3.0, 4.0], 1, 2, 1.0);
        assert_eq!(n, 4);
        assert_eq!(drain(&mut c), [3.0, 3.0, 4.0, 4.0]);
    }

    #[test]
    fn push_count_writes_only_what_fits_and_drops_rest() {
        let (mut p, mut c) = RingBuffer::<f32>::new(2);
        // 3 mono frames → 3 out samples but only 2 slots: best-effort, count == 2.
        let n = push_samples_mapped_count(&mut p, &[1.0, 2.0, 3.0], 1, 1, 1.0);
        assert_eq!(n, 2);
        assert_eq!(drain(&mut c), [1.0, 2.0]);
    }

    #[test]
    fn push_count_returns_zero_when_full() {
        let (mut p, _c) = RingBuffer::<f32>::new(1);
        assert_eq!(push_samples_mapped_count(&mut p, &[9.0], 1, 1, 1.0), 1);
        assert_eq!(push_samples_mapped_count(&mut p, &[1.0], 1, 1, 1.0), 0);
    }

    // -- push_samples_mapped (blocking write-all) ------------------

    #[test]
    fn push_blocking_writes_all_when_capacity_suffices() {
        let (mut p, mut c) = RingBuffer::<f32>::new(8);
        let state = SharedState::default();
        push_samples_mapped(&mut p, &state, &[1.0, 2.0, 3.0, 4.0], 1, 1, 2.0);
        assert_eq!(drain(&mut c), [2.0, 4.0, 6.0, 8.0]);
    }

    #[test]
    fn push_blocking_bails_on_shutdown_instead_of_hanging() {
        let (mut p, _c) = RingBuffer::<f32>::new(1);
        assert!(p.push(0.0).is_ok()); // fill the only slot
        let state = SharedState::default();
        state.set_shutdown();
        // No consumer drains; must return promptly via the shutdown check.
        push_samples_mapped(&mut p, &state, &[1.0, 2.0], 1, 1, 1.0);
    }

    // -- Play+seek coalescing decision -----------------------------

    #[test]
    fn seek_prefill_defers_only_when_seek_pending() {
        assert!(matches!(seek_prefill(&Some(3.0)), Prefill::Defer));
        assert!(matches!(seek_prefill(&None), Prefill::Now));
    }
}
