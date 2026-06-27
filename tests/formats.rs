use std::time::Duration;
use voxio::{Vox, VoxEvent};

const MP3: &str = "tests/test_suite/sine.mp3";
const AAC_M4A: &str = "tests/test_suite/sine.m4a";
const FLAC: &str = "tests/test_suite/sine.flac";
const ALAC_M4A: &str = "tests/test_suite/sine.alac.m4a";
const OGG_VORBIS: &str = "tests/test_suite/sine.ogg";
const WAV: &str = "tests/test_suite/sine.wav";
const AIFF: &str = "tests/test_suite/sine.aiff";
const OPUS: &str = "tests/test_suite/sine.opus";

fn await_track_started(events: &voxio::VoxEvents) -> Option<VoxEvent> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match events.recv_timeout(Duration::from_millis(100)) {
            Some(VoxEvent::DeviceChanged { .. }) => continue,
            Some(e) => return Some(e),
            None if std::time::Instant::now() > deadline => return None,
            None => {}
        }
    }
}

fn assert_plays(path: &str) {
    let (vox, events) = Vox::new().expect("engine init");
    vox.play(path).unwrap_or_else(|e| panic!("{path}: play() failed: {e}"));
    let event = await_track_started(&events);
    assert!(
        matches!(event, Some(VoxEvent::TrackStarted { .. })),
        "{path}: expected TrackStarted, got {event:?}"
    );
    let dur = vox.duration();
    assert!(
        dur.as_secs_f64() > 0.5,
        "{path}: expected duration > 0.5s, got {dur:?}"
    );
    let sr = vox.sample_rate();
    assert!(sr > 0, "{path}: expected sample_rate > 0, got {sr}");
    let ch = vox.channels();
    assert!(ch > 0, "{path}: expected channels > 0, got {ch}");

    // Let it play briefly to confirm audio actually flows
    std::thread::sleep(Duration::from_millis(200));
    let pos = vox.position().as_secs_f64();
    assert!(
        pos > 0.05,
        "{path}: position should advance during playback, got {pos}s"
    );
}

fn assert_decode_error(path: &str) {
    let (vox, events) = Vox::new().expect("engine init");
    vox.play(path).unwrap_or_else(|e| panic!("{path}: play() failed: {e}"));
    let event = await_track_started(&events);
    assert!(
        matches!(
            event,
            Some(VoxEvent::Error {
                error: voxio::VoxError::Decoder(_),
                recoverable: true
            })
        ),
        "{path}: expected recoverable Decoder error, got {event:?}"
    );
}

// ============================================================================
// FORMAT SUPPORT: each claimed format must successfully decode and play.
// ============================================================================

#[test]
fn format_mp3_plays() {
    assert_plays(MP3);
}

#[test]
fn format_aac_plays() {
    assert_plays(AAC_M4A);
}

#[test]
fn format_flac_plays() {
    assert_plays(FLAC);
}

#[test]
fn format_alac_plays() {
    assert_plays(ALAC_M4A);
}

#[test]
fn format_ogg_vorbis_plays() {
    assert_plays(OGG_VORBIS);
}

#[test]
fn format_wav_plays() {
    assert_plays(WAV);
}

#[test]
fn format_aiff_plays() {
    assert_plays(AIFF);
}

#[test]
fn format_opus_plays() {
    assert_plays(OPUS);
}

// ============================================================================
// FORMAT STRESS: rapid play/stop/gapless across all formats.
// ============================================================================

#[test]
fn all_formats_gapless_chain() {
    let (vox, events) = Vox::new().expect("engine init");
    let formats = [MP3, FLAC, WAV, AIFF, AAC_M4A, OGG_VORBIS, OPUS];

    vox.play(formats[0]).unwrap();
    let e = await_track_started(&events);
    assert!(matches!(e, Some(VoxEvent::TrackStarted { reason: voxio::StartReason::Play, .. })));

    // Chain each format gaplessly
    for &fmt in &formats[1..] {
        vox.set_next(fmt).unwrap();
        let e = await_track_started(&events);
        assert!(
            matches!(e, Some(VoxEvent::NextReady { .. }) | Some(VoxEvent::TrackStarted { .. })),
            "expected NextReady or TrackStarted for {fmt}, got {e:?}"
        );
    }

    // Stop and drain
    vox.stop();
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
fn all_formats_rapid_switch() {
    let (vox, _events) = Vox::new().expect("engine init");
    let formats = [MP3, FLAC, WAV, AIFF, AAC_M4A, OGG_VORBIS, OPUS];

    for &fmt in &formats {
        vox.play(fmt).unwrap();
        std::thread::sleep(Duration::from_millis(50));
    }

    // Last format should be playing
    std::thread::sleep(Duration::from_millis(200));
    assert!(vox.is_active(), "should be active after rapid format switch");

    vox.stop();
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
fn all_formats_seek_and_resume() {
    let (vox, events) = Vox::new().expect("engine init");
    let formats = [MP3, FLAC, WAV, AIFF, AAC_M4A, OGG_VORBIS, OPUS];

    for &fmt in &formats {
        vox.play(fmt).unwrap();
        let _ = await_track_started(&events);

        // Seek forward
        vox.seek_to(0.5);
        std::thread::sleep(Duration::from_millis(50));

        // Seek back
        vox.seek_to(0.0);
        std::thread::sleep(Duration::from_millis(50));

        // Pause and resume
        vox.pause();
        vox.resume();
        std::thread::sleep(Duration::from_millis(50));

        vox.stop();
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ============================================================================
// NEGATIVE: confirm non-audio files are rejected at decode.
// ============================================================================

#[test]
fn plaintext_file_errors_at_decode() {
    assert_decode_error("tests/test_suite/invalid_filetype.txt");
}

// ============================================================================
// FORMAT + REPLAYGAIN: verify ReplayGain doesn't break any format.
// ============================================================================

#[test]
fn replaygain_does_not_break_any_format() {
    let (vox, events) = Vox::new().expect("engine init");
    let formats = [MP3, FLAC, WAV, AIFF, AAC_M4A, OGG_VORBIS, OPUS];

    for &fmt in &formats {
        vox.set_replaygain(voxio::ReplayGainMode::Track);
        vox.play(fmt).unwrap();
        let _ = await_track_started(&events);
        std::thread::sleep(Duration::from_millis(100));
        assert!(vox.is_active(), "{fmt}: should survive ReplayGain Track mode");

        vox.set_replaygain(voxio::ReplayGainMode::Album);
        std::thread::sleep(Duration::from_millis(50));
        assert!(vox.is_active(), "{fmt}: should survive ReplayGain Album mode");

        vox.set_replaygain(voxio::ReplayGainMode::Off);
        std::thread::sleep(Duration::from_millis(50));
        assert!(vox.is_active(), "{fmt}: should survive ReplayGain Off mode");

        vox.stop();
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ============================================================================
// CROSS-FORMAT GAPLESS: transition between different codecs.
// ============================================================================

#[test]
fn cross_format_gapless_mp3_to_flac() {
    let (vox, events) = Vox::new().expect("engine init");
    vox.play(MP3).unwrap();
    let _ = await_track_started(&events);
    vox.set_next(FLAC).unwrap();

    // Seek past end to force immediate gapless transition
    let dur = vox.duration().as_secs_f64();
    vox.seek_to(dur + 1.0);

    let e = await_track_started(&events);
    assert!(
        matches!(e, Some(VoxEvent::TrackEnded { reason: voxio::EndReason::EndOfStream, .. }) | Some(VoxEvent::TrackStarted { .. })),
        "expected gapless transition mp3->flac, got {e:?}"
    );
}

#[test]
fn cross_format_gapless_wav_to_opus() {
    let (vox, events) = Vox::new().expect("engine init");
    vox.play(WAV).unwrap();
    let _ = await_track_started(&events);
    vox.set_next(OPUS).unwrap();

    let dur = vox.duration().as_secs_f64();
    vox.seek_to(dur + 1.0);

    let e = await_track_started(&events);
    assert!(
        matches!(e, Some(VoxEvent::TrackEnded { .. }) | Some(VoxEvent::TrackStarted { .. })),
        "expected gapless transition wav->opus, got {e:?}"
    );
}

#[test]
fn cross_format_gapless_ogg_to_aac() {
    let (vox, events) = Vox::new().expect("engine init");
    vox.play(OGG_VORBIS).unwrap();
    let _ = await_track_started(&events);
    vox.set_next(AAC_M4A).unwrap();

    let dur = vox.duration().as_secs_f64();
    vox.seek_to(dur + 1.0);

    let e = await_track_started(&events);
    assert!(
        matches!(e, Some(VoxEvent::TrackEnded { .. }) | Some(VoxEvent::TrackStarted { .. })),
        "expected gapless transition ogg->aac, got {e:?}"
    );
}

#[test]
fn cross_format_gapless_aiff_to_alac() {
    let (vox, events) = Vox::new().expect("engine init");
    vox.play(AIFF).unwrap();
    let _ = await_track_started(&events);
    vox.set_next(ALAC_M4A).unwrap();

    let dur = vox.duration().as_secs_f64();
    vox.seek_to(dur + 1.0);

    let e = await_track_started(&events);
    assert!(
        matches!(e, Some(VoxEvent::TrackEnded { .. }) | Some(VoxEvent::TrackStarted { .. })),
        "expected gapless transition aiff->alac, got {e:?}"
    );
}
