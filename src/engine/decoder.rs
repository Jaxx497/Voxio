use std::{
    fs::File,
    io::{self},
    path::PathBuf,
    sync::LazyLock,
};
use symphonia::{
    core::{
        audio::{Channels, Position},
        codecs::{
            audio::{
                AudioDecoder as SymphoniaDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO,
                well_known::CODEC_ID_OPUS,
            },
            registry::CodecRegistry,
        },
        errors::Error as SymphError,
        formats::{FormatOptions, FormatReader, SeekMode, SeekTo, probe::Hint},
        io::MediaSourceStream,
        meta::MetadataOptions,
        units::{Time, TimeBase},
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
    registry.register_audio_decoder::<OpusDecoder>();
    registry
});

#[derive(Debug, Clone)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: usize,
    pub n_frames: Option<u64>,
}

pub(crate) struct VoxDecoder {
    path: String,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn SymphoniaDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    sample_buf: Vec<f32>,
    pub info: AudioInfo,
}

impl VoxDecoder {
    pub fn open<S: AsRef<str>>(path: S) -> Result<Self> {
        let path = path.as_ref();

        let mut format = open_format_reader(&path)
            .map_err(|e| VoxError::Decoder(format!("Format reader error: {}", e)))?;

        let track = format
            .tracks()
            .iter()
            .find(|t| {
                t.codec_params
                    .as_ref()
                    .and_then(|cp| cp.audio())
                    .is_some_and(|a| a.codec != CODEC_ID_NULL_AUDIO)
            })
            .ok_or(VoxError::Decoder("No track!".into()))?;

        let track_id = track.id;
        let n_frames = track.num_frames;
        let time_base = track.time_base;

        let audio_params = track
            .codec_params
            .as_ref()
            .and_then(|cp| cp.audio())
            .ok_or(VoxError::Decoder("No audio codec params".into()))?;

        let metadata_sample_rate = audio_params.sample_rate;
        let metadata_channels = audio_params.channels.as_ref().map(|c| c.count());
        let metadata_complete = metadata_sample_rate.is_some() && metadata_channels.is_some();

        // For Opus in MKV/WebM, channel info may be missing. Default to stereo.
        let decoder_params =
            if audio_params.codec == CODEC_ID_OPUS && audio_params.channels.is_none() {
                let mut params = audio_params.clone();
                params.channels = Some(Channels::Positioned(
                    Position::FRONT_LEFT | Position::FRONT_RIGHT,
                ));
                params
            } else {
                audio_params.clone()
            };

        let mut decoder = CODEC_REGISTRY
            .make_audio_decoder(&decoder_params, &AudioDecoderOptions::default())
            .map_err(|e| VoxError::Decoder(e.to_string()))?;

        let (sample_rate, channels) = if metadata_complete {
            (metadata_sample_rate.unwrap(), metadata_channels.unwrap())
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
                    Ok(decoded) => {
                        let spec = decoded.spec();
                        found_spec = Some((spec.rate(), spec.channels().count()));
                        break;
                    }
                    Err(SymphError::DecodeError(_)) => continue,
                    Err(_) => break,
                }
            }

            let _ = format.seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::ZERO,
                    track_id: Some(track_id),
                },
            );
            decoder.reset();

            found_spec.ok_or_else(|| {
                VoxError::Decoder("Could not determine sample rate and channels".into())
            })?
        };

        Ok(VoxDecoder {
            path: path.to_string(),
            format,
            decoder,
            track_id,
            time_base,
            sample_buf: Vec::new(),
            info: AudioInfo {
                sample_rate,
                channels,
                n_frames,
            },
        })
    }

    pub fn next_packet(&mut self) -> Result<Option<&[f32]>> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                Err(SymphError::DecodeError(_)) => continue,
                Err(SymphError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(e) => return Err(VoxError::Decoder(e.to_string())),
            };

            if packet.track_id != self.track_id {
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

            if decoded.frames() == 0 {
                continue;
            }

            decoded.copy_to_vec_interleaved::<f32>(&mut self.sample_buf);

            return Ok(Some(&self.sample_buf));
        }
    }

    pub fn playable_duration(&self) -> Option<f64> {
        let frames = self.info.n_frames?;
        Some(frames as f64 / self.info.sample_rate as f64)
    }

    pub fn seek(&mut self, secs: f64) -> Result<f64> {
        let time = Time::try_from_secs_f64(secs)
            .ok_or_else(|| VoxError::Seek("Invalid seek time".into()))?;

        let track_id = self.track_id;
        let make_seek_to = || SeekTo::Time {
            time,
            track_id: Some(track_id),
        };

        let seeked = match self.format.seek(SeekMode::Coarse, make_seek_to()) {
            Ok(s) => s,
            Err(_) => {
                // Seek failed — likely a backward seek in a container without cue points.
                // Re-open the format reader to reset to byte 0, then seek forward.
                self.reopen()?;
                self.format
                    .seek(SeekMode::Coarse, make_seek_to())
                    .map_err(|e| VoxError::Seek(e.to_string()))?
            }
        };

        self.decoder.reset();
        self.sample_buf.clear();

        let actual_secs = match self.time_base {
            Some(tb) => tb
                .calc_time(seeked.actual_ts)
                .map(|t| t.as_secs_f64())
                .unwrap_or(secs),
            None => seeked.actual_ts.get() as f64 / self.info.sample_rate as f64,
        };

        Ok(actual_secs)
    }

    fn reopen(&mut self) -> Result<()> {
        let format = open_format_reader(&self.path)
            .map_err(|e| VoxError::Seek(format!("Failed to reopen for seek: {}", e)))?;

        let track = format
            .tracks()
            .iter()
            .find(|t| t.id == self.track_id)
            .ok_or(VoxError::Seek("Track not found on reopen".into()))?;

        let audio_params = track
            .codec_params
            .as_ref()
            .and_then(|cp| cp.audio())
            .ok_or(VoxError::Seek("No audio codec params on reopen".into()))?;

        let decoder_params =
            if audio_params.codec == CODEC_ID_OPUS && audio_params.channels.is_none() {
                let mut params = audio_params.clone();
                params.channels = Some(Channels::Positioned(
                    Position::FRONT_LEFT | Position::FRONT_RIGHT,
                ));
                params
            } else {
                audio_params.clone()
            };

        let decoder = CODEC_REGISTRY
            .make_audio_decoder(&decoder_params, &AudioDecoderOptions::default())
            .map_err(|e| VoxError::Seek(e.to_string()))?;

        self.format = format;
        self.decoder = decoder;

        Ok(())
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
