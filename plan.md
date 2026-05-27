# Voxio Engine Rework: Event Stream + Device Resilience

## Context

Voxio today exposes state by polling: `track_ended()` is a one-shot bool, `is_active()` / `is_paused()` / `position()` are atomics, and errors are swallowed to `stderr` via `eprintln!`. The output device is captured once inside `Vox::new()` and the resulting `cpal::Stream` is held for the engine's lifetime. If the user unplugs a DAC, switches default output, or the OS silently zombifies the stream (a failure mode confirmed by `examples/probe_device_switch.rs`), playback dies with no signal to the caller.

This rework promotes voxio from "polled state machine" to "event-driven engine that survives device changes."

Goals:
1. **Event stream.** The engine pushes typed events (track started, ended, error, device changed) through a channel the caller drains. Breaking change — `track_ended()` and the `eprintln!` error path go away.
2. **Auto device rebuild.** A watchdog thread detects device loss (cpal error callback) and OS-default changes (heartbeat + default-device name poll). On detection, the engine rebuilds the cpal stream against the new default, re-derives the output format, rebuilds the resampler, seeks the decoder back to the current position, prefills, and resumes. Caller sees `DeviceChanged` and a ~100 ms gap.
3. **Accurate seek + post-seek skip.** Switch `VoxDecoder::seek` from `SeekMode::Coarse` to `SeekMode::Accurate` so the decoder lands on the exact target frame rather than the nearest preceding keyframe. Combine with a small post-seek sample discard (codec pre-roll trim) to mask decoder-warm-up artifacts before the existing 30 ms fade-in. Net result: seeks land where the UI says they will, and the audible glitch is shorter and cleaner.
4. **Broaden recoverable errors.** Today `VoxDecoder::next_packet` recovers from `DecodeError` and `ResetRequired` only; other symphonia errors kill the worker thread via `eprintln!` and a fatal `Err` bubble. Widen the recoverable set to include transient `IoError` cases and isolated packet failures, with a bounded retry budget; surface each as `VoxEvent::Error { recoverable: true }` and keep the engine alive. Only truly unrecoverable conditions terminate the track, and they emit `TrackEnded { reason: Failed }` instead of crashing.
5. **Command serialization.** Today `play`/`set_next`/`seek`/`stop` go through the command channel, but `pause`/`resume`/`toggle_playback` mutate `SharedState` atomics directly from the caller thread. Mixing the two paths creates ordering races — e.g. `play → seek → pause` where the pause is silently dropped because `set_paused` short-circuits when `is_active == false` (which can happen briefly if a seek past-EOF flips the active flag, or during the gap between `play` being called and the worker setting state). Route every state-changing command through the channel and make the worker the sole writer of `paused` / `active`. Public API methods become pure command-senders.

Decisions already locked with the user:
- Channel-based event delivery, wrapped in `voxio::VoxEvents` so callers don't need `crossbeam` in their `Cargo.toml`.
- Default output device only (no pinning API; can be added later additively).
- Detection = cpal error callback + 500 ms heartbeat watchdog with 500 ms debounce on default-name change.
- Auto-rebuild + resume on every device change (Option A from discussion).
- Breaking changes are fine. Bump to `0.2.0`.

## Architecture

### Module layout

```
src/
├── lib.rs              re-exports: Vox, VoxEvent, VoxEvents, EndReason, ErrorKind, VoxError
├── error.rs            extended: add Output(String) variants for rebuild failure cases
└── engine/
    ├── mod.rs          Vox struct + public API (slimmer than today)
    ├── command.rs      decoder worker; accepts AudioBinding rebinds mid-track; serializes pause/resume/toggle through commands
    ├── decoder.rs      seek switched to SeekMode::Accurate; broadened error recovery in next_packet
    ├── resampler.rs    unchanged
    ├── state.rs        extended: callback_count, rebuilding, stream_error atomics
    ├── tap.rs          unchanged (tap_writer rebuilt with stream)
    ├── event.rs        NEW: VoxEvent enum, VoxEvents wrapper, EndReason, ErrorKind
    ├── output.rs       NEW: stream construction + output supervisor thread
    └── watchdog.rs     NEW: heartbeat + default-device-name poll thread
```

### Public API shape

```rust
// engine/event.rs
pub enum VoxEvent {
    TrackStarted { path: PathBuf, duration: Duration, sample_rate: u32, channels: usize },
    TrackEnded   { path: PathBuf, reason: EndReason },
    Error        { kind: ErrorKind, message: String },
    DeviceChanged { name: String, sample_rate: u32, channels: usize, reason: RebindReason },
    DeviceLost   { name: String },               // emitted when rebuild fails; engine retries
    StateChanged { paused: bool },
}

pub enum EndReason { Natural, Stopped, Replaced, Failed }
pub enum ErrorKind { FileOpen, Decode, Output, Seek }
pub enum RebindReason { HardLoss, DefaultChanged, StreamZombie }

pub struct VoxEvents { rx: crossbeam::channel::Receiver<VoxEvent> }
impl VoxEvents {
    pub fn try_recv(&self) -> Option<VoxEvent>;
    pub fn recv(&self) -> Option<VoxEvent>;
    pub fn recv_timeout(&self, d: Duration) -> Option<VoxEvent>;
    pub fn iter(&self) -> impl Iterator<Item = VoxEvent> + '_;
    pub fn len(&self) -> usize;
}
```

```rust
// engine/mod.rs — caller-facing
impl Vox {
    pub fn new() -> Result<(Self, VoxEvents)>;
    pub fn play<P: AsRef<Path>>(&mut self, p: P) -> Result<()>;
    pub fn set_next<P: AsRef<Path>>(&mut self, p: P) -> Result<()>;
    pub fn seek_to(&mut self, pos: f64) -> Result<()>;
    pub fn seek_relative(&mut self, delta: f64) -> Result<()>;
    pub fn toggle_playback(&self) -> Result<()>;
    pub fn pause(&self) -> Result<()>;
    pub fn resume(&self) -> Result<()>;
    pub fn stop(&self) -> Result<()>;
    pub fn position(&self) -> Duration;
    pub fn duration(&self) -> Duration;
    pub fn sample_rate(&self) -> u32;     // current output rate; updates on rebuild
    pub fn channels(&self) -> usize;
    pub fn is_paused(&self) -> bool;
    pub fn is_active(&self) -> bool;
    pub fn get_latest_samples(&mut self, amount: usize) -> Vec<f32>;
}
// `track_ended()` removed — use VoxEvent::TrackEnded.
```

Channel sizing: bounded `crossbeam::channel::bounded(256)`. Decoder-thread sends use blocking `send` (decoder can wait). Audio-callback sends use `try_send` and drop-on-full to preserve RT-safety — only a `StateChanged` or stream-error signal originates there, both safe to drop.

### Threads and ownership

| Thread | Owns | Communicates via |
|---|---|---|
| Caller | `Vox`, `VoxEvents`, current `TapReader` | `commands: Sender<VoxCommand>`, `output_ctrl: Sender<OutputControl>`, drains `tap_swap: Receiver<TapReader>` |
| Decoder worker (existing) | current/next `VoxDecoder`, `VoxResampler`, `rtrb::Producer` | `commands` receiver, NEW `audio_binding: Receiver<AudioBinding>`, `events: Sender<VoxEvent>` |
| Output supervisor (NEW) | active `cpal::Stream` (owns `TapWriter` + `rtrb::Consumer` inside its callback closure) | `output_ctrl` receiver, `events` sender, sends `AudioBinding` to decoder, sends `TapReader` to Vox via `tap_swap` |
| Watchdog (NEW) | none | reads `SharedState`, sends `OutputControl::Rebuild` |

`AudioBinding` is the handoff packet from supervisor → decoder when the stream rebuilds:

```rust
struct AudioBinding {
    producer: rtrb::Producer<f32>,
    output_rate: u32,
    output_channels: usize,
}
```

### Rebuild flow (the load-bearing path)

1. Watchdog or cpal-error-flag detects change → `output_ctrl.send(Rebuild { reason })`.
2. Supervisor sets `state.rebuilding = true`. Existing audio callback (still alive briefly) sees the flag, outputs silence, drains the ring buffer.
3. Supervisor drops the old `Stream` (cpal joins the audio thread).
4. Supervisor queries `host.default_output_device()`, builds new `StreamConfig`, allocates fresh `(Producer, Consumer)` ring buffer and `(TapWriter, TapReader)` pair. The new `TapReader` is shipped to `Vox` via a dedicated unbounded `crossbeam::channel::Sender<TapReader>` — `Vox::get_latest_samples` drains that channel with `try_recv` before each read and swaps its locally-owned `TapReader` if a new one arrived. No locks: the `TapReader` has a single owner (`Vox`) at any time, the channel handoff is lock-free, and the rtrb consumer underneath uses atomic indices.
5. Supervisor builds new `Stream` with new consumer + new tap_writer. Calls `stream.play()`.
6. Supervisor sends `AudioBinding { producer, output_rate, output_channels }` on the binding channel.
7. Decoder worker, between decode iterations (checked in `decode_handler`), drains the binding channel. If a new binding arrived:
   - Captures `current_pos = state.get_samples() as f64 / old_sps`.
   - Replaces its `producer`, `output_rate`, `output_channels`.
   - Rebuilds `VoxResampler` using new `output_rate`.
   - Clears `pending` buffer.
   - If `current` track exists, calls `decoder.seek(current_pos)` to land back where we were.
   - Calls `prefill_after_seek(...)` to refill the new ring buffer.
8. Supervisor clears `state.rebuilding = false`, emits `VoxEvent::DeviceChanged { name, sample_rate, channels, reason }`.
9. If step 4 or 5 fails: emit `VoxEvent::DeviceLost { name }`, leave `rebuilding = true`, return to step 1 on the next watchdog tick (watchdog retries every 500 ms while in lost state).

### Watchdog (`engine/watchdog.rs`)

500 ms tick loop. Each tick:
- **Heartbeat check.** Read `state.callback_count()` (incremented by audio callback each invocation). If unchanged for 4 ticks (2 s) while `is_active && !is_paused && !rebuilding`, trigger `Rebuild { StreamZombie }`.
- **Error flag check.** `state.take_stream_error()` (swap-acquire). If set, trigger `Rebuild { HardLoss }`. Set by the cpal error callback closure.
- **Default-device drift.** Query `host.default_output_device().description().name()`. Compare against last-known. If different, mark "pending change" with timestamp. If pending change has been stable for ≥500 ms (debounce — Windows flips defaults rapidly during plug-in), trigger `Rebuild { DefaultChanged }` and adopt the new name as last-known.

The watchdog is the *only* place that decides to rebuild. The cpal error callback just sets a flag; the supervisor reacts only to messages on `output_ctrl`. This keeps the rebuild trigger logic in one place and testable.

### State additions (`engine/state.rs`)

```rust
callback_count:  AtomicU64,    // ++ in audio callback
rebuilding:      AtomicBool,   // gates decoder + callback during rebuild
stream_error:    AtomicBool,   // set by cpal error closure, swap-cleared by watchdog
output_rate:     AtomicU32,    // live output sample rate; updated by supervisor on rebuild
output_channels: AtomicUsize,  // live output channel count; updated by supervisor on rebuild
```

Methods: `bump_callback()`, `callback_count()`, `set_rebuilding(bool)`, `is_rebuilding()`, `set_stream_error()`, `take_stream_error()`, `output_rate()`, `output_channels()`, `set_output_format(rate, channels)`.

`Vox::sample_rate()` and `Vox::channels()` now read these atomics — the values stay correct after a device rebuild without any shared-mutable container.

### Lock-free invariant

No `Mutex`, `RwLock`, or `parking_lot` anywhere in the engine. Shared state is one of:
- **Atomics on `Arc<SharedState>`** — for scalars: counters, flags, output format, sample position, duration.
- **Channels (`crossbeam::channel`)** — for ownership handoff: commands to the worker, `AudioBinding` to the worker, `TapReader` to `Vox`, `OutputControl` to the supervisor, `VoxEvent` to the caller. Each channel is single-producer or single-consumer in practice; recipients own the value once received.
- **Single-owner ring buffers (`rtrb`)** — for sample data and tap data. Atomic-index based, lock-free SPSC, with the producer/consumer halves each owned by exactly one thread.

This keeps the audio callback RT-safe (no `lock()` call paths) and removes priority-inversion risk entirely.

### Event emission points

Where events get sent:

| Event | Emitted from | Trigger |
|---|---|---|
| `TrackStarted` | decoder worker | end of `handle_play()` after successful `VoxDecoder::open` |
| `TrackEnded { Natural }` | decoder worker | `handle_track_end()` when no `next` queued |
| `TrackEnded { Replaced }` | decoder worker | `transition_to()` (gapless) and `handle_play()` interrupting a running track |
| `TrackEnded { Stopped }` | decoder worker | `stop_playback()` when called with an active track |
| `TrackEnded { Failed }` | decoder worker | when `next_packet()` returns a hard `Err` mid-track |
| `Error` | decoder worker | every place that currently `eprintln!`s |
| `DeviceChanged` | output supervisor | end of successful rebuild |
| `DeviceLost` | output supervisor | rebuild failure |
| `StateChanged` | decoder worker | after `set_paused` / `set_active` toggles |

Event channel is `crossbeam::channel::bounded(256)`. Decoder sends use `send` (blocks if caller stops draining — fine, decoder is not RT). Supervisor uses `send`. Audio callback never emits directly; it sets atomic flags only.

### Accurate seek + post-seek skip (`engine/decoder.rs`, `engine/command.rs`)

In `VoxDecoder::seek`:
- Change `SeekMode::Coarse` (`decoder.rs:255`) to `SeekMode::Accurate`. Symphonia then seeks to the nearest preceding keyframe and decodes-and-discards forward to the exact target frame internally. `samples_decoded` is set to `seeked.actual_ts`, which now equals the target rather than a keyframe boundary.
- Cost: extra decode work for the codec between keyframe and target (a few packets for MP3, a few hundred ms for some AAC variants). Hidden under the existing seek-silence + 30 ms fade-in.

In `VoxWorker::handle_seek` (after the existing `decoder.seek` call):
- Add a "post-seek skip" pass: decode and discard up to `SEEK_POST_SKIP_MS` (new constant in `lib.rs`, default 10 ms) of input-rate samples before prefill begins. This trims codec ramp-up (MP3 bit-reservoir, AAC overlap-add) so the first audible sample after the fade-in is from a settled decoder state.
- Implementation: extend `VoxDecoder` with a `skip_samples(n: u64) -> Result<()>` helper that pulls packets via the existing decode path and increments `samples_decoded` without writing to any output. `handle_seek` calls it between `decoder.seek(...)` and `prefill_after_seek(...)`.
- The skip is *post-seek frame-discard*, distinct from symphonia's internal accurate-seek discard — it cleans the decoder, not the seek landing.

The existing 30 ms `SEEK_FADE_MS` fade-in stays. Combined with the skip, post-seek audio is: 10 ms of decoded-and-discarded warm-up → 30 ms fade-in from silence → full-volume playback at the exact requested frame.

### Broadened error recovery (`engine/decoder.rs`, `engine/command.rs`)

In `VoxDecoder::next_packet`, the recoverable set today is `DecodeError` and `ResetRequired` (`decoder.rs:181-183, 198-200`). Widen to:

| Symphonia error | Today | New behavior |
|---|---|---|
| `IoError(UnexpectedEof)` | `Ok(None)` (clean end) | unchanged |
| `IoError(other)` | fatal `Err` | recoverable up to 3 retries within one track; on exhaustion, return `Err(VoxError::Decoder)` |
| `DecodeError(_)` | continue silently | continue but bump a per-track corruption counter; if >32 in a single track, emit `Error { recoverable: false }` and return `Err` |
| `ResetRequired` | reset + continue | unchanged |
| `LimitError(_)` | fatal `Err` | fatal `Err` (truly unrecoverable) |
| `Unsupported(_)` | fatal `Err` | fatal `Err` |
| `SeekError(_)` | n/a in this path | only from `format.seek`; surface as recoverable seek error |

In `VoxWorker::decode_handler`, when `next_packet` returns `Err`:
- Emit `VoxEvent::Error { kind: ErrorKind::Decode, message, recoverable: false }`.
- Emit `VoxEvent::TrackEnded { path, reason: EndReason::Failed }`.
- Call `stop_playback()` to clean current/next/resampler.
- **Do not** propagate the `Err` out of `run()` — the worker thread stays alive and ready for the next `Play` command.

Same treatment in `handle_play`: file-open failure emits `Error { kind: FileOpen, recoverable: true }` (caller can retry with a different path) and leaves the worker idle.

This eliminates every `eprintln!` and every `Err` bubble that today silently kills the decoder thread.

### Command serialization (`engine/mod.rs`, `engine/command.rs`, `engine/state.rs`)

Today's split:
- Channel commands: `Play`, `QueueNext`, `Seek`, `Stop`, `Shutdown`.
- Direct atomic writes from caller thread: `pause()`, `resume()`, `toggle_playback()`, and `set_active`/`set_paused`/`reset_samples`/`start_seek` inside `play()` / `seek_to()` / `seek_relative()`.

The split causes the user-reported `play → seek → pause` race because the atomic writes interleave with the worker's command processing in an order the caller didn't intend.

Fix:
1. **All state mutation moves to the worker.** Public API methods become pure command-senders. `Vox::play()` / `seek_to()` / `pause()` / `resume()` / `toggle_playback()` no longer touch `SharedState` directly. The worker is the only writer of `active`, `paused`, `samples_played`, `seek_pending`, `seek_generation`, `duration_micros`.
2. **Extend `VoxCommand`** with `Pause`, `Resume`, `Toggle`. The worker handles each by writing the `paused` atomic (which the audio callback reads as before).
3. **Remove the `is_active` short-circuit in `SharedState::set_paused`** (`state.rs:78-80`). The worker decides whether a pause makes sense for the current state; if `pause` arrives while idle, the worker simply records the intent so the next track honors it (or it's a no-op — pick at implementation time; the current bug is the silent drop).
4. **Atomic flip kept inside the worker for seek/play bootstrap.** `handle_play` performs the `start_seek` → `reset_samples` → `set_active(true)` sequence at the start of its work, *not* the caller. This way the seek/active/sample bookkeeping is serialized with respect to any pause/resume that arrived earlier in the queue.
5. **Bounded channel capacity stays at `CHANNEL_COUNT = 16`** (already in `lib.rs:8`). `Pause` / `Resume` / `Toggle` use blocking `send`. If the channel is genuinely full of older state-changing commands, that's the worker being momentarily behind — blocking is correct; we want order preserved.

Latency cost of routing pause through the channel: a single channel send + the worker's poll loop (which polls every decode iteration, sub-millisecond at typical sample rates). Imperceptible. The audio callback still reads `paused` as a Relaxed atomic for instant per-callback silencing as soon as the worker writes it.

### Backwards-compat shims

None. Per user decision, breaking. Update `examples/probe_device_switch.rs` to use the new API as a smoke test (or replace it — the rework subsumes its purpose).

## Critical files to modify

- `src/lib.rs` — re-exports for `VoxEvent`, `VoxEvents`, `EndReason`, `ErrorKind`, `RebindReason`.
- `src/error.rs` — keep variants, add nothing yet; channel-closed paths covered.
- `src/engine/mod.rs` — rewrite `Vox::new` to return `(Self, VoxEvents)`; spawn supervisor + watchdog threads; drop direct `Stream` ownership; remove `track_ended()`; add `Drop` impl that sends `OutputControl::Shutdown`.
- `src/engine/command.rs` — extend `VoxWorker` to (a) accept an `events: Sender<VoxEvent>`, (b) drain `audio_binding: Receiver<AudioBinding>` between decode iterations and re-seek on rebind, (c) emit the events listed in the table, (d) handle new `Pause` / `Resume` / `Toggle` commands and write `paused`/`active` exclusively from the worker, (e) call `decoder.skip_samples(SEEK_POST_SKIP_MS-worth)` between `handle_seek`'s seek and prefill, (f) wrap `decode_handler` errors so the worker stays alive and emits `Error` + `TrackEnded { Failed }` instead of bubbling.
- `src/engine/decoder.rs` — switch `SeekMode::Coarse` to `SeekMode::Accurate`; add `skip_samples(n: u64) -> Result<()>`; widen the recoverable-error set in `next_packet` per the table above; introduce a per-track corruption counter and bounded retry budget.
- `src/engine/state.rs` — add `callback_count`, `rebuilding`, `stream_error` and their accessors; remove the `is_active` short-circuit in `set_paused` (mutation discipline moves to the worker).
- `src/engine/event.rs` — NEW; types listed above.
- `src/engine/output.rs` — NEW; extracts the current `build_output_stream` block from `mod.rs` into `build_stream(host, &events_tx, &state) -> Result<(Stream, Producer, TapReader, StreamInfo)>`; implements `output_supervisor_loop`.
- `src/engine/watchdog.rs` — NEW; `watchdog_loop`.

## Reuse / existing utilities

- `rtrb::RingBuffer::new` already used in `mod.rs:58`; reuse the same pattern in `output.rs` on each rebuild.
- `tap::new_tap` (`engine/tap.rs`) — `TapWriter`/`TapReader` pair, rebuild on each stream alongside the ring buffer. `Vox` keeps `tap: TapReader` as a plain owned field; the supervisor ships replacements via the `tap_swap` channel, and `get_latest_samples` drains it before reading. No new dependencies, no shared-mutable wrapper.
- `VoxDecoder::seek` (`decoder.rs:247`) — already returns landed position; mode flipped to `Accurate`. Reused by both user-initiated seeks and the device-rebuild path to resume playback at the captured time.
- `VoxResampler::new` (`resampler.rs`) — called with the new `output_rate` after rebuild, same as for a normal new-track case.
- `VoxWorker::prefill_after_seek` (`command.rs:411`) — already exists; the rebuild path calls it after re-seeking to fill the new ring buffer before resuming.
- `SharedState::start_seek` / `finish_seek` (`state.rs:128/134`) — reuse around the rebuild seek-back; the audio callback's existing seek-gating logic already handles "output silence while a seek is in flight," which is exactly what we want during a rebuild.

## Verification

End-to-end checks:

1. **Cargo builds clean.** `cargo build` and `cargo build --examples` succeed with the new API.

2. **Event channel basics.** Write a small example (`examples/event_log.rs`) that opens `Vox::new()?`, plays a known-good MP3, then drains `events` in a loop printing each. Expected sequence: `TrackStarted` → (no spurious events while playing) → `TrackEnded { Natural }` at end of file. Run with a 10-second clip and a file that doesn't exist (expect `Error { FileOpen }`).

3. **Gapless still works.** Play track A, immediately `set_next(B)`. Expect `TrackStarted{A}` → `TrackEnded{A, Replaced}` → `TrackStarted{B}` with no audible gap. Verify via existing manual listening test plus checking event ordering.

4. **Device rebuild — soft (default change).** With `examples/probe_device_switch.rs` reworked as `examples/device_switch.rs`: play a long track, then in Windows Sound settings switch the default output device. Expected: ~100 ms audio glitch, `DeviceChanged { reason: DefaultChanged, name, .. }` event arrives, playback continues on new device, `vox.sample_rate()` reflects new rate.

5. **Device rebuild — hard (unplug).** Same example, unplug a USB DAC while playing. Expected: `DeviceChanged { reason: HardLoss, .. }` within ~2 s, playback resumes on new default.

6. **Device rebuild — zombie.** Hard to reproduce on demand, but the watchdog's stale-callback-count path is testable in isolation: a unit test in `watchdog.rs` that drives a mock `SharedState` and asserts `OutputControl::Rebuild { StreamZombie }` after 2 s of no callback bumps.

7. **Shutdown is clean.** Drop `Vox` mid-playback. All three threads (decoder, supervisor, watchdog) join in `Drop`. Run under `cargo run --example event_log` and check for hangs on Ctrl-C.

8. **Accurate seek lands where requested.** Seek to a known timestamp in a long track, immediately read `vox.position()`. Expect the returned `Duration` to equal the requested seek time within one output frame (sub-millisecond). Today, with `SeekMode::Coarse`, the landing can be hundreds of milliseconds before the request.

9. **Seek artifact cleanup.** Listen for codec ramp-up artifacts (clicks, brief noise) at the moment of seek on MP3 and AAC files. After the post-seek skip + existing fade, expect a clean fade-in with no audible warm-up tick.

10. **Broadened error recovery.** Test with (a) a deliberately corrupted MP3 (truncated mid-frame), (b) a file with sporadic decode errors. Expect `VoxEvent::Error { recoverable: true }` per recoverable hiccup, playback continuing through them. With a hard-corrupt file, expect `Error { recoverable: false }` + `TrackEnded { Failed }`, and verify the worker thread is still alive by issuing a subsequent `vox.play(good_file)` and confirming `TrackStarted`.

11. **Command serialization.** Run a tight `play(A); seek_to(30.0); pause();` sequence. Expect `paused == true` to stick 100% of the time across 1000 iterations. Repeat with `play(A); seek_to(very_late_time); pause();` (where the seek lands past EOF) — pause must still be respected for the next track. Also run a randomized fuzz: shuffle play/seek/pause/resume/toggle calls and assert that the final observable state matches the last command sent.

12. **No regressions.** Existing manual tests the user uses (whatever player UI is calling voxio today) — seek, pause, resume, stop, set_next — all continue to work with the new event-emitting code paths.

Build commands during development: `cargo build`, `cargo build --examples`, `cargo run --example device_switch`, `cargo run --example event_log`.

