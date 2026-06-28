use crate::error::{Result, VoxError};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    sync::LazyLock,
};
use symphonia::{
    core::{
        audio::{Channels, Position},
        codecs::{
            audio::{
                AudioCodecId, AudioCodecParameters, AudioDecoder as SymphoniaDecoder,
                AudioDecoderOptions,
                well_known::{CODEC_ID_MP3, CODEC_ID_OPUS, CODEC_ID_VORBIS},
            },
            registry::CodecRegistry,
        },
        errors::Error as SymphoniaErr,
        formats::{
            FormatOptions, FormatReader, SeekMode, SeekTo, SeekedTo, TrackType, probe::Hint,
        },
        io::MediaSourceStream,
        meta::{MetadataOptions, StandardTag},
        units::{Duration as UnitsDuration, Time, TimeBase, Timestamp},
    },
    default::{get_probe, register_enabled_codecs},
};

#[cfg(feature = "opus")]
use symphonia_adapter_libopus::OpusDecoder;

static CODEC_REGISTRY: LazyLock<CodecRegistry> = LazyLock::new(|| {
    let mut registry = CodecRegistry::new();
    register_enabled_codecs(&mut registry);
    #[cfg(feature = "opus")]
    {
        registry.register_audio_decoder::<OpusDecoder>();
    }
    registry
});

/// libopus needs an explicit channel layout when the container omits one;
/// Symphonia leaves `channels` as `None` for headerless Opus streams.
fn opus_params(audio_params: &AudioCodecParameters) -> AudioCodecParameters {
    let mut params = audio_params.clone();
    if audio_params.codec == CODEC_ID_OPUS && audio_params.channels.is_none() {
        params.channels = Some(Channels::Positioned(
            Position::FRONT_LEFT | Position::FRONT_RIGHT,
        ));
    }
    params
}

const MAX_PROBE_PACKETS: usize = 10;
/// Allowed maximum number of consecutive transient IO errors before giving up.
/// A successful read resets the count.
const MAX_IO_RETRIES: u32 = 3;
/// Unbroken run of undecodable packets tolerated mid-stream before treating it
/// as the end (~0.85s at 44.1 kHz). Resets on a good packet; only applies once
/// playback has begun — a broken intro is skipped without limit.
const MAX_CORRUPT_PACKETS: u32 = 32;

/// Codec-specific post-seek pre-roll. MP3/AAC settle within ~one frame, but Opus
/// needs ~80 ms to reconverge (RFC 7845 §4.5) and Vorbis a long MDCT window.
const OPUS_SEEK_PREROLL_MS: u64 = 80;
const VORBIS_SEEK_PREROLL_MS: u64 = 80;

/// A coarse seek that lands more than this far past the requested target is
/// treated as a failure to seek (not a normal keyframe overshoot). Cue-less
/// containers like matroska only index clusters they have already read, so a
/// backward seek into an un-indexed region returns `Ok` while staying put —
/// the landed position then sits tens of seconds past the target. This margin
/// sits well above real coarse granularity (tens of ms observed) and far below
/// such failures (tens of seconds), so a too-tight value only costs a needless
/// reopen, never correctness.
const SEEK_OVERSHOOT_TOLERANCE_SECS: f64 = 4.0;

/// Outcome of a [`VoxDecoder::seek`].
pub(crate) struct SeekLanding {
    /// Where playback landed, in seconds.
    pub secs: f64,
    /// Whether the codec still needs post-seek warm-up frames discarded to
    /// reconverge. True after a coarse seek (a discontinuous jump); false after
    /// the linear-decode fallback, which leaves the codec already warm and
    /// positioned exactly at the target.
    pub needs_warmup: bool,
}

impl SeekLanding {
    fn coarse(secs: f64) -> Self {
        Self { secs, needs_warmup: true }
    }
    fn linear(secs: f64) -> Self {
        Self { secs, needs_warmup: false }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AudioInfo {
    pub sample_rate: u32,
    pub channels: usize,
    pub codec: AudioCodecId,
    n_frames: Option<u64>,
    pub replaygain: ReplayGainInfo,
}

/// Which ReplayGain tags, if any, the engine applies during playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplayGainMode {
    /// Apply no gain; play at unity.
    #[default]
    Off,
    /// Apply per-track ReplayGain tags.
    Track,
    /// Apply per-album ReplayGain tags.
    Album,
}

/// ReplayGain values parsed from a track's metadata.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ReplayGainInfo {
    track_gain_db: Option<f32>,
    track_peak: Option<f32>,
    album_gain_db: Option<f32>,
    album_peak: Option<f32>,
}

impl ReplayGainInfo {
    /// Linear gain for `mode`, falling back to the other scope's tags and clamped
    /// against the peak so amplification can't clip. 1.0 when untagged.
    pub fn linear_gain(&self, mode: ReplayGainMode) -> f32 {
        let (gain_db, peak) = match mode {
            ReplayGainMode::Off => return 1.0,
            ReplayGainMode::Track => (
                self.track_gain_db.or(self.album_gain_db),
                self.track_peak.or(self.album_peak),
            ),
            ReplayGainMode::Album => (
                self.album_gain_db.or(self.track_gain_db),
                self.album_peak.or(self.track_peak),
            ),
        };
        let Some(db) = gain_db else { return 1.0 };
        let gain = 10f32.powf(db / 20.0);
        match peak.filter(|p| *p > 0.0 && *p <= 1.0) {
            Some(peak) => gain.min(1.0 / peak),
            None => gain,
        }
    }
}

pub(crate) struct VoxDecoder {
    path: String,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn SymphoniaDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    sample_buf: Vec<f32>,
    io_retries: u32,
    corrupt_packets: u32,
    /// Has any packet decoded yet? Separates a graceful tail-failure from a file
    /// that never decoded at all (fatal).
    decoded_any: bool,
    /// Reported duration is an estimate (headerless MP3) a scan should refine.
    duration_estimated: bool,
    /// Per-channel frames handed out so far. On a seek-free playthrough this is
    /// the renderable length, used to reconcile the reported duration at the end.
    frames_emitted: u64,
    seek_preroll_frames: u64,
    pub info: AudioInfo,
}

enum Recovery {
    Retry,
    Eof,
}

impl VoxDecoder {
    pub fn open(path: &str) -> Result<Self> {
        let mut format = open_format_reader(path)
            .map_err(|e| VoxError::Decoder(format!("Format reader error: {}", e)))?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| VoxError::Decoder("no audio track".into()))?;

        // Track-level timing lives on the Track in 0.6, not in codec params.
        let track_id = track.id;
        let n_frames = track.num_frames;
        let time_base = track.time_base;
        let track_duration = track.duration;

        let audio_params = track
            .codec_params
            .as_ref()
            .and_then(|cp| cp.audio())
            .ok_or_else(|| VoxError::Decoder("no audio codec params".into()))?;

        let metadata_sample_rate = audio_params.sample_rate;
        let metadata_channels = audio_params.channels.as_ref().map(|c| c.count());

        let decoder_params = opus_params(audio_params);

        let mut decoder = CODEC_REGISTRY
            .make_audio_decoder(&decoder_params, &AudioDecoderOptions::default())
            .map_err(|e| VoxError::Decoder(e.to_string()))?;

        let (sample_rate, channels) =
            if let (Some(sr), Some(ch)) = (metadata_sample_rate, metadata_channels) {
                (sr, ch)
            } else {
                let mut found_spec = None;

                for _ in 0..MAX_PROBE_PACKETS {
                    let packet = match format.next_packet() {
                        Ok(Some(p)) => p,
                        _ => break,
                    };

                    if packet.track_id != track_id {
                        continue;
                    }

                    match decoder.decode(&packet) {
                        Ok(d) => {
                            let spec = d.spec();
                            found_spec = Some((spec.rate(), spec.channels().count()));
                            break;
                        }
                        Err(SymphoniaErr::DecodeError(_)) => continue,
                        Err(_) => break,
                    }
                }
                let _ = format.seek(
                    SeekMode::Coarse,
                    SeekTo::Time {
                        time: Time::ZERO,
                        track_id: Some(track_id),
                    },
                );
                decoder.reset();

                found_spec.ok_or_else(|| {
                    VoxError::Decoder("Could not determine sample_rate and/or channels".into())
                })?
            };

        // Some containers omit the frame count; fall back to the timebase duration
        // so seek-clamping and duration reporting still work.
        let n_frames = n_frames.or_else(|| {
            let media = format.media_info();
            duration_secs(time_base, track_duration)
                .or_else(|| duration_secs(media.time_base, media.duration))
                .map(|secs| (secs * sample_rate as f64) as u64)
        });

        let replaygain = read_replaygain(&mut *format, track_id);

        let seek_preroll_frames = (sample_rate as u64
            * match decoder_params.codec {
                CODEC_ID_OPUS => OPUS_SEEK_PREROLL_MS,
                CODEC_ID_VORBIS => VORBIS_SEEK_PREROLL_MS,
                _ => 0,
            })
            / 1000;

        let duration_estimated =
            decoder_params.codec == CODEC_ID_MP3 && mp3_duration_is_estimated(path);

        Ok(VoxDecoder {
            path: path.to_string(),
            format,
            decoder,
            track_id,
            time_base,
            sample_buf: Vec::new(),
            io_retries: 0,
            corrupt_packets: 0,
            decoded_any: false,
            duration_estimated,
            frames_emitted: 0,
            seek_preroll_frames,
            info: AudioInfo {
                sample_rate,
                channels,
                codec: decoder_params.codec,
                n_frames,
                replaygain,
            },
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn playable_duration(&self) -> Option<f64> {
        let frames = self.info.n_frames?;
        Some(frames as f64 / self.info.sample_rate as f64)
    }

    /// Whether the reported duration is an estimate a scan should refine.
    pub(crate) fn duration_estimated(&self) -> bool {
        self.duration_estimated
    }

    /// Per-channel frames decoded so far. Equals the renderable length only on a
    /// seek-free playthrough (a seek re-decodes/skips without resetting the tally).
    pub(crate) fn frames_emitted(&self) -> u64 {
        self.frames_emitted
    }

    /// Frames to decode-and-discard after a seek so the codec reconverges.
    pub fn seek_preroll_frames(&self) -> u64 {
        self.seek_preroll_frames
    }

    /// Classify a symphonia error: end-of-stream, retry, or fatal. Only IO retries
    /// exhaust to a fatal error; decode failures never do.
    fn recover(&mut self, e: SymphoniaErr) -> Result<Recovery> {
        match e {
            SymphoniaErr::IoError(ref io) if io.kind() == io::ErrorKind::UnexpectedEof => {
                Ok(Recovery::Eof)
            }
            SymphoniaErr::IoError(_) => {
                self.io_retries += 1;
                if self.io_retries > MAX_IO_RETRIES {
                    return Err(VoxError::Decoder(format!("io retries exhausted: {e}")));
                }
                Ok(Recovery::Retry)
            }
            SymphoniaErr::DecodeError(_) => {
                // No audio yet: still hunting for the first good frame, so skip a
                // broken intro (re-encodes can leave a long reservoir-corrupt
                // preamble) without limit. The budget detects the end of a
                // *playing* stream, not a bad start.
                if !self.decoded_any {
                    return Ok(Recovery::Retry);
                }
                self.corrupt_packets += 1;
                if self.corrupt_packets > MAX_CORRUPT_PACKETS {
                    // A sustained run after playback = the stream's true end.
                    return Ok(Recovery::Eof);
                }
                Ok(Recovery::Retry)
            }
            SymphoniaErr::ResetRequired => {
                self.decoder.reset();
                Ok(Recovery::Retry)
            }
            e => Err(VoxError::Decoder(e.to_string())),
        }
    }

    pub fn next_packet(&mut self) -> Result<Option<&[f32]>> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => break,
                Err(e) => match self.recover(e)? {
                    Recovery::Eof => break,
                    Recovery::Retry => continue,
                },
            };

            if packet.track_id != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(e) => match self.recover(e)? {
                    Recovery::Eof => break,
                    Recovery::Retry => continue,
                },
            };

            if decoded.frames() == 0 {
                continue;
            }

            decoded.copy_to_vec_interleaved::<f32>(&mut self.sample_buf);

            // Progress: reset the recovery budgets so tolerance is per corrupt
            // *run*, not per track.
            self.io_retries = 0;
            self.corrupt_packets = 0;
            self.decoded_any = true;
            self.frames_emitted += (self.sample_buf.len() / self.info.channels.max(1)) as u64;
            return Ok(Some(&self.sample_buf));
        }

        // End of stream: nothing decoded means no usable audio, not a normal end.
        if self.decoded_any {
            Ok(None)
        } else {
            Err(VoxError::Decoder("no decodable audio in stream".into()))
        }
    }

    /// Decode and discard up to `frames` input-rate frames, returning how many
    /// were skipped. Used after a seek to trim codec warm-up.
    pub fn skip_samples(&mut self, frames: u64) -> Result<u64> {
        let mut skipped = 0u64;
        while skipped < frames {
            match self.next_packet()? {
                Some(buf) => skipped += (buf.len() / self.info.channels.max(1)) as u64,
                None => break,
            }
        }
        Ok(skipped)
    }

    /// Seek to `secs`, returning where playback landed (see [`SeekLanding`]).
    ///
    /// Three tiers, each falling through when the prior one fails or — for a
    /// cue-less container — returns `Ok` while staying put (detected as a large
    /// overshoot past the target):
    /// 1. coarse seek from the current position (the fast path for indexed files);
    /// 2. reopen + coarse seek from byte 0, so a backward target becomes a forward
    ///    scan an incrementally-built index can satisfy;
    /// 3. reopen + linear decode-and-discard to the target frame, which works for
    ///    any codec/container with or without cue points (including seek-to-0,
    ///    which some readers reject as out-of-range).
    pub fn seek(&mut self, secs: f64) -> Result<SeekLanding> {
        let time = Time::try_from_secs_f64(secs)
            .ok_or_else(|| VoxError::Seek("invalid seek time".into()))?;

        let track_id = self.track_id;
        let make_seek_to = || SeekTo::Time {
            time,
            track_id: Some(track_id),
        };
        let landed_close = |actual: f64| actual <= secs + SEEK_OVERSHOOT_TOLERANCE_SECS;

        // Tier 1: coarse seek in place.
        if let Ok(s) = self.format.seek(SeekMode::Coarse, make_seek_to()) {
            let actual = self.seeked_secs(s, secs);
            if landed_close(actual) {
                self.decoder.reset();
                self.sample_buf.clear();
                return Ok(SeekLanding::coarse(actual));
            }
        }

        // Tier 2: reopen and coarse seek from the start.
        self.reopen()?;
        if let Ok(s) = self.format.seek(SeekMode::Coarse, make_seek_to()) {
            let actual = self.seeked_secs(s, secs);
            if landed_close(actual) {
                self.decoder.reset();
                self.sample_buf.clear();
                return Ok(SeekLanding::coarse(actual));
            }
        }

        // Tier 3: reopen and decode forward to the target. No `decoder.reset()` —
        // the codec is already warm from decoding continuously up to here, and is
        // positioned exactly at the target, so the caller must not apply post-seek
        // warm-up (it would discard real audio and offset the position).
        self.reopen()?;
        let target_frames = (secs * self.info.sample_rate as f64) as u64;
        let skipped = self.skip_samples(target_frames)?;
        Ok(SeekLanding::linear(skipped as f64 / self.info.sample_rate as f64))
    }

    /// Landed timestamp of a successful seek, in seconds. Falls back to the
    /// requested `secs` when the timebase can't convert the timestamp.
    fn seeked_secs(&self, seeked: SeekedTo, secs: f64) -> f64 {
        match self.time_base {
            Some(tb) => tb
                .calc_time(seeked.actual_ts)
                .map(|t| t.as_secs_f64())
                .unwrap_or(secs),
            None => seeked.actual_ts.get() as f64 / self.info.sample_rate as f64,
        }
    }

    fn reopen(&mut self) -> Result<()> {
        let format = open_format_reader(&self.path)
            .map_err(|e| VoxError::Seek(format!("Failed to reopen for seek: {e}")))?;

        let track = format
            .tracks()
            .iter()
            .find(|t| t.id == self.track_id)
            .ok_or_else(|| VoxError::Seek("track not found on reopen".into()))?;

        let audio_params = track
            .codec_params
            .as_ref()
            .and_then(|cp| cp.audio())
            .ok_or_else(|| VoxError::Seek("no audio params on reopen".into()))?;

        let decoder_params = opus_params(audio_params);

        let decoder = CODEC_REGISTRY
            .make_audio_decoder(&decoder_params, &AudioDecoderOptions::default())
            .map_err(|e| VoxError::Seek(e.to_string()))?;

        self.format = format;
        self.decoder = decoder;
        // Reading restarts from byte 0: tolerate a broken intro again and let a
        // genuinely empty stream surface as fatal (see `next_packet`).
        self.decoded_any = false;
        self.io_retries = 0;
        self.corrupt_packets = 0;

        Ok(())
    }
}

/// Convert a timebase-unit duration into seconds.
fn duration_secs(tb: Option<TimeBase>, dur: Option<UnitsDuration>) -> Option<f64> {
    let ts = dur?.timestamp_from(Timestamp::ZERO)?;
    tb?.calc_time(ts).map(|t| t.as_secs_f64())
}

/// Collect ReplayGain tags from the media-level and per-track metadata.
fn read_replaygain(format: &mut dyn FormatReader, track_id: u32) -> ReplayGainInfo {
    let mut rg = ReplayGainInfo::default();

    let mut metadata = format.metadata();
    let Some(revision) = metadata.skip_to_latest() else {
        return rg;
    };

    let track_tags = revision
        .per_track
        .iter()
        .filter(|t| t.track_id == track_id as u64)
        .flat_map(|t| t.metadata.tags.iter());

    for tag in revision.media.tags.iter().chain(track_tags) {
        match &tag.std {
            Some(StandardTag::ReplayGainTrackGain(v)) => rg.track_gain_db = parse_rg(v),
            Some(StandardTag::ReplayGainTrackPeak(v)) => rg.track_peak = parse_rg(v),
            Some(StandardTag::ReplayGainAlbumGain(v)) => rg.album_gain_db = parse_rg(v),
            Some(StandardTag::ReplayGainAlbumPeak(v)) => rg.album_peak = parse_rg(v),
            _ => {}
        }
    }
    rg
}

/// Parse a ReplayGain tag value, tolerating a trailing unit like "dB".
fn parse_rg(value: &str) -> Option<f32> {
    value
        .trim()
        .trim_end_matches(|c: char| c.is_alphabetic())
        .trim()
        .parse()
        .ok()
}

/// True when the first frame carries no Xing/Info/VBRI header (those give an
/// exact frame count), so the duration is only an estimate. Both failure modes
/// are benign: a miss costs a needless scan, a false hit keeps the estimate.
fn mp3_duration_is_estimated(path: &str) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };

    // Skip a leading ID3v2 tag so we land on the first MPEG frame, where any
    // Xing/Info/VBRI header lives.
    let mut head = [0u8; 10];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    if f.seek(SeekFrom::Start(id3v2_len(&head))).is_err() {
        return false;
    }

    // 2 KiB covers the largest possible first frame (320 kbps, 44.1 kHz).
    let mut buf = [0u8; 2048];
    let n = f.read(&mut buf).unwrap_or(0);
    !has_frame_count_header(&buf[..n])
}

/// Byte length of a leading ID3v2 tag (header + body + optional footer), or 0 if
/// `head` (the first 10 bytes of the file) is not an ID3v2 header.
fn id3v2_len(head: &[u8; 10]) -> u64 {
    if &head[0..3] != b"ID3" {
        return 0;
    }
    // The size is a 28-bit synchsafe integer (7 bits per byte).
    let size = ((head[6] as u64 & 0x7f) << 21)
        | ((head[7] as u64 & 0x7f) << 14)
        | ((head[8] as u64 & 0x7f) << 7)
        | (head[9] as u64 & 0x7f);
    let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

/// Whether a Xing/Info/VBRI header — which carries an exact frame count — is
/// present in the given first-frame bytes.
fn has_frame_count_header(first_frame: &[u8]) -> bool {
    first_frame
        .windows(4)
        .any(|w| w == b"Xing" || w == b"Info" || w == b"VBRI")
}

fn open_format_reader(p: &str) -> Result<Box<dyn FormatReader>> {
    let path = PathBuf::from(p);
    let file = File::open(&path).map_err(|_| VoxError::FileOpen(p.to_string()))?;

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let probed = get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| VoxError::Decoder(e.to_string()))?;

    Ok(probed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No well-formed fixture should be flagged for a duration scan: non-MP3
    /// formats carry exact counts, and every MP3 here has a Xing/Info header.
    /// Guards against the detector over-flagging valid files (needless scans).
    #[test]
    fn valid_fixtures_are_not_scanned() {
        for f in [
            "sine.flac",
            "sine.aiff",
            "sine.alac.m4a",
            "sine.m4a",
            "sine.ogg",
            "sine.mp3",
            "part1.mp3",
            "part2.mp3",
            "Preludes, Op. 28 - No. 16 'Hades'.mp3",
        ] {
            let path = format!("tests/test_suite/{f}");
            let decoder = VoxDecoder::open(&path).unwrap_or_else(|e| panic!("open {f}: {e:?}"));
            assert!(
                !decoder.duration_estimated(),
                "{f} should not be flagged for a duration scan"
            );
        }
    }

    #[test]
    fn id3v2_len_parses_synchsafe_size() {
        // No ID3v2 tag (a raw MPEG frame sync) -> length 0.
        assert_eq!(id3v2_len(&[0xFF, 0xFB, 0x90, 0, 0, 0, 0, 0, 0, 0]), 0);
        // "ID3", v2.4, no flags, synchsafe size = (2 << 7) | 1 = 257.
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00\x00\x02\x01"), 10 + 257);
        // Footer flag (bit 4) set -> add 10 more bytes.
        assert_eq!(id3v2_len(b"ID3\x04\x00\x10\x00\x00\x02\x01"), 10 + 257 + 10);
    }

    #[test]
    fn detects_vbr_header_presence() {
        assert!(has_frame_count_header(b"\x00\x00\x00Xing\x00\x00"));
        assert!(has_frame_count_header(b"....Info...."));
        assert!(has_frame_count_header(b"VBRI then audio"));
        assert!(!has_frame_count_header(b"plain cbr audio data"));
    }

    #[test]
    fn parse_rg_tolerates_units() {
        assert_eq!(parse_rg("-6.0 dB"), Some(-6.0));
        assert_eq!(parse_rg("+2.35dB"), Some(2.35));
        assert_eq!(parse_rg("0.987654"), Some(0.987654));
        assert_eq!(parse_rg("garbage"), None);
    }

    #[test]
    fn linear_gain_modes_and_fallbacks() {
        let rg = ReplayGainInfo {
            track_gain_db: Some(-6.0),
            track_peak: Some(0.5),
            album_gain_db: None,
            album_peak: None,
        };

        assert_eq!(rg.linear_gain(ReplayGainMode::Off), 1.0);
        let track = rg.linear_gain(ReplayGainMode::Track);
        assert!((track - 0.501).abs() < 0.001);
        // Album falls back to track tags when album tags are absent
        assert_eq!(rg.linear_gain(ReplayGainMode::Album), track);
        // No tags at all → unity
        assert_eq!(
            ReplayGainInfo::default().linear_gain(ReplayGainMode::Track),
            1.0
        );
    }

    #[test]
    fn linear_gain_peak_clamps_amplification() {
        let rg = ReplayGainInfo {
            track_gain_db: Some(12.0), // ~3.98x, would clip
            track_peak: Some(0.8),
            album_gain_db: None,
            album_peak: None,
        };
        assert_eq!(rg.linear_gain(ReplayGainMode::Track), 1.0 / 0.8);
    }

    // -- File-backed decode tests ----------------------------------
    // Exercise the real Symphonia path so codec/registry regressions surface.

    const SAMPLE_MP3: &str = "tests/test_suite/Preludes, Op. 28 - No. 16 'Hades'.mp3";

    #[test]
    fn open_reports_audio_format() {
        let d = VoxDecoder::open(SAMPLE_MP3).unwrap();
        assert!(d.info.sample_rate > 0);
        assert!(d.info.channels > 0);
        assert_eq!(d.path(), SAMPLE_MP3);
    }

    #[test]
    fn open_nonexistent_errors() {
        assert!(VoxDecoder::open("tests/test_suite/does_not_exist.mp3").is_err());
    }

    #[test]
    fn open_non_audio_errors() {
        assert!(VoxDecoder::open("tests/test_suite/invalid_filetype.txt").is_err());
    }

    #[test]
    fn playable_duration_is_positive() {
        let d = VoxDecoder::open(SAMPLE_MP3).unwrap();
        let dur = d.playable_duration().expect("mp3 should report a duration");
        assert!(dur > 0.0, "got {dur}");
    }

    #[test]
    fn next_packet_yields_interleaved_frames() {
        let mut d = VoxDecoder::open(SAMPLE_MP3).unwrap();
        let channels = d.info.channels;
        let buf = d
            .next_packet()
            .unwrap()
            .expect("expected at least one decoded packet");
        assert!(!buf.is_empty());
        assert_eq!(buf.len() % channels, 0, "buffer must be whole frames");
    }

    #[test]
    fn seek_lands_near_target() {
        let mut d = VoxDecoder::open(SAMPLE_MP3).unwrap();
        let landed = d.seek(5.0).unwrap().secs;
        assert!((landed - 5.0).abs() < 0.5, "landed at {landed}");
    }

    #[test]
    fn seek_backward_returns_to_start() {
        let mut d = VoxDecoder::open(SAMPLE_MP3).unwrap();
        d.seek(5.0).unwrap();
        let landed = d.seek(0.0).unwrap().secs;
        assert!(landed < 0.5, "landed at {landed}");
    }

    #[test]
    fn skip_samples_advances_past_request() {
        let mut d = VoxDecoder::open(SAMPLE_MP3).unwrap();
        // Whole packets are decoded, so the count meets-or-overshoots the target.
        let skipped = d.skip_samples(1000).unwrap();
        assert!(skipped >= 1000, "skipped {skipped}");
    }

    // A cue-less matroska/webm: its reader only indexes clusters it has read, so
    // `format.seek` returns Ok while staying put on a backward seek into an
    // un-indexed region. Guards the reopen-based fallback that lands correctly.
    const CUELESS_WEBM: &str = "tests/test_suite/sine.webm";

    #[test]
    fn seek_backward_after_forward_seek_lands_on_target() {
        let mut d = VoxDecoder::open(CUELESS_WEBM).unwrap();
        // Forward seek leaves the region before it un-indexed (the case where a
        // naive backward seek silently stays put at ~8s).
        d.seek(8.0).unwrap();
        let landed = d.seek(2.0).unwrap().secs;
        assert!((landed - 2.0).abs() < 0.5, "landed at {landed}, expected ~2s");
    }

    #[test]
    fn seek_to_zero_on_cueless_webm_succeeds() {
        let mut d = VoxDecoder::open(CUELESS_WEBM).unwrap();
        d.seek(8.0).unwrap();
        // Seek-to-0 is rejected/ignored by the reader; the linear tier handles it.
        let landed = d.seek(0.0).unwrap().secs;
        assert!(landed < 0.5, "landed at {landed}, expected ~0s");
    }
}
