use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

#[derive(Default)]
pub(crate) struct SharedState {
    active: AtomicBool, // is a track loaded?
    paused: AtomicBool,
    samples_played: AtomicU64, // playback position, in output samples
    seek_pending: AtomicBool,  // a seek is in progress
    seek_epoch: AtomicU64,     // bumped per seek; pairs with seek_ack
    seek_ack: AtomicU64,       // callback's "I drained the stale audio" signal
    duration_micros: AtomicU64,
    track_epoch: AtomicU64,    // bumped per track; invalidates stale duration scans
    callback_count: AtomicU64, // heartbeat — the watchdog reads this
    rebuilding: AtomicBool,    // stream is being rebuilt
    shutdown: AtomicBool,
    output_rate: AtomicU32,
    output_channels: AtomicUsize,
}

impl SharedState {
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Returns number of samples played so far
    pub(crate) fn get_samples(&self) -> u64 {
        self.samples_played.load(Ordering::Acquire)
    }

    pub(crate) fn is_seeking(&self) -> bool {
        self.seek_pending.load(Ordering::Acquire)
    }

    /// Returns the playable duration in seconds
    pub(crate) fn get_duration_secs(&self) -> f64 {
        self.duration_micros.load(Ordering::Acquire) as f64 / 1_000_000.0
    }

    /// Live output sample rate; updated by the supervisor on rebuild
    pub(crate) fn output_rate(&self) -> u32 {
        self.output_rate.load(Ordering::Acquire)
    }

    /// Live output channel count; updated by the supervisor on rebuild
    pub(crate) fn output_channels(&self) -> usize {
        self.output_channels.load(Ordering::Acquire)
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

    /// Set paused status of player. Only the decoder worker writes this.
    pub(crate) fn set_paused(&self, val: bool) {
        self.paused.store(val, Ordering::Relaxed);
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

    /// Mark a new track as current, returning its epoch. A duration scan applies
    /// its result only while the epoch still matches, so a track change or stop
    /// discards an in-flight scan.
    pub(crate) fn bump_track_epoch(&self) -> u64 {
        self.track_epoch.fetch_add(1, Ordering::Release) + 1
    }

    /// Current track epoch.
    pub(crate) fn track_epoch(&self) -> u64 {
        self.track_epoch.load(Ordering::Acquire)
    }

    /// Record the live output format; called by the supervisor on (re)build
    pub(crate) fn set_output_format(&self, rate: u32, channels: usize) {
        self.output_rate.store(rate, Ordering::Release);
        self.output_channels.store(channels, Ordering::Release);
    }

    // ======================
    //      Seek Control
    // ======================

    /// Signal that a seek operation has started.
    ///
    /// The epoch is bumped before the pending flag so a callback that observes
    /// the flag (Acquire) also sees the new epoch.
    pub(crate) fn start_seek(&self) {
        self.seek_epoch.fetch_add(1, Ordering::Relaxed);
        self.seek_pending.store(true, Ordering::Release);
    }

    /// Signal that a seek operation has completed.
    pub(crate) fn finish_seek(&self) {
        self.seek_pending.store(false, Ordering::Release);
    }

    /// Current seek epoch; bumped on every `start_seek`
    pub(crate) fn seek_epoch(&self) -> u64 {
        self.seek_epoch.load(Ordering::Relaxed)
    }

    /// Audio callback's acknowledgement that it drained the ring for `epoch`
    pub(crate) fn ack_seek(&self, epoch: u64) {
        self.seek_ack.store(epoch, Ordering::Release);
    }

    /// Has the audio callback drained the ring for the current epoch?
    pub(crate) fn seek_acked(&self) -> bool {
        self.seek_ack.load(Ordering::Acquire) == self.seek_epoch()
    }

    // ==========================
    //      Stream Liveness
    // ==========================

    /// Incremented by the audio callback on every invocation
    pub(crate) fn bump_callback(&self) {
        self.callback_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn callback_count(&self) -> u64 {
        self.callback_count.load(Ordering::Relaxed)
    }

    /// Gates the decoder and audio callback while the stream is rebuilt
    pub(crate) fn set_rebuilding(&self, val: bool) {
        self.rebuilding.store(val, Ordering::Release);
    }

    pub(crate) fn is_rebuilding(&self) -> bool {
        self.rebuilding.load(Ordering::Acquire)
    }

    /// Engine teardown flag; unblocks busy loops so threads can join
    pub(crate) fn set_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_unstarted_is_acked() {
        assert!(SharedState::default().seek_acked()); // epoch 0 == ack 0
    }

    #[test]
    fn start_seek_bumps_epoch_and_unacks() {
        let s = SharedState::default();
        let before = s.seek_epoch();
        s.start_seek();
        assert_eq!(s.seek_epoch(), before + 1);
        assert!(s.is_seeking());
        assert!(!s.seek_acked());
    }

    #[test]
    fn ack_must_match_current_epoch() {
        let s = SharedState::default();
        s.start_seek();
        let epoch = s.seek_epoch();
        s.ack_seek(epoch - 1); // stale ack from a prior seek
        assert!(!s.seek_acked());
        s.ack_seek(epoch);
        assert!(s.seek_acked());
    }

    #[test]
    fn restarted_seek_invalidates_prior_ack() {
        // A new seek before the callback acks must re-arm the drain wait, or
        // wait_for_seek_drain would skip draining stale audio.
        let s = SharedState::default();
        s.start_seek();
        s.ack_seek(s.seek_epoch());
        assert!(s.seek_acked());
        s.start_seek();
        assert!(!s.seek_acked());
    }

    #[test]
    fn finish_seek_clears_pending() {
        let s = SharedState::default();
        s.start_seek();
        s.finish_seek();
        assert!(!s.is_seeking());
    }

    #[test]
    fn samples_set_add_reset() {
        let s = SharedState::default();
        s.set_samples(100);
        assert_eq!(s.get_samples(), 100);
        s.add_samples(50);
        assert_eq!(s.get_samples(), 150);
        s.reset_samples();
        assert_eq!(s.get_samples(), 0);
    }
}
