use std::time::Duration;

const BUFFER_MS: usize = 150;
const SAMPLE_TAP_CAPACITY: usize = 4096;
const WATCHDOG_TICK_DURATION: Duration = Duration::from_millis(200);
const ZOMBIE_TICKS: u32 = 3;

/// Tuning knobs for the engine, passed to [`Vox::new_with_config`].
///
/// Use [`VoxConfig::default`] for sensible defaults and override only the
/// fields you care about.
///
/// [`Vox::new_with_config`]: crate::Vox::new_with_config
pub struct VoxConfig {
    /// Output ring-buffer size, in milliseconds of audio. Larger values are
    /// more robust to scheduling jitter; smaller values reduce latency.
    ///
    /// Default: 150
    pub buffer_ms: usize,
    /// Capacity, in samples, of the visualization tap's ring buffer.
    ///
    /// Default: 4096
    pub tap_capacity: usize,
    /// How often the watchdog polls for device loss and stream stalls.
    ///
    /// Default: Duration::from_millis(200)
    pub watchdog_tick: Duration,
    /// Number of consecutive idle watchdog ticks before the stream is treated
    /// as a zombie and rebuilt.
    ///
    /// Default: 3
    pub zombie_ticks: u32,
}

impl Default for VoxConfig {
    fn default() -> Self {
        VoxConfig {
            buffer_ms: BUFFER_MS,
            tap_capacity: SAMPLE_TAP_CAPACITY,
            watchdog_tick: WATCHDOG_TICK_DURATION,
            zombie_ticks: ZOMBIE_TICKS,
        }
    }
}
