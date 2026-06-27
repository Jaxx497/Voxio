use crate::error::VoxError;
use rubato::{Fft, FixedSync, Resampler, audioadapter_buffers::direct::InterleavedSlice};

const RESAMPLER_CHUNK_SIZE: usize = 1024;
const RESAMPLER_SUBCHUNK_SIZE: usize = 2;

/// Wrapper around Rubato's FFT resampler. Returns None when no conversion is needed.
pub(crate) struct VoxResampler {
    resampler: Fft<f32>,
    output_buf: Vec<f32>,
    input_rate: u32,
    channels: usize,
}

impl VoxResampler {
    pub fn new(
        input_rate: u32,
        output_rate: u32,
        channels: usize,
    ) -> Result<Option<Self>, VoxError> {
        if input_rate == output_rate {
            return Ok(None);
        }

        let resampler = Fft::<f32>::new(
            input_rate as usize,
            output_rate as usize,
            RESAMPLER_CHUNK_SIZE,
            RESAMPLER_SUBCHUNK_SIZE,
            channels,
            FixedSync::Input,
        )
        .map_err(|e| VoxError::Resampler(e.to_string()))?;

        let output_buf = vec![0.0f32; resampler.output_frames_max() * channels];

        Ok(Some(Self {
            resampler,
            output_buf,
            input_rate,
            channels,
        }))
    }

    /// Feed interleaved input. Converted blocks are handed to `output`.
    pub fn process<F>(&mut self, pending: &mut Vec<f32>, mut output: F) -> Result<(), VoxError>
    where
        F: FnMut(&[f32]),
    {
        let frames_needed = self.resampler.input_frames_next();
        let samples_needed = frames_needed * self.channels;

        while pending.len() >= samples_needed {
            let input =
                InterleavedSlice::new(&pending[..samples_needed], self.channels, frames_needed)
                    .map_err(|e| VoxError::Resampler(e.to_string()))?;

            let out_frames_max = self.resampler.output_frames_max();
            let mut out =
                InterleavedSlice::new_mut(&mut self.output_buf, self.channels, out_frames_max)
                    .map_err(|e| VoxError::Resampler(e.to_string()))?;

            let (_, out_frames) = self
                .resampler
                .process_into_buffer(&input, &mut out, None)
                .map_err(|e| VoxError::Resampler(e.to_string()))?;

            pending.drain(..samples_needed);
            output(&self.output_buf[..out_frames * self.channels]);
        }

        Ok(())
    }

    /// Flush a trailing partial block at EOF. Pads to a full chunk so `process`
    /// consumes it, but emits only the frames matching real input (≈ ratio ×
    /// leftover) so no resampled silence leaks out — the gap that hit rate-changed
    /// gapless transitions. The leftover is always < one chunk (see `process`).
    pub fn flush<F>(&mut self, pending: &mut Vec<f32>, mut output: F) -> Result<(), VoxError>
    where
        F: FnMut(&[f32]),
    {
        if pending.is_empty() {
            return Ok(());
        }

        // Real audio frames present *before* padding — bounds what we emit.
        let real_frames = pending.len() / self.channels;
        let expected_frames =
            (self.resampler.resample_ratio() * real_frames as f64).ceil() as usize;

        // Pad to a full chunk so process() actually consumes the partial tail.
        let samples_needed = self.resampler.input_frames_next() * self.channels;
        pending.resize(samples_needed, 0.0);

        let channels = self.channels;
        let mut emitted = 0usize;
        self.process(pending, |block| {
            if emitted >= expected_frames {
                return; // remaining output is resampled silence — drop it
            }
            let avail = block.len() / channels;
            let take = avail.min(expected_frames - emitted);
            if take > 0 {
                output(&block[..take * channels]);
                emitted += take;
            }
        })
    }

    /// Clear internal state after a discontinuity (e.g. a seek).
    pub fn reset(&mut self) {
        self.resampler.reset();
    }

    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interleaved input: a small repeating ramp, enough to count frames out.
    fn ramp(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels)
            .map(|i| (i % 100) as f32 * 0.01)
            .collect()
    }

    #[test]
    fn none_when_rates_match() {
        assert!(VoxResampler::new(44100, 44100, 2).unwrap().is_none());
    }

    #[test]
    fn some_when_rates_differ() {
        assert!(VoxResampler::new(44100, 48000, 2).unwrap().is_some());
    }

    #[test]
    fn input_rate_getter() {
        let rs = VoxResampler::new(44100, 48000, 2).unwrap().unwrap();
        assert_eq!(rs.input_rate(), 44100);
    }

    #[test]
    fn upsample_produces_more_frames() {
        let mut rs = VoxResampler::new(44100, 48000, 2).unwrap().unwrap();
        let (channels, in_frames) = (2, 44100); // one second
        let mut pending = ramp(in_frames, channels);

        let mut out_samples = 0usize;
        rs.process(&mut pending, |b| out_samples += b.len())
            .unwrap();

        let out_frames = out_samples / channels;
        let expected = in_frames as f64 * 48000.0 / 44100.0;
        // FFT latency eats a little; stay within a loose band.
        assert!(
            (out_frames as f64) > expected * 0.9 && (out_frames as f64) < expected * 1.1,
            "got {out_frames} frames, expected ~{expected}"
        );
        // Leftover is always less than one input chunk.
        assert!(pending.len() < RESAMPLER_CHUNK_SIZE * channels);
    }

    #[test]
    fn downsample_produces_fewer_frames() {
        let mut rs = VoxResampler::new(48000, 44100, 2).unwrap().unwrap();
        let (channels, in_frames) = (2, 48000);
        let mut pending = ramp(in_frames, channels);

        let mut out_samples = 0usize;
        rs.process(&mut pending, |b| out_samples += b.len())
            .unwrap();

        let out_frames = out_samples / channels;
        let expected = in_frames as f64 * 44100.0 / 48000.0;
        assert!(
            (out_frames as f64) > expected * 0.9 && (out_frames as f64) < expected * 1.1,
            "got {out_frames} frames, expected ~{expected}"
        );
    }

    #[test]
    fn process_leaves_partial_chunk_untouched() {
        let mut rs = VoxResampler::new(44100, 48000, 2).unwrap().unwrap();
        // Fewer samples than one chunk → nothing consumed or emitted.
        let mut pending = ramp(100, 2);
        let mut out_samples = 0usize;
        rs.process(&mut pending, |b| out_samples += b.len())
            .unwrap();
        assert_eq!(out_samples, 0);
        assert_eq!(pending.len(), 100 * 2);
    }

    #[test]
    fn flush_empty_is_noop() {
        let mut rs = VoxResampler::new(44100, 48000, 2).unwrap().unwrap();
        let mut pending = Vec::new();
        let mut out_samples = 0usize;
        rs.flush(&mut pending, |b| out_samples += b.len()).unwrap();
        assert_eq!(out_samples, 0);
    }

    #[test]
    fn mono_channel_upsamples() {
        let mut rs = VoxResampler::new(22050, 44100, 1).unwrap().unwrap();
        let mut pending = ramp(22050, 1);
        let mut out_samples = 0usize;
        rs.process(&mut pending, |b| out_samples += b.len())
            .unwrap();
        assert!(
            out_samples > 22050,
            "2x upsample should roughly double frames"
        );
    }

    #[test]
    fn flush_emits_only_real_tail() {
        let mut rs = VoxResampler::new(44100, 48000, 2).unwrap().unwrap();
        let mut pending = ramp(100, 2); // 100 real frames in
        let mut out_frames = 0usize;
        rs.flush(&mut pending, |b| out_frames += b.len() / 2)
            .unwrap();

        let expected = (100.0 * 48000.0 / 44100.0_f64).ceil() as usize; // ≈ 109
        assert_eq!(
            out_frames, expected,
            "must emit real tail, not a padded chunk"
        );
        assert!(pending.is_empty());
    }
}
