//! Offline waveform extraction: decode a whole file into per-bucket amplitudes
//! for visualization. Runs on the caller's thread — `Vox` not needed — so
//! generation and caching live wherever you like. Raw RMS by default; any EQ,
//! normalization, or smoothing is a display choice left to the caller.

use crate::{
    decoder::VoxDecoder,
    error::{Result, VoxError},
};
use std::time::Duration;

/// Frames per fine accumulator before downsampling. Keeps memory bounded
/// regardless of file length (~a few MB worst-case).
const FINE_FRAMES: u64 = 512;

/// Centre of the treble high-shelf. Gain above this lifts transient detail
/// (hats, snares) that drives the "spiky" look.
const TREBLE_HZ: f32 = 6000.0;

/// How each output bucket summarizes its samples.
#[derive(Debug, Copy, Clone, Default)]
pub enum BinMetric {
    /// Root-mean-square — smooth envelope tracking average energy.
    #[default]
    Rms,
    /// Peak absolute amplitude — spikier, transient-driven.
    Peak,
}

/// Waveform extraction options. Defaults to raw RMS with no shaping. All
/// filtering runs per-sample in the decode loop — it can't be reconstructed from
/// the bins afterward. Amplitude-only shaping (contrast, smoothing, dB curve) is
/// left to the caller.
#[derive(Debug)]
pub struct WaveformOptions {
    /// Number of output buckets.
    pub bins: usize,
    /// How each bucket is summarized. See [`BinMetric`].
    pub metric: BinMetric,
    /// Optional high-pass cutoff in Hz. Rolls off steady bass that flattens the
    /// waveform. `None` is raw. ~150–400 Hz is a useful range.
    pub highpass_hz: Option<f32>,
    /// Treble high-shelf gain in dB above ~6 kHz. Emphasizes high-frequency
    /// transients. `0.0` is flat; `0.0..=18.0` is useful.
    pub treble_db: f32,
}

impl Default for WaveformOptions {
    fn default() -> Self {
        WaveformOptions {
            bins: 500,
            metric: BinMetric::Rms,
            highpass_hz: None,
            treble_db: 0.0,
        }
    }
}

/// Generated waveform with the span of audio it covers.
#[derive(Debug, Clone)]
pub struct Waveform {
    /// Per-bucket amplitudes. Length = requested `bins`, values typically `0.0..=1.0`.
    pub bins: Vec<f32>,
    /// Sample-accurate duration from decoded frames. Unlike a container's reported
    /// duration this needs no header. On a partial decode it covers only what
    /// actually rendered, so `bins` and `duration` stay aligned.
    pub duration: Duration,
}

impl Waveform {
    /// Decode `path` in full and reduce it to `opts.bins` per-bucket amplitudes
    /// plus the sample-accurate [`duration`](Waveform::duration) it covers.
    /// Channels are downmixed to mono; pass [`WaveformOptions::default`] for
    /// unshaped RMS.
    ///
    /// Uses the same decoder as playback, so the result lines up with what plays.
    /// Runs synchronously and decodes the whole file — call it off the hot path
    /// (e.g. a worker thread).
    ///
    /// Mirrors playback's tolerance: a mid-stream decode error keeps whatever
    /// decoded, so a damaged file still yields bins and duration for the
    /// renderable prefix. `Err` only when nothing usable decodes.
    pub fn generate(path: &str, opts: &WaveformOptions) -> Result<Waveform> {
        if opts.bins == 0 {
            return Ok(Waveform {
                bins: Vec::new(),
                duration: Duration::ZERO,
            });
        }

        let mut decoder = VoxDecoder::open(path)?;
        let channels = decoder.info.channels.max(1);
        let sample_rate = decoder.info.sample_rate;
        let mut hp = opts
            .highpass_hz
            .map(|fc| OnePoleHighpass::new(fc, sample_rate));
        let mut treble = (opts.treble_db.abs() > 0.01)
            .then(|| Biquad::high_shelf(TREBLE_HZ, opts.treble_db, sample_rate));

        // Fold into fixed-size buckets so memory stays bounded regardless of length.
        let metric = opts.metric;
        let mut fine: Vec<(f64, u64)> = Vec::new();
        let mut acc = 0.0f64;
        let mut count = 0u64;

        loop {
            let buf = match decoder.next_packet() {
                Ok(Some(buf)) => buf,
                Ok(None) => break,
                // Mid-stream error: keep the prefix, like playback does.
                Err(e) => {
                    if fine.is_empty() && count == 0 {
                        return Err(e);
                    }
                    break;
                }
            };
            for frame in buf.chunks_exact(channels) {
                let mut mono = (frame.iter().sum::<f32>() / channels as f32) as f64;
                if let Some(hp) = hp.as_mut() {
                    mono = hp.step(mono);
                }
                if let Some(treble) = treble.as_mut() {
                    mono = treble.step(mono);
                }
                match metric {
                    BinMetric::Rms => acc += mono * mono,
                    BinMetric::Peak => acc = acc.max(mono.abs()),
                }
                count += 1;
                if count == FINE_FRAMES {
                    fine.push((acc, count));
                    acc = 0.0;
                    count = 0;
                }
            }
        }
        if count > 0 {
            fine.push((acc, count));
        }

        if fine.is_empty() {
            return Err(VoxError::Decoder("no decodable audio in stream".into()));
        }

        let frames: u64 = fine.iter().map(|&(_, c)| c).sum();
        Ok(Waveform {
            bins: downsample(&fine, opts.bins, metric),
            duration: Duration::from_secs_f64(frames as f64 / sample_rate.max(1) as f64),
        })
    }
}

/// Flat waveforms render at this height (mid-pane) instead of collapsing to a line.
const FLAT_LEVEL: f32 = 0.3;
/// In [`Waveform::local_normalize`], local maxima below this fraction of the global
/// peak are floored, so silent gaps aren't amplified to full scale.
const LOCAL_FLOOR: f32 = 0.08;

/// Amplitude-only display shaping for the finished [`bins`](Waveform::bins).
///
/// [`generate`](Waveform::generate) returns the raw signal; these are optional
/// polish that operate purely on the output array — no audio — so they can be
/// composed, reordered, and re-run live (e.g. on cached bins as a viewer's controls
/// change). Each mutates in place and returns `&mut self`, so a typical chain reads
/// `wf.local_normalize(..).normalize().contrast(..)`.
impl Waveform {
    /// Scale [`bins`](Waveform::bins) to `0.0..=1.0` by global min/max so they fill
    /// the height. A flat signal (no range) is set to a mid-level so it reads as a
    /// line rather than nothing.
    pub fn normalize(&mut self) -> &mut Self {
        if self.bins.is_empty() {
            return self;
        }
        let min = self.bins.iter().copied().fold(f32::INFINITY, f32::min);
        let max = self.bins.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let range = max - min;
        if range < f32::EPSILON {
            self.bins.iter_mut().for_each(|v| *v = FLAT_LEVEL);
        } else {
            self.bins.iter_mut().for_each(|v| *v = (*v - min) / range);
        }
        self
    }

    /// Apply a contrast curve `vᵞ` to a normalized (`0.0..=1.0`) envelope. `gamma > 1`
    /// pushes the body down while peaks stay put, exaggerating the peaks-and-valleys;
    /// `gamma == 1` is a no-op; `0 < gamma < 1` lifts quiet detail. Call
    /// [`normalize`](Waveform::normalize) again afterward only to refill the height.
    pub fn contrast(&mut self, gamma: f32) -> &mut Self {
        if (gamma - 1.0).abs() >= f32::EPSILON {
            let gamma = gamma.max(0.0);
            self.bins
                .iter_mut()
                .for_each(|v| *v = v.max(0.0).powf(gamma));
        }
        self
    }

    /// Local (windowed) normalization — the dynaudnorm equivalent. Divide each bin
    /// by a smoothed local maximum over ±`half_window` bins, blended with the global
    /// max by `strength` (`0.0` = pure global / unchanged, `1.0` = pure local). Lifts
    /// quiet sections against their own neighbourhood while leaving uniformly-loud
    /// material alone (its local max already ≈ the global max). Run before
    /// [`normalize`](Waveform::normalize).
    pub fn local_normalize(&mut self, strength: f32, half_window: usize) -> &mut Self {
        let strength = strength.clamp(0.0, 1.0);
        if self.bins.is_empty() || strength == 0.0 {
            return self;
        }
        let global_max = self.bins.iter().copied().fold(0.0f32, f32::max);
        if global_max <= f32::EPSILON {
            return self;
        }
        let floor = global_max * LOCAL_FLOOR;
        let n = self.bins.len();
        let mut local = vec![0.0f32; n];
        for (i, slot) in local.iter_mut().enumerate() {
            let lo = i.saturating_sub(half_window);
            let hi = (i + half_window + 1).min(n);
            *slot = self.bins[lo..hi]
                .iter()
                .copied()
                .fold(0.0f32, f32::max)
                .max(floor);
        }
        // Smooth the divisor envelope so it doesn't band the output bin-to-bin.
        smooth(&mut local, half_window / 2);
        for (v, &l) in self.bins.iter_mut().zip(local.iter()) {
            *v /= global_max * (1.0 - strength) + l * strength;
        }
        self
    }
}

/// In-place moving-average over ±`radius` samples.
fn smooth(v: &mut [f32], radius: usize) {
    if radius == 0 || v.len() < 2 {
        return;
    }
    let src = v.to_vec();
    let n = src.len();
    for (i, out) in v.iter_mut().enumerate() {
        let lo = i.saturating_sub(radius);
        let hi = (i + radius + 1).min(n);
        *out = src[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
    }
}

/// First-order high-pass: `y[n] = a * (y[n-1] + x[n] - x[n-1])`.
struct OnePoleHighpass {
    a: f64,
    prev_x: f64,
    prev_y: f64,
}

impl OnePoleHighpass {
    fn new(cutoff_hz: f32, sample_rate: u32) -> Self {
        let rc = 1.0 / (2.0 * std::f64::consts::PI * cutoff_hz.max(1.0) as f64);
        let dt = 1.0 / sample_rate.max(1) as f64;
        OnePoleHighpass {
            a: rc / (rc + dt),
            prev_x: 0.0,
            prev_y: 0.0,
        }
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.a * (self.prev_y + x - self.prev_x);
        self.prev_x = x;
        self.prev_y = y;
        y
    }
}

/// Direct-form-I biquad, RBJ high-shelf.
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn high_shelf(fc: f32, gain_db: f32, sample_rate: u32) -> Self {
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * fc.max(1.0) as f64 / sample_rate.max(1) as f64;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / 2.0 * std::f64::consts::SQRT_2;
        let sqrt_a = a.sqrt();

        let b0 = a * ((a + 1.0) + (a - 1.0) * cos + 2.0 * sqrt_a * alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos - 2.0 * sqrt_a * alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cos + 2.0 * sqrt_a * alpha;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
        let a2 = (a + 1.0) - (a - 1.0) * cos - 2.0 * sqrt_a * alpha;

        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Merge fine buckets into exactly `bins` values. Proportional slices; repeats
/// (upsamples) when fine buckets are fewer than bins.
fn downsample(fine: &[(f64, u64)], bins: usize, metric: BinMetric) -> Vec<f32> {
    let m = fine.len();
    (0..bins)
        .map(|j| {
            let lo = j * m / bins;
            let hi = ((j + 1) * m / bins).max(lo + 1).min(m);
            match metric {
                // Pool summed squares and counts → RMS.
                BinMetric::Rms => {
                    let (mut ss, mut n) = (0.0f64, 0u64);
                    for &(s, c) in &fine[lo..hi] {
                        ss += s;
                        n += c;
                    }
                    if n > 0 {
                        (ss / n as f64).sqrt() as f32
                    } else {
                        0.0
                    }
                }
                // Largest bucket peak.
                BinMetric::Peak => fine[lo..hi].iter().map(|&(p, _)| p).fold(0.0, f64::max) as f32,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINE: &str = "tests/test_suite/sine.flac";

    /// Default-shaped bins at the requested count — the common case below.
    fn bins(path: &str, n: usize) -> Result<Vec<f32>> {
        Ok(Waveform::generate(
            path,
            &WaveformOptions {
                bins: n,
                ..Default::default()
            },
        )?
        .bins)
    }

    #[test]
    fn produces_requested_bin_count() {
        let wf = bins(SINE, 500).unwrap();
        assert_eq!(wf.len(), 500);
    }

    #[test]
    fn sine_has_steady_nonzero_amplitude() {
        let wf = bins(SINE, 200).unwrap();
        assert!(wf.iter().all(|&v| (0.0..=1.0).contains(&v)), "out of range");
        let avg = wf.iter().sum::<f32>() / wf.len() as f32;
        assert!(avg > 0.05, "expected audible RMS, got avg {avg}");
        assert!(
            wf.iter().all(|&v| (v - avg).abs() < 0.2),
            "a steady sine should not vary wildly across bins"
        );
    }

    #[test]
    fn zero_bins_is_empty() {
        assert!(bins(SINE, 0).unwrap().is_empty());
    }

    #[test]
    fn reports_covered_duration() {
        let wf = Waveform::generate(SINE, &WaveformOptions::default()).unwrap();
        let secs = wf.duration.as_secs_f64();
        assert!((secs - 2.0).abs() < 0.1, "expected ~2 s, got {secs}");
    }

    #[test]
    fn peak_exceeds_rms_for_a_sine() {
        let opts = |m| WaveformOptions {
            bins: 200,
            metric: m,
            ..Default::default()
        };
        let rms = Waveform::generate(SINE, &opts(BinMetric::Rms))
            .unwrap()
            .bins;
        let peak = Waveform::generate(SINE, &opts(BinMetric::Peak))
            .unwrap()
            .bins;
        let avg = |w: &[f32]| w.iter().sum::<f32>() / w.len() as f32;
        assert!(
            avg(&peak) > avg(&rms) * 1.3,
            "peak {} should exceed rms {} (~1.41x for a sine)",
            avg(&peak),
            avg(&rms)
        );
    }

    /// RMS gain a filter applies to a steady sine at `freq`, past the start-up transient.
    fn sine_gain(freq: f64, sr: u32, mut step: impl FnMut(f64) -> f64) -> f64 {
        let n = 16384;
        let inp: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / sr as f64).sin())
            .collect();
        let out: Vec<f64> = inp.iter().map(|&x| step(x)).collect();
        let skip = n / 2;
        let rms = |v: &[f64]| (v[skip..].iter().map(|x| x * x).sum::<f64>()).sqrt();
        rms(&out) / rms(&inp)
    }

    #[test]
    fn highpass_rolls_off_bass_passes_highs() {
        let sr = 44100;
        let mut lo = OnePoleHighpass::new(300.0, sr);
        let g_lo = sine_gain(60.0, sr, |x| lo.step(x));
        let mut hi = OnePoleHighpass::new(300.0, sr);
        let g_hi = sine_gain(6000.0, sr, |x| hi.step(x));
        assert!(g_lo < 0.4, "60 Hz should be cut, gain {g_lo}");
        assert!(g_hi > 0.85, "6 kHz should pass, gain {g_hi}");
    }

    #[test]
    fn treble_shelf_lifts_highs_leaves_lows() {
        let sr = 44100;
        let mut lo = Biquad::high_shelf(6000.0, 12.0, sr);
        let g_lo = sine_gain(150.0, sr, |x| lo.step(x));
        let mut hi = Biquad::high_shelf(6000.0, 12.0, sr);
        let g_hi = sine_gain(13000.0, sr, |x| hi.step(x));
        // +12 dB high-shelf ≈ 3.98x above the corner, ~unity well below it.
        assert!(
            (0.9..1.15).contains(&g_lo),
            "150 Hz should be ~flat, gain {g_lo}"
        );
        assert!(g_hi > 3.5, "13 kHz should be boosted ~+12 dB, gain {g_hi}");
    }

    #[test]
    fn undecodable_file_errors() {
        let err = bins("tests/test_suite/invalid_filetype.txt", 100);
        assert!(matches!(
            err,
            Err(VoxError::Decoder(_) | VoxError::FileOpen(_))
        ));
    }

    /// Build a bins-only `Waveform` for exercising the shaping methods.
    fn wf(bins: Vec<f32>) -> Waveform {
        Waveform {
            bins,
            duration: Duration::ZERO,
        }
    }

    #[test]
    fn normalize_maps_to_unit_range() {
        let mut w = wf(vec![2.0, 4.0, 6.0]);
        w.normalize();
        assert_eq!(w.bins, [0.0, 0.5, 1.0]);
    }

    #[test]
    fn normalize_flat_shows_a_line() {
        let mut w = wf(vec![0.7, 0.7, 0.7]);
        w.normalize();
        assert!(w.bins.iter().all(|&v| v == FLAT_LEVEL));
    }

    #[test]
    fn contrast_lowers_body_keeps_peaks() {
        let mut w = wf(vec![0.0, 0.5, 1.0]);
        w.contrast(2.0);
        assert_eq!(w.bins, [0.0, 0.25, 1.0]); // peaks at 1 stay, mid drops
    }

    #[test]
    fn local_normalize_lifts_a_quiet_section() {
        // Loud first half, quiet (0.1x) second half.
        let mut w = wf((0..200).map(|i| if i < 100 { 0.8 } else { 0.08 }).collect());
        w.local_normalize(1.0, 12).normalize();
        assert!(
            w.bins[150] > 0.5,
            "quiet section should lift, got {}",
            w.bins[150]
        );
    }

    #[test]
    fn local_normalize_leaves_uniform_loud_alone() {
        let mut w = wf(vec![0.8f32; 200]);
        w.local_normalize(1.0, 12);
        // Every divisor ≈ the global max, so the signal is unchanged.
        assert!(w.bins.iter().all(|&v| (v - 1.0).abs() < 1e-3));
    }

    #[test]
    fn shaping_chains_and_stays_in_range() {
        let mut w = wf(vec![0.2, 0.5, 1.0, 0.3]);
        w.local_normalize(0.5, 2).normalize().contrast(1.5);
        assert!(w.bins.iter().all(|&v| (0.0..=1.0).contains(&v)));
    }
}
