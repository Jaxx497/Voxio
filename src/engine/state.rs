use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct SharedState {
    active: AtomicBool,
    paused: AtomicBool,
    samples_played: AtomicU64,
    track_ended: AtomicBool,
    seek_pending: AtomicBool,
    seek_generation: AtomicU64,
    duration_micros: AtomicU64,
}

impl Default for SharedState {
    fn default() -> Self {
        SharedState {
            active: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            samples_played: AtomicU64::new(0),
            track_ended: AtomicBool::new(false),
            seek_pending: AtomicBool::new(false),
            seek_generation: AtomicU64::new(0),
            duration_micros: AtomicU64::new(0),
        }
    }
}

impl SharedState {
    // ===================
    //      Read State
    // ===================

    /// Is a track loaded (playing or paused)?
    /// returns bool
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Is the playback paused?
    /// returns bool
    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Returns number of samples played so far
    pub(crate) fn get_samples(&self) -> u64 {
        self.samples_played.load(Ordering::Acquire)
    }

    /// Is a seek operation in progress?
    pub(crate) fn is_seeking(&self) -> bool {
        self.seek_pending.load(Ordering::Acquire)
    }

    /// Get the current seek generation counter
    pub(crate) fn seek_generation(&self) -> u64 {
        self.seek_generation.load(Ordering::Acquire)
    }

    /// Returns the playable duration in seconds
    pub(crate) fn get_duration_secs(&self) -> f64 {
        self.duration_micros.load(Ordering::Acquire) as f64 / 1_000_000.0
    }

    // ======================
    //      Write State
    // =====================

    /// Set status of player activity
    ///
    /// `true`   => Something is loaded
    /// `false`  => Player is empty
    pub(crate) fn set_active(&self, val: bool) {
        self.active.store(val, Ordering::Release);
    }

    /// Set paused status of player
    pub(crate) fn set_paused(&self, val: bool) {
        if val && !self.is_active() {
            return;
        }
        self.paused.store(val, Ordering::Relaxed);
    }

    /// Toggle paused field
    pub(crate) fn toggle_playback(&self) {
        if self.is_active() {
            self.paused.fetch_xor(true, Ordering::Relaxed);
        } else {
            self.paused.store(false, Ordering::Relaxed)
        }
    }

    pub(crate) fn set_samples(&self, samples: u64) {
        self.samples_played.store(samples, Ordering::SeqCst);
    }

    /// Add samples to existing value
    pub(crate) fn add_samples(&self, val: u64) {
        self.samples_played.fetch_add(val, Ordering::Release);
    }

    /// Clear all samples
    pub(crate) fn reset_samples(&self) {
        self.samples_played.store(0, Ordering::Release);
    }

    /// Set the playable duration in seconds
    pub(crate) fn set_duration_secs(&self, secs: f64) {
        let micros = (secs * 1_000_000.0) as u64;
        self.duration_micros.store(micros, Ordering::Release);
    }

    pub(crate) fn signal_track_ended(&self) {
        self.track_ended.store(true, Ordering::Release);
    }

    pub(crate) fn take_track_ended(&self) -> bool {
        self.track_ended.swap(false, Ordering::Acquire)
    }

    // ======================
    //      Seek Control
    // ======================

    /// Signal that a seek operation has started.
    /// Sets pending flag first, then increments generation.
    /// This ordering ensures the decoder thread pauses before the audio callback drains.
    pub(crate) fn start_seek(&self) {
        self.seek_pending.store(true, Ordering::Release);
        self.seek_generation.fetch_add(1, Ordering::Release);
    }

    /// Signal that a seek operation has completed.
    pub(crate) fn finish_seek(&self) {
        self.seek_pending.store(false, Ordering::Release);
    }
}
