use std::{
    fs::File,
    io::{self},
    path::PathBuf,
    sync::LazyLock,
};
use symphonia::{
    core::{
        audio::{Channels, SampleBuffer},
        codecs::{
            CODEC_TYPE_NULL, CODEC_TYPE_OPUS, CodecRegistry, Decoder as SymphoniaDecoder,
            DecoderOptions,
        },
        errors::Error as SymphError,
        formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
        io::MediaSourceStream,
        meta::MetadataOptions,
        probe::Hint,
        units::Time,
    },
    default::{get_probe, register_enabled_codecs},
};
use symphonia_adapter_libopus::OpusDecoder;

use crate::{
    MAX_PROBE_PACKETS,
    error::{Result, VoxError},
};

static CODEC_REGISTRY: LazyLock<CodecRegistry> = LazyLock::new(|| {
    let mut registry = CodecRegistry::new();
    register_enabled_codecs(&mut registry);
    registry.register_all::<OpusDecoder>();
    registry
});

#[derive(Debug, Clone)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: usize,
    pub n_frames: Option<u64>,
}

pub(crate) struct VoxDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn SymphoniaDecoder>,
    track_id: u32,
    sample_buf: Option<SampleBuffer<f32>>,
    pub info: AudioInfo,
    delay_samples: u64,
    padding_samples: u64,
    total_samples: Option<u64>,
    samples_decoded: u64,
}

impl VoxDecoder {
    pub fn open<S: AsRef<str>>(path: S) -> Result<Self> {
        let path = path.as_ref();

        let mut format = open_format_reader(&path)
            .map_err(|e| VoxError::Decoder(format!("Format reader error: {}", e)))?;

        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or(VoxError::Decoder("No track!".into()))?;

        let codec_params = &track.codec_params;
        let track_id = track.id;

        let metadata_sample_rate = codec_params.sample_rate;
        let metadata_channels = codec_params.channels.map(|c| c.count());
        let n_frames = codec_params.n_frames;
        let metadata_complete = metadata_sample_rate.is_some() && metadata_channels.is_some();

        // For Opus in MKV/WebM, channel info may be missing. Default to stereo.
        let decoder_params =
            if codec_params.codec == CODEC_TYPE_OPUS && codec_params.channels.is_none() {
                let mut params = codec_params.clone();
                params.channels = Some(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
                params
            } else {
                codec_params.clone()
            };

        let mut decoder = CODEC_REGISTRY
            .make(&decoder_params, &DecoderOptions::default())
            .map_err(|e| VoxError::Decoder(e.to_string()))?;

        // If metadata is incomplete, probe first packets to determine format
        // Extract gapless metadata (encoder delay and padding)
        // These are populated by Symphonia when enable_gapless is true
        let delay_samples = codec_params.delay.unwrap_or(0) as u64;
        let padding_samples = codec_params.padding.unwrap_or(0) as u64;
        let total_samples = codec_params.n_frames;

        let (sample_rate, channels) = if metadata_complete {
            (metadata_sample_rate.unwrap(), metadata_channels.unwrap())
        } else {
            let mut found_spec = None;

            for _ in 0..MAX_PROBE_PACKETS {
                let packet = match format.next_packet() {
                    Ok(p) => p,
                    Err(_) => break,
                };

                if packet.track_id() != track_id {
                    continue;
                }

                match decoder.decode(&packet) {
                    Ok(decoded) => {
                        let spec = *decoded.spec();
                        found_spec = Some((spec.rate, spec.channels.count()));
                        break;
                    }
                    Err(SymphError::DecodeError(_)) => continue,
                    Err(_) => break,
                }
            }

            // Seek back to beginning after probing
            let _ = format.seek(
                SeekMode::Coarse,
                SeekTo::Time {
                    time: Time::from(0.0),
                    track_id: Some(track_id),
                },
            );
            decoder.reset();

            found_spec.ok_or_else(|| {
                VoxError::Decoder("Could not determine sample rate and channels".into())
            })?
        };

        Ok(VoxDecoder {
            format,
            decoder,
            track_id,
            sample_buf: None,
            info: AudioInfo {
                sample_rate,
                channels,
                n_frames,
            },
            delay_samples,
            padding_samples,
            total_samples,
            samples_decoded: 0,
        })
    }

    pub fn next_packet(&mut self) -> Result<Option<&[f32]>> {
        let channels = self.info.channels as u64;

        // Calculate the valid frame range (excluding delay and padding)
        let valid_start = self.delay_samples;
        let valid_end = self
            .total_samples
            .map(|total| total.saturating_sub(self.padding_samples))
            .unwrap_or(u64::MAX);

        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymphError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(SymphError::DecodeError(_)) => continue,
                Err(SymphError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(e) => return Err(VoxError::Decoder(e.to_string())),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(SymphError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(SymphError::DecodeError(_)) => continue,
                Err(SymphError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(e) => return Err(VoxError::Decoder(e.to_string())),
            };

            let num_frames = decoded.frames() as u64;

            // Calculate the frame range within the track for this packet
            let packet_start_frame = self.samples_decoded;
            let packet_end_frame = packet_start_frame + num_frames;

            // Update total frames decoded
            self.samples_decoded = packet_end_frame;

            // Check if this packet is entirely outside the valid range
            if packet_end_frame <= valid_start || packet_start_frame >= valid_end {
                // Entire packet is delay or padding, skip it
                continue;
            }

            // Calculate the slice bounds within this packet's samples
            let skip_start_frames = valid_start.saturating_sub(packet_start_frame);
            let skip_end_frames = packet_end_frame.saturating_sub(valid_end);

            let buf = self.sample_buf.get_or_insert_with(|| {
                SampleBuffer::new(decoded.capacity() as u64, *decoded.spec())
            });

            buf.copy_interleaved_ref(decoded);

            let samples = buf.samples();
            let start_idx = (skip_start_frames * channels) as usize;
            let end_idx = samples.len() - (skip_end_frames * channels) as usize;

            return Ok(Some(&samples[start_idx..end_idx]));
        }
    }

    /// Returns the playable duration in seconds, excluding encoder delay and padding.
    /// Returns None if total_samples is unknown.
    pub fn playable_duration(&self) -> Option<f64> {
        let total = self.total_samples?;
        let playable_frames = total.saturating_sub(self.delay_samples + self.padding_samples);
        Some(playable_frames as f64 / self.info.sample_rate as f64)
    }

    pub fn seek(&mut self, secs: f64) -> Result<u64> {
        let time = Time {
            seconds: secs as u64,
            frac: secs.fract(),
        };

        let seeked = self
            .format
            .seek(
                SeekMode::Coarse,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| VoxError::Seek(e.to_string()))?;

        self.decoder.reset();
        self.sample_buf = None;
        // Update samples_decoded to match the seek position (actual_ts is in time base units)
        self.samples_decoded = seeked.actual_ts;

        Ok(seeked.actual_ts)
    }
}

fn open_format_reader(p: &str) -> Result<Box<dyn FormatReader>> {
    let path = PathBuf::from(&p);
    let file = File::open(&path).map_err(|_| VoxError::FileOpen(p.to_string()))?;

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    };

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let format_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };

    let probed = get_probe()
        .format(&hint, mss, &format_opts, &MetadataOptions::default())
        .map_err(|e| VoxError::Decoder(e.to_string()))?;

    Ok(probed.format)
}
