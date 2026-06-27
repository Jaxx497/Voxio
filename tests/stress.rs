use std::{
    thread,
    time::{Duration, Instant},
};
use voxio::{
    EndReason, ReplayGainMode, StartReason, Vox, VoxConfig, VoxError, VoxEvent, VoxEvents,
};

const LEGAL_FILE: &str = "tests/test_suite/Preludes, Op. 28 - No. 16 'Hades'.mp3";
const PART1: &str = "tests/test_suite/part1.mp3";
const PART2: &str = "tests/test_suite/part2.mp3";
const TEXT_FILE: &str = "tests/test_suite/invalid_filetype.txt";

fn fresh() -> (Vox, VoxEvents) {
    Vox::new().expect("engine init")
}

fn fresh_cfg(cfg: VoxConfig) -> (Vox, VoxEvents) {
    Vox::new_with_config(cfg).expect("engine init with config")
}

fn await_event(events: &VoxEvents) -> Option<VoxEvent> {
    events.recv_timeout(Duration::from_millis(500))
}

fn await_event_ignoring_device(events: &VoxEvents) -> Option<VoxEvent> {
    loop {
        match await_event(events) {
            Some(VoxEvent::DeviceChanged { .. }) => continue,
            other => return other,
        }
    }
}

/// Wait specifically for a TrackStarted event, skipping intermediate events
/// like TrackEnded, Stopped, Error, and DeviceChanged that may arrive first.
fn await_track_started(events: &VoxEvents) -> Option<VoxEvent> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if Instant::now() > deadline {
            return None;
        }
        match await_event(events) {
            Some(VoxEvent::DeviceChanged { .. }) => continue,
            Some(VoxEvent::TrackEnded { .. }) => continue,
            Some(VoxEvent::Stopped) => continue,
            Some(VoxEvent::StateChanged { .. }) => continue,
            Some(VoxEvent::Error { .. }) => continue,
            Some(VoxEvent::NextReady { .. }) => continue,
            other @ Some(VoxEvent::TrackStarted { .. }) => return other,
            other => return other,
        }
    }
}

fn drain_events(events: &VoxEvents) -> Vec<VoxEvent> {
    let mut out = Vec::new();
    while let Some(e) = events.try_recv() {
        out.push(e);
    }
    out
}

#[allow(dead_code)]
fn drain_until_stopped(events: &VoxEvents, timeout: Duration) -> Vec<VoxEvent> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    loop {
        match events.recv_timeout(Duration::from_millis(50)) {
            Some(VoxEvent::DeviceChanged { .. }) => {}
            Some(VoxEvent::Stopped) => {
                out.push(VoxEvent::Stopped);
                break;
            }
            Some(e) => out.push(e),
            None => break,
        }
        if Instant::now() > deadline {
            break;
        }
    }
    out
}

#[allow(dead_code)]
fn wait_active(v: &Vox, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if v.is_active() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

fn wait_inactive(v: &Vox, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !v.is_active() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

// ============================================================================
// 1. RAPID PLAY/STOP CYCLES
// Simulates a user mashing play/stop buttons quickly.
// ============================================================================

#[test]
fn rapid_play_stop_cycles() {
    let (v, events) = fresh();
    for _ in 0..20 {
        v.play(LEGAL_FILE).unwrap();
        thread::sleep(Duration::from_millis(30));
        v.stop();
        thread::sleep(Duration::from_millis(20));
    }
    let _ = drain_events(&events);
    assert!(
        wait_inactive(&v, Duration::from_secs(3)),
        "engine should be idle after rapid play/stop"
    );
}

#[test]
fn rapid_play_stop_with_event_drain() {
    let (v, events) = fresh();
    let mut started = 0u32;
    let mut ended = 0u32;

    for _ in 0..15 {
        v.play(LEGAL_FILE).unwrap();
        thread::sleep(Duration::from_millis(20));
        v.stop();
        thread::sleep(Duration::from_millis(20));
    }

    // Drain all events
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match events.try_recv() {
            Some(VoxEvent::TrackStarted { .. }) => started += 1,
            Some(VoxEvent::TrackEnded { .. }) => ended += 1,
            Some(VoxEvent::Stopped) => {}
            Some(_) => {}
            None => break,
        }
    }
    // At minimum the first play should have started
    assert!(started >= 1, "expected at least 1 TrackStarted, got {started}");
    assert!(ended >= 1, "expected at least 1 TrackEnded, got {ended}");
}

// ============================================================================
// 2. RAPID SEEK STORMS
// Simulates a user dragging a seek bar frantically.
// ============================================================================

#[test]
fn rapid_absolute_seeks() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let dur = v.duration().as_secs_f64();
    for i in 0..30 {
        let target = (i as f64 / 30.0) * dur;
        v.seek_to(target);
        thread::sleep(Duration::from_millis(15));
    }

    // Engine should still be alive and responsive
    thread::sleep(Duration::from_millis(200));
    assert!(v.is_active(), "engine should survive seek storm");
    let _ = drain_events(&events);
}

#[test]
fn rapid_relative_seeks() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // Seek forward and backward alternately
    for i in 0..20 {
        if i % 2 == 0 {
            v.seek_relative(1.0);
        } else {
            v.seek_relative(-1.0);
        }
        thread::sleep(Duration::from_millis(15));
    }

    thread::sleep(Duration::from_millis(200));
    assert!(v.is_active(), "engine should survive relative seek storm");
    let _ = drain_events(&events);
}

#[test]
fn seek_to_very_large_value() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // Seek way past the end — should trigger EndOfStream
    v.seek_to(f64::MAX / 2.0);
    let event = await_event_ignoring_device(&events);
    assert!(
        matches!(
            event,
            Some(VoxEvent::TrackEnded {
                reason: EndReason::EndOfStream,
                ..
            })
        ),
        "expected EndOfStream for f64::MAX seek, got {event:?}"
    );
}

#[test]
fn seek_to_negative_large_value() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    v.seek_to(-1_000_000.0);
    thread::sleep(Duration::from_millis(100));
    let pos = v.position().as_secs_f64();
    assert!(
        pos < 0.5,
        "negative seek should clamp to ~0, got {pos}"
    );
}

#[test]
fn seek_while_paused_repeatedly() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    v.pause();
    await_event_ignoring_device(&events);

    for i in 0..15 {
        v.seek_to(i as f64);
        thread::sleep(Duration::from_millis(20));
    }

    // Should still be paused
    assert!(v.is_paused(), "should remain paused after seeks");

    v.resume();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::StateChanged { paused: false })));
    let _ = drain_events(&events);
}

// ============================================================================
// 3. GAPLESS PLAYLIST CHAINS
// Simulates building and playing a full playlist gaplessly.
// ============================================================================

#[test]
fn gapless_chain_two_tracks() {
    let (v, events) = fresh();
    v.play(PART1).unwrap();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::TrackStarted { reason: StartReason::Play, .. })));

    v.set_next(PART2).unwrap();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::NextReady { .. })));

    // Seek past end of part1 to trigger gapless transition immediately
    let dur = v.duration().as_secs_f64();
    v.seek_to(dur + 1.0);

    let e = await_event_ignoring_device(&events);
    assert!(
        matches!(e, Some(VoxEvent::TrackEnded { reason: EndReason::EndOfStream, .. })),
        "expected EndOfStream, got {e:?}"
    );

    let e = await_event_ignoring_device(&events);
    assert!(
        matches!(e, Some(VoxEvent::TrackStarted { reason: StartReason::Gapless, .. })),
        "expected gapless TrackStarted, got {e:?}"
    );

    let _ = drain_events(&events);
}

#[test]
fn gapless_set_next_overwrites_previous() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // Set next to part1, then overwrite with part2
    v.set_next(PART1).unwrap();
    await_event_ignoring_device(&events);
    v.set_next(PART2).unwrap();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::NextReady { .. })));

    let _ = drain_events(&events);
}

#[test]
fn gapless_clear_and_re_set() {
    let (v, events) = fresh();
    v.play(PART1).unwrap();
    await_event_ignoring_device(&events);

    v.set_next(PART2).unwrap();
    await_event_ignoring_device(&events);

    v.clear_next();

    // Set a new next
    v.set_next(LEGAL_FILE).unwrap();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::NextReady { .. })));

    let _ = drain_events(&events);
}

#[test]
fn set_next_same_file_as_current() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    v.set_next(LEGAL_FILE).unwrap();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::NextReady { .. })));

    // Seek past end to trigger the transition
    let dur = v.duration().as_secs_f64();
    v.seek_to(dur + 1.0);

    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::TrackEnded { .. })));

    let e = await_event_ignoring_device(&events);
    assert!(
        matches!(e, Some(VoxEvent::TrackStarted { reason: StartReason::Gapless, .. })),
        "gapless to same file should work, got {e:?}"
    );

    let _ = drain_events(&events);
}

// ============================================================================
// 4. RAPID COMMAND COALESCING
// Tests the command worker's ability to coalesce rapid fire commands.
// ============================================================================

#[test]
fn rapid_play_then_seek_coalescing() {
    let (v, events) = fresh();

    // Fire play + seek back-to-back — the worker should coalesce them
    v.play(LEGAL_FILE).unwrap();
    v.seek_to(10.0);
    thread::sleep(Duration::from_millis(50));

    // The seek should have been applied (play+seek coalescing)
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let pos = v.position().as_secs_f64();
        if (pos - 10.0).abs() < 1.0 || Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let pos = v.position().as_secs_f64();
    assert!(
        (8.0..12.0).contains(&pos),
        "play+seek coalescing: expected ~10s, got {pos}"
    );
    let _ = drain_events(&events);
}

#[test]
fn rapid_play_seek_stop_sequence() {
    let (v, events) = fresh();

    v.play(LEGAL_FILE).unwrap();
    v.seek_to(5.0);
    v.stop();
    thread::sleep(Duration::from_millis(100));

    // Stop should take precedence
    assert!(
        wait_inactive(&v, Duration::from_secs(2)),
        "stop should win over play+seek"
    );
    let _ = drain_events(&events);
}

#[test]
fn double_play_rapid() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    thread::sleep(Duration::from_millis(10));
    v.play(LEGAL_FILE).unwrap();

    // Second play should interrupt the first
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if v.is_active() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(v.is_active(), "should be playing after double play");
    let _ = drain_events(&events);
}

#[test]
fn many_seeks_rapid_coalescing() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // Fire 50 seeks in rapid succession
    for i in 0..50 {
        v.seek_to(i as f64 * 0.1);
    }
    thread::sleep(Duration::from_millis(300));

    assert!(v.is_active(), "should survive 50 rapid seeks");
    let _ = drain_events(&events);
}

// ============================================================================
// 5. PAUSE/RESUME STRESS
// Simulates rapid pause/resume toggling.
// ============================================================================

#[test]
fn rapid_pause_resume_toggle() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    for _ in 0..20 {
        v.pause();
        v.resume();
    }
    thread::sleep(Duration::from_millis(100));

    assert!(v.is_active(), "engine should survive rapid pause/resume");
    // Final state should be resumed (even number of toggles)
    assert!(!v.is_paused(), "should end in resumed state");
    let _ = drain_events(&events);
}

#[test]
fn pause_resume_with_seek_between() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    for i in 0..10 {
        v.pause();
        v.seek_to(i as f64);
        v.resume();
        thread::sleep(Duration::from_millis(30));
    }

    assert!(v.is_active(), "should survive pause+seek+resume cycles");
    let _ = drain_events(&events);
}

#[test]
fn double_pause_is_idempotent() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    v.pause();
    let e1 = await_event_ignoring_device(&events);
    assert!(matches!(e1, Some(VoxEvent::StateChanged { paused: true })));

    // Second pause should NOT emit another StateChanged
    v.pause();
    let e2 = events.try_recv();
    assert!(
        e2.is_none(),
        "double pause should not emit duplicate StateChanged, got {e2:?}"
    );

    v.resume();
    let _ = drain_events(&events);
}

#[test]
fn double_resume_is_idempotent() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // Already playing (not paused), resume should be a no-op
    v.resume();
    let e = events.try_recv();
    assert!(
        e.is_none(),
        "resume when not paused should not emit StateChanged, got {e:?}"
    );
    let _ = drain_events(&events);
}

// ============================================================================
// 6. REPLAYGAIN MODE CYCLING
// Switches ReplayGain modes rapidly during playback.
// ============================================================================

#[test]
fn replaygain_cycling_during_playback() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    for _ in 0..10 {
        v.set_replaygain(ReplayGainMode::Track);
        v.set_replaygain(ReplayGainMode::Album);
        v.set_replaygain(ReplayGainMode::Off);
    }
    thread::sleep(Duration::from_millis(100));

    assert!(v.is_active(), "should survive RG mode cycling");
    let _ = drain_events(&events);
}

#[test]
fn replaygain_change_during_seek() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    v.set_replaygain(ReplayGainMode::Track);
    v.seek_to(3.0);
    v.set_replaygain(ReplayGainMode::Album);
    v.seek_to(6.0);
    v.set_replaygain(ReplayGainMode::Off);

    thread::sleep(Duration::from_millis(200));
    assert!(v.is_active(), "should survive RG+seek combo");
    let _ = drain_events(&events);
}

// ============================================================================
// 7. MULTIPLE ENGINE INSTANCES
// Creates many Vox instances to test resource cleanup and isolation.
// ============================================================================

#[test]
fn multiple_engines_simultaneously() {
    let mut engines = Vec::new();
    for _ in 0..5 {
        let (v, events) = fresh();
        engines.push((v, events));
    }

    // All should be valid
    for (v, _) in &engines {
        assert!(!v.is_active(), "fresh engine should not be active");
    }

    // Play on first, stop on second, etc.
    if let Some((v, e)) = engines.first() {
        v.play(LEGAL_FILE).unwrap();
        let _ = await_event_ignoring_device(e);
        assert!(v.is_active());
    }

    // Dropping all while one is playing
    drop(engines);
}

#[test]
fn engine_creation_and_drop_tight_loop() {
    for _ in 0..10 {
        let (v, events) = fresh();
        let _ = events;
        drop(v);
    }
}

#[test]
fn engine_create_play_drop_immediately() {
    for _ in 0..5 {
        let (v, events) = fresh();
        let _ = v.play(LEGAL_FILE);
        // Drop immediately without stopping
        drop(v);
        drop(events);
    }
}

// ============================================================================
// 8. DROP WHILE PLAYING
// Ensures the engine cleans up properly when dropped mid-playback.
// ============================================================================

#[test]
fn drop_vox_while_playing_no_panic() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(v.is_active());
    // Drop without stopping — should not panic or leak
    drop(v);
    drop(events);
}

#[test]
fn drop_vox_while_paused_no_panic() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);
    v.pause();
    thread::sleep(Duration::from_millis(50));
    drop(v);
    drop(events);
}

#[test]
fn drop_events_first_then_vox() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    thread::sleep(Duration::from_millis(50));
    // Drop events first, then vox — should not deadlock
    drop(events);
    drop(v);
}

// ============================================================================
// 9. THREAD SAFETY
// Vox is Send but not Sync — commands must come from one thread while we
// verify that the engine remains responsive and doesn't deadlock.
// ============================================================================

#[test]
fn rapid_seek_from_sender_thread() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // Drive seeks from a separate thread (Vox is Send)
    let handle = thread::spawn(move || {
        for i in 0..50 {
            v.seek_to(i as f64 * 0.3);
            thread::sleep(Duration::from_millis(5));
        }
        // Return ownership back so we can check state
        v
    });

    let v = handle.join().unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(v.is_active(), "should survive rapid seeks from sender thread");
    let _ = drain_events(&events);
}

#[test]
fn rapid_pause_resume_from_sender_thread() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let handle = thread::spawn(move || {
        for i in 0..30 {
            if i % 2 == 0 {
                v.pause();
            } else {
                v.resume();
            }
            thread::sleep(Duration::from_millis(5));
        }
        v
    });

    let v = handle.join().unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(v.is_active(), "should survive rapid pause/resume from sender thread");
    let _ = drain_events(&events);
}

#[test]
fn send_commands_then_query_state() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // Drive playback from a separate thread, then return
    let handle = thread::spawn(move || {
        for i in 0..10 {
            v.seek_to(i as f64);
            v.pause();
            v.resume();
            thread::sleep(Duration::from_millis(30));
        }
        v
    });

    let v = handle.join().unwrap();

    // Query state on the main thread — all reads are atomic, should be safe
    for _ in 0..50 {
        let _ = v.position();
        let _ = v.position_samples();
        let _ = v.duration();
        let _ = v.duration_samples();
        let _ = v.is_active();
        let _ = v.is_paused();
        let _ = v.sample_rate();
        let _ = v.channels();
        thread::sleep(Duration::from_millis(5));
    }
    assert!(v.is_active());
    let _ = drain_events(&events);
}

// ============================================================================
// 10. TAPHANDLE STRESS
// Exercises the visualization tap under load.
// ============================================================================

#[test]
fn tap_handle_polling_during_playback() {
    let (mut v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let mut tap = v.take_tap().expect("tap should be available");

    // Poll tap rapidly while playback runs
    for _ in 0..100 {
        let samples = tap.latest(256);
        // May be empty if not enough data yet, that's fine
        let _ = samples;
        thread::sleep(Duration::from_millis(10));
    }

    assert!(v.is_active());
    let _ = drain_events(&events);
}

#[test]
fn tap_handle_returns_none_on_second_take() {
    let (mut v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let _tap1 = v.take_tap().expect("first take should succeed");
    let tap2 = v.take_tap();
    assert!(tap2.is_none(), "second take_tap should return None");
    let _ = drain_events(&events);
}

#[test]
fn tap_handle_reports_correct_format() {
    let (mut v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let tap = v.take_tap().unwrap();
    let sr = tap.sample_rate();
    let ch = tap.channels();
    assert!(sr > 0, "sample_rate should be positive, got {sr}");
    assert!(ch > 0, "channels should be positive, got {ch}");
    assert_eq!(sr, v.sample_rate());
    assert_eq!(ch, v.channels());
    let _ = drain_events(&events);
}

// ============================================================================
// 11. CONFIG EDGE CASES
// Tests with extreme VoxConfig values.
// ============================================================================

#[test]
fn config_very_small_buffer() {
    let cfg = VoxConfig {
        buffer_ms: 10,
        ..Default::default()
    };
    let (v, events) = fresh_cfg(cfg);
    v.play(LEGAL_FILE).unwrap();

    // Should still play, just with tiny buffer
    thread::sleep(Duration::from_millis(200));
    assert!(v.is_active(), "should work with 10ms buffer");
    let _ = drain_events(&events);
}

#[test]
fn config_very_large_buffer() {
    let cfg = VoxConfig {
        buffer_ms: 5000,
        ..Default::default()
    };
    let (v, events) = fresh_cfg(cfg);
    v.play(LEGAL_FILE).unwrap();

    thread::sleep(Duration::from_millis(200));
    assert!(v.is_active(), "should work with 5000ms buffer");
    let _ = drain_events(&events);
}

#[test]
fn config_tiny_tap_capacity() {
    let cfg = VoxConfig {
        tap_capacity: 4,
        ..Default::default()
    };
    let (mut v, events) = fresh_cfg(cfg);
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let mut tap = v.take_tap().unwrap();
    // Should not panic even with tiny capacity
    for _ in 0..20 {
        let _ = tap.latest(100);
        thread::sleep(Duration::from_millis(10));
    }
    let _ = drain_events(&events);
}

#[test]
fn config_huge_tap_capacity() {
    let cfg = VoxConfig {
        tap_capacity: 1_000_000,
        ..Default::default()
    };
    let (mut v, events) = fresh_cfg(cfg);
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let mut tap = v.take_tap().unwrap();
    let samples = tap.latest(10_000);
    let _ = samples;
    let _ = drain_events(&events);
}

#[test]
fn config_fast_watchdog_tick() {
    let cfg = VoxConfig {
        watchdog_tick: Duration::from_millis(10),
        zombie_ticks: 2,
        ..Default::default()
    };
    let (v, events) = fresh_cfg(cfg);
    v.play(LEGAL_FILE).unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(v.is_active(), "fast watchdog should not interfere");
    let _ = drain_events(&events);
}

// ============================================================================
// 12. EVENT CHANNEL SATURATION
// Floods the engine with commands to saturate the event channel.
// ============================================================================

#[test]
fn event_channel_overflow_from_rapid_commands() {
    let (v, events) = fresh();

    // Fire many play/stop cycles without draining events
    for _ in 0..30 {
        let _ = v.play(LEGAL_FILE);
        thread::sleep(Duration::from_millis(5));
        v.stop();
        thread::sleep(Duration::from_millis(5));
    }

    // Now drain — should get events without panic
    let mut count = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match events.try_recv() {
            Some(_) => count += 1,
            None => {
                thread::sleep(Duration::from_millis(10));
                if events.try_recv().is_none() {
                    break;
                }
            }
        }
    }
    assert!(count > 0, "should have received some events");
}

// ============================================================================
// 13. POSITION/DURATION CONSISTENCY
// Checks that position/duration remain sane under rapid state changes.
// ============================================================================

#[test]
fn position_never_exceeds_duration() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let dur = v.duration().as_secs_f64();
    assert!(dur > 0.0, "duration should be positive");

    // Rapidly seek around and check position
    for i in 0..20 {
        let target = (i as f64 / 20.0) * dur;
        v.seek_to(target);
        thread::sleep(Duration::from_millis(30));
        let pos = v.position().as_secs_f64();
        // Allow some tolerance for timing, but position should never wildly exceed duration
        assert!(
            pos < dur + 2.0,
            "position {pos} should not exceed duration {dur} + 2s"
        );
    }
    let _ = drain_events(&events);
}

#[test]
fn position_resets_on_new_track() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    v.seek_to(10.0);
    thread::sleep(Duration::from_millis(200));
    let pos_before = v.position().as_secs_f64();
    assert!(pos_before > 5.0, "should have seeked forward");

    // Play a new track — position should reset
    v.play(LEGAL_FILE).unwrap();
    thread::sleep(Duration::from_millis(200));
    let pos_after = v.position().as_secs_f64();
    assert!(
        pos_after < 2.0,
        "position should reset on new track, got {pos_after}"
    );
    let _ = drain_events(&events);
}

#[test]
fn duration_updates_on_track_change() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);
    let dur1 = v.duration();

    v.play(PART1).unwrap();
    await_event_ignoring_device(&events);
    let dur2 = v.duration();

    // Different files should (likely) have different durations
    // At minimum, duration should be valid
    assert!(dur1.as_secs_f64() > 0.0);
    assert!(dur2.as_secs_f64() > 0.0);
    let _ = drain_events(&events);
}

// ============================================================================
// 14. STOP/IDLE OPERATIONS
// Tests calling operations when nothing is playing.
// ============================================================================

#[test]
fn stop_on_idle_engine() {
    let (v, events) = fresh();
    // Stop when nothing is playing — should be a no-op, no panic
    v.stop();
    thread::sleep(Duration::from_millis(50));
    assert!(!v.is_active());
    let _ = drain_events(&events);
}

#[test]
fn seek_on_idle_engine() {
    let (v, events) = fresh();
    // Seek when nothing is playing — should be a no-op
    v.seek_to(5.0);
    v.seek_relative(1.0);
    thread::sleep(Duration::from_millis(50));
    assert!(!v.is_active());
    let _ = drain_events(&events);
}

#[test]
fn pause_on_idle_engine() {
    let (v, events) = fresh();
    // Pause when nothing is playing — should not panic
    v.pause();
    thread::sleep(Duration::from_millis(50));
    assert!(!v.is_active());
    let _ = drain_events(&events);
}

#[test]
fn resume_on_idle_engine() {
    let (v, events) = fresh();
    // Resume when nothing is playing — should not panic
    v.resume();
    thread::sleep(Duration::from_millis(50));
    assert!(!v.is_active());
    let _ = drain_events(&events);
}

#[test]
fn set_next_on_idle_engine() {
    let (v, events) = fresh();
    // set_next when nothing is playing — should error with NotActive
    assert_eq!(v.set_next(LEGAL_FILE), Err(VoxError::NotActive));
    let _ = drain_events(&events);
}

#[test]
fn clear_next_on_idle_engine() {
    let (v, events) = fresh();
    v.clear_next(); // no-op, should not panic
    let _ = drain_events(&events);
}

#[test]
fn set_replaygain_on_idle_engine() {
    let (v, events) = fresh();
    v.set_replaygain(ReplayGainMode::Track);
    v.set_replaygain(ReplayGainMode::Album);
    v.set_replaygain(ReplayGainMode::Off);
    let _ = drain_events(&events);
}

// ============================================================================
// 15. ERROR RECOVERY
// Tests that the engine recovers from error conditions.
// ============================================================================

#[test]
fn recover_from_bad_file_then_play_good() {
    let (v, events) = fresh();

    // Play a bad file
    let res = v.play("nonexistent.mp3");
    assert_eq!(res, Err(VoxError::FileOpen("nonexistent.mp3".to_string())));

    // Should still be able to play a good file
    v.play(LEGAL_FILE).unwrap();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::TrackStarted { .. })));
    let _ = drain_events(&events);
}

#[test]
fn recover_from_text_file_then_play_good() {
    let (v, events) = fresh();

    // Play a text file — will open OK but decode fails
    v.play(TEXT_FILE).unwrap();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::Error { recoverable: true, .. })));

    // Should recover and play a real file
    v.play(LEGAL_FILE).unwrap();
    let e = await_track_started(&events);
    assert!(matches!(e, Some(VoxEvent::TrackStarted { .. })));
    let _ = drain_events(&events);
}

#[test]
fn set_next_bad_file_does_not_corrupt_engine() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // set_next with bad file — should emit error but engine stays alive
    v.set_next("nonexistent.mp3").unwrap_err();

    // set_next with a good file should still work
    v.set_next(PART2).unwrap();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::NextReady { .. })));
    let _ = drain_events(&events);
}

// ============================================================================
// 16. MIXED OPERATION STORM
// Combines many operation types in rapid succession.
// ============================================================================

#[test]
fn mixed_operation_storm() {
    let (v, events) = fresh();

    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // Rapid mixed operations
    v.pause();
    v.seek_to(5.0);
    v.set_replaygain(ReplayGainMode::Track);
    v.resume();
    v.seek_relative(-2.0);
    v.set_next(PART1).unwrap();
    v.clear_next();
    v.set_next(PART2).unwrap();
    v.pause();
    v.seek_to(0.0);
    v.set_replaygain(ReplayGainMode::Off);
    v.resume();
    v.seek_relative(3.0);

    thread::sleep(Duration::from_millis(300));
    assert!(v.is_active(), "should survive mixed operation storm");
    let _ = drain_events(&events);
}

#[test]
fn play_stop_play_seek_stop_play_cycle() {
    let (v, events) = fresh();

    v.play(LEGAL_FILE).unwrap();
    thread::sleep(Duration::from_millis(50));
    v.stop();
    thread::sleep(Duration::from_millis(50));

    v.play(PART1).unwrap();
    thread::sleep(Duration::from_millis(50));
    v.seek_to(2.0);
    thread::sleep(Duration::from_millis(50));
    v.stop();
    thread::sleep(Duration::from_millis(50));

    v.play(PART2).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(v.is_active(), "should be playing after play/stop/play/seek/stop/play");
    let _ = drain_events(&events);
}

// ============================================================================
// 17. EVENT ORDERING INVARIANTS
// Verifies that events arrive in the correct order under stress.
// ============================================================================

#[test]
fn track_started_before_track_ended_on_interrupt() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();

    let mut saw_started = false;
    let mut saw_ended = false;

    // Collect first two meaningful events
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && (!saw_started || !saw_ended) {
        match events.try_recv() {
            Some(VoxEvent::TrackStarted { .. }) => {
                assert!(!saw_ended, "TrackStarted must come before TrackEnded");
                saw_started = true;
            }
            Some(VoxEvent::TrackEnded { .. }) => {
                assert!(saw_started, "TrackEnded must come after TrackStarted");
                saw_ended = true;
            }
            Some(VoxEvent::DeviceChanged { .. }) => {}
            Some(_) => {}
            None => thread::sleep(Duration::from_millis(10)),
        }
    }

    assert!(saw_started, "should have seen TrackStarted");
    v.stop();
    let _ = drain_events(&events);
}

#[test]
fn stopped_event_after_track_ended() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    v.stop();

    let mut saw_ended = false;
    let mut saw_stopped = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !saw_stopped {
        match events.try_recv() {
            Some(VoxEvent::TrackEnded {
                reason: EndReason::Interrupted,
                ..
            }) => {
                saw_ended = true;
            }
            Some(VoxEvent::Stopped) => {
                assert!(saw_ended, "Stopped must come after TrackEnded");
                saw_stopped = true;
            }
            Some(VoxEvent::DeviceChanged { .. }) => {}
            Some(_) => {}
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
    assert!(saw_stopped, "should have seen Stopped after stop()");
}

// ============================================================================
// 18. SEEK PRECISION UNDER LOAD
// Verifies seek accuracy doesn't degrade under repeated seeks.
// ============================================================================

#[test]
fn repeated_seek_accuracy() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let targets = [1.0, 5.0, 10.0, 0.0, 15.0, 3.0, 8.0, 0.0];

    for &target in &targets {
        v.seek_to(target);
        // Wait for seek to land
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let pos = v.position().as_secs_f64();
            if (pos - target).abs() < 0.5 || Instant::now() > deadline {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    // Final seek to 0 should land precisely
    v.seek_to(0.0);
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let pos = v.position().as_secs_f64();
        if pos < 0.2 || Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let pos = v.position().as_secs_f64();
    assert!(pos < 0.3, "final seek to 0 should land near 0, got {pos}");
    let _ = drain_events(&events);
}

// ============================================================================
// 19. SAMPLE-LEVEL POSITION TRACKING
// Tests position_samples / duration_samples consistency.
// ============================================================================

#[test]
fn sample_position_is_consistent_with_duration() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let sr = v.sample_rate();
    let ch = v.channels();
    assert!(sr > 0 && ch > 0);

    // Wait a bit for playback
    thread::sleep(Duration::from_millis(300));

    let pos_samples = v.position_samples();
    let dur_samples = v.duration_samples();

    assert!(pos_samples > 0, "position_samples should advance");
    assert!(dur_samples > 0, "duration_samples should be positive");
    assert!(
        pos_samples <= dur_samples + (sr as u64 * ch as u64 * 2),
        "position_samples ({pos_samples}) should not wildly exceed duration_samples ({dur_samples})"
    );

    let _ = drain_events(&events);
}

// ============================================================================
// 20. ENGINE LIFECYCLE TRANSITIONS
// Tests full lifecycle: create -> play -> stop -> play -> drop.
// ============================================================================

#[test]
fn full_lifecycle_transitions() {
    let (v, events) = fresh();

    // Phase 1: play and seek
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);
    v.seek_to(3.0);
    thread::sleep(Duration::from_millis(100));
    assert!(v.is_active());

    // Phase 2: stop
    v.stop();
    thread::sleep(Duration::from_millis(100));
    assert!(!v.is_active());

    // Phase 3: play a different file
    v.play(PART1).unwrap();
    let e = await_track_started(&events);
    assert!(matches!(e, Some(VoxEvent::TrackStarted { .. })));

    // Phase 4: gapless transition
    v.set_next(PART2).unwrap();
    await_event_ignoring_device(&events);

    let dur = v.duration().as_secs_f64();
    v.seek_to(dur + 1.0);

    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::TrackEnded { reason: EndReason::EndOfStream, .. })));
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::TrackStarted { reason: StartReason::Gapless, .. })));

    // Phase 5: stop and drop
    v.stop();
    thread::sleep(Duration::from_millis(100));
    assert!(!v.is_active());
    drop(v);
    drop(events);
}

// ============================================================================
// 21. PATH HANDLING EDGE CASES
// Tests various path formats.
// ============================================================================

#[test]
fn path_with_spaces_and_special_chars() {
    let (v, events) = fresh();
    // The test file has spaces, apostrophes, and commas
    v.play(LEGAL_FILE).unwrap();
    let e = await_event_ignoring_device(&events);
    assert!(matches!(e, Some(VoxEvent::TrackStarted { .. })));
    let _ = drain_events(&events);
}

#[test]
fn empty_string_path() {
    let (v, events) = fresh();
    let res = v.play("");
    assert_eq!(res, Err(VoxError::FileOpen("".to_string())));
    let _ = drain_events(&events);
}

#[test]
fn directory_as_path() {
    let (v, events) = fresh();
    let res = v.play("tests/test_suite/");
    assert_eq!(res, Err(VoxError::FileOpen("tests/test_suite/".to_string())));
    let _ = drain_events(&events);
}

#[test]
fn nonexistent_path() {
    let (v, events) = fresh();
    let res = v.play("tests/test_suite/does_not_exist.mp3");
    assert_eq!(
        res,
        Err(VoxError::FileOpen("tests/test_suite/does_not_exist.mp3".to_string()))
    );
    let _ = drain_events(&events);
}

// ============================================================================
// 22. BOUNDARY VALUE TESTS
// Tests with extreme numeric values.
// ============================================================================

#[test]
fn seek_to_zero_repeatedly() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    for _ in 0..10 {
        v.seek_to(0.0);
        thread::sleep(Duration::from_millis(20));
    }

    thread::sleep(Duration::from_millis(100));
    let pos = v.position().as_secs_f64();
    assert!(pos < 1.0, "repeated seek-to-zero: got {pos}");
    let _ = drain_events(&events);
}

#[test]
fn seek_relative_zero() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    thread::sleep(Duration::from_millis(300));
    let before = v.position().as_secs_f64();

    v.seek_relative(0.0);
    thread::sleep(Duration::from_millis(100));
    let after = v.position().as_secs_f64();

    // Position should be roughly the same (accounting for playback time)
    assert!(
        (after - before).abs() < 0.5,
        "seek_relative(0.0) should not change position much: before={before}, after={after}"
    );
    let _ = drain_events(&events);
}

// ============================================================================
// 23. RAPID ENGINE RECREATION
// Creates and destroys engines rapidly to test cleanup.
// ============================================================================

#[test]
fn rapid_engine_recreation() {
    for i in 0..20 {
        let (v, events) = fresh();
        if i % 2 == 0 {
            let _ = v.play(LEGAL_FILE);
            thread::sleep(Duration::from_millis(10));
        }
        // Drop — should clean up without deadlock or leak
        drop(v);
        drop(events);
    }
}

#[test]
fn engine_recreation_with_different_configs() {
    let configs = [
        VoxConfig {
            buffer_ms: 10,
            ..Default::default()
        },
        VoxConfig {
            buffer_ms: 150,
            ..Default::default()
        },
        VoxConfig {
            buffer_ms: 1000,
            ..Default::default()
        },
        VoxConfig {
            tap_capacity: 4,
            ..Default::default()
        },
        VoxConfig {
            tap_capacity: 100_000,
            ..Default::default()
        },
        VoxConfig {
            watchdog_tick: Duration::from_millis(10),
            ..Default::default()
        },
        VoxConfig {
            zombie_ticks: 1,
            ..Default::default()
        },
    ];

    for cfg in configs {
        let (v, events) = fresh_cfg(cfg);
        v.play(LEGAL_FILE).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(v.is_active());
        drop(v);
        drop(events);
    }
}

// ============================================================================
// 24. EVENT DRAIN UNDER PRESSURE
// Ensures event draining works correctly under heavy load.
// ============================================================================

#[test]
fn recv_active_returns_when_engine_stops() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    thread::sleep(Duration::from_millis(100));
    v.stop();

    // recv_active should return None once the engine is inactive
    let start = Instant::now();
    let mut got_none = false;
    while start.elapsed() < Duration::from_secs(3) {
        if events.recv_active(Duration::from_millis(50)).is_none() {
            got_none = true;
            break;
        }
    }
    assert!(got_none, "recv_active should return None when engine stops");
}

#[test]
fn recv_timeout_respects_timeout() {
    let (_v, events) = fresh();
    // No playback — events should be empty
    let start = Instant::now();
    let result = events.recv_timeout(Duration::from_millis(100));
    let elapsed = start.elapsed();
    assert!(result.is_none(), "should timeout with no events");
    assert!(
        elapsed >= Duration::from_millis(80),
        "timeout should be respected: elapsed {elapsed:?}"
    );
}

// ============================================================================
// 25. CONCURRENT PLAY AND SET_NEXT
// Exercises the gapless queue while the engine is actively decoding.
// ============================================================================

#[test]
fn set_next_while_decode_is_busy() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    // Rapidly set and clear next while the decoder is busy
    for _ in 0..10 {
        let _ = v.set_next(PART1);
        v.clear_next();
        let _ = v.set_next(PART2);
        thread::sleep(Duration::from_millis(10));
    }

    thread::sleep(Duration::from_millis(100));
    assert!(v.is_active(), "should survive rapid set_next/clear_next");
    let _ = drain_events(&events);
}

#[test]
fn play_interrupts_gapless_setup() {
    let (v, events) = fresh();
    v.play(PART1).unwrap();
    await_event_ignoring_device(&events);

    v.set_next(PART2).unwrap();
    await_event_ignoring_device(&events);

    // Interrupt with a new play — should cancel the gapless queue
    v.play(LEGAL_FILE).unwrap();
    let e = await_track_started(&events);
    assert!(
        matches!(e, Some(VoxEvent::TrackStarted { reason: StartReason::Play, .. })),
        "interrupting play should emit Play start reason, got {e:?}"
    );

    // Now seek past end — no gapless should fire
    let dur = v.duration().as_secs_f64();
    v.seek_to(dur + 1.0);

    let e = await_event_ignoring_device(&events);
    assert!(
        matches!(e, Some(VoxEvent::TrackEnded { reason: EndReason::EndOfStream, .. })),
        "should end without gapless, got {e:?}"
    );

    // Drain and check no gapless TrackStarted follows
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        match events.try_recv() {
            Some(VoxEvent::TrackStarted { reason: StartReason::Gapless, .. }) => {
                panic!("unexpected gapless transition after play-interrupt")
            }
            Some(_) => {}
            None => {
                thread::sleep(Duration::from_millis(50));
                if events.try_recv().is_none() {
                    break;
                }
            }
        }
    }
}

// ============================================================================
// 26. OUTPUT FORMAT CONSISTENCY
// Checks sample_rate and channels stay consistent.
// ============================================================================

#[test]
fn output_format_is_stable_during_playback() {
    let (v, events) = fresh();
    v.play(LEGAL_FILE).unwrap();
    await_event_ignoring_device(&events);

    let sr = v.sample_rate();
    let ch = v.channels();

    // Query format repeatedly — should not change without a device event
    for _ in 0..20 {
        assert_eq!(v.sample_rate(), sr, "sample_rate should be stable");
        assert_eq!(v.channels(), ch, "channels should be stable");
        thread::sleep(Duration::from_millis(25));
    }
    let _ = drain_events(&events);
}

// ============================================================================
// 27. STRESS: ALL OPERATIONS IN TIGHT LOOP
// Final comprehensive stress test.
// ============================================================================

#[test]
fn all_operations_tight_loop() {
    let (v, events) = fresh();

    for i in 0..10 {
        let file = if i % 2 == 0 { LEGAL_FILE } else { PART1 };
        v.play(file).unwrap();
        v.pause();
        v.seek_to(i as f64);
        v.set_replaygain(ReplayGainMode::Track);
        v.resume();
        v.seek_relative(-0.5);
        v.set_replaygain(ReplayGainMode::Album);
        v.pause();
        v.seek_to(0.0);
        v.set_replaygain(ReplayGainMode::Off);
        v.resume();
        thread::sleep(Duration::from_millis(30));
    }

    v.stop();
    thread::sleep(Duration::from_millis(100));
    assert!(!v.is_active(), "should be idle after tight loop");
    let _ = drain_events(&events);
}
