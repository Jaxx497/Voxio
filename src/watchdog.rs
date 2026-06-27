use crate::{event::RebindReason, output::OutputControl, state::SharedState};
use cpal::traits::{DeviceTrait, HostTrait};
use crossbeam_channel::Sender;
use std::{
    mem,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

const TICK_SLICE: Duration = Duration::from_millis(50);

/// Detects device loss, OS-default changes, and zombified streams.
pub(crate) fn spawn(
    state: Arc<SharedState>,
    ctrl: Sender<OutputControl>,
    tick_duration: Duration,
    zombie_tick: u32,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let host = cpal::default_host();
        let mut brain = WatchdogBrain::new(state.callback_count(), default_name(&host));

        loop {
            let mut slept = Duration::ZERO;
            while slept < tick_duration {
                thread::sleep(TICK_SLICE);
                slept += TICK_SLICE;
                if state.is_shutdown() {
                    return;
                }
            }

            if let Some(reason) = brain.tick(&state, default_name(&host), zombie_tick)
                && ctrl.send(OutputControl::Rebuild(reason)).is_err()
            {
                return;
            }
        }
    })
}

fn default_name(host: &cpal::Host) -> Option<String> {
    host.default_output_device()
        .and_then(|d| d.description().ok())
        .map(|d| d.name().to_string())
}

/// Per-tick rebuild decision, separated from the timing loop for testability.
struct WatchdogBrain {
    last_cb: u64,
    last_name: Option<String>,
    /// Awaits debounce confirmation
    pending: Option<Option<String>>,
    stale_ticks: u32,
    resync: bool,
}

impl WatchdogBrain {
    fn new(last_cb: u64, last_name: Option<String>) -> Self {
        WatchdogBrain {
            last_cb,
            last_name,
            pending: None,
            stale_ticks: 0,
            resync: false,
        }
    }

    fn tick(
        &mut self,
        state: &SharedState,
        name: Option<String>,
        zombie_tick: u32,
    ) -> Option<RebindReason> {
        // A rebuild is in flight; stay quiet and re-baseline afterwards.
        if state.is_rebuilding() {
            self.stale_ticks = 0;
            self.resync = true;
            return None;
        }

        if mem::take(&mut self.resync) {
            self.last_cb = state.callback_count();
            self.last_name = name;
            self.pending = None;
            self.stale_ticks = 0;
            return None;
        }

        // Heartbeat: the callback count must advance while audibly playing.
        let cb = state.callback_count();
        let stalled = cb == self.last_cb && state.is_active() && !state.is_paused();
        self.last_cb = cb;
        if stalled {
            self.stale_ticks += 1;
            if self.stale_ticks >= zombie_tick {
                self.stale_ticks = 0;
                self.resync = true;
                return Some(RebindReason::StreamZombie);
            }
        } else {
            self.stale_ticks = 0;
        }

        // Default-device drift, debounced one tick to ride out OS flapping.
        if name != self.last_name {
            if self.pending.as_ref() == Some(&name) {
                self.last_name = name;
                self.pending = None;
                self.resync = true;
                return Some(RebindReason::DefaultChanged);
            }
            self.pending = Some(name);
        } else {
            self.pending = None;
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ZOMBIE_TICKS: u32 = 3;

    fn active_state() -> SharedState {
        let state = SharedState::default();
        state.set_active(true);
        state
    }

    #[test]
    fn zombie_after_stale_ticks() {
        let state = active_state();
        let mut brain = WatchdogBrain::new(state.callback_count(), None);

        for _ in 0..ZOMBIE_TICKS - 1 {
            assert_eq!(brain.tick(&state, None, ZOMBIE_TICKS), None);
        }
        assert_eq!(
            brain.tick(&state, None, ZOMBIE_TICKS),
            Some(RebindReason::StreamZombie)
        );
    }

    #[test]
    fn no_zombie_while_callbacks_flow() {
        let state = active_state();
        let mut brain = WatchdogBrain::new(state.callback_count(), None);

        for _ in 0..ZOMBIE_TICKS * 2 {
            state.bump_callback();
            assert_eq!(brain.tick(&state, None, ZOMBIE_TICKS), None);
        }
    }

    #[test]
    fn no_zombie_while_paused_or_idle() {
        let state = active_state();
        state.set_paused(true);
        let mut brain = WatchdogBrain::new(state.callback_count(), None);

        for _ in 0..ZOMBIE_TICKS * 2 {
            assert_eq!(brain.tick(&state, None, ZOMBIE_TICKS), None);
        }
    }

    #[test]
    fn default_change_is_debounced() {
        let state = SharedState::default();
        let mut brain = WatchdogBrain::new(0, Some("A".into()));

        // First observation arms the debounce; second confirms it
        assert_eq!(brain.tick(&state, Some("B".into()), ZOMBIE_TICKS), None);
        assert_eq!(
            brain.tick(&state, Some("B".into()), ZOMBIE_TICKS),
            Some(RebindReason::DefaultChanged)
        );
    }

    #[test]
    fn default_flapping_does_not_trigger() {
        let state = SharedState::default();
        let mut brain = WatchdogBrain::new(0, Some("A".into()));

        assert_eq!(brain.tick(&state, Some("B".into()), ZOMBIE_TICKS), None);
        assert_eq!(brain.tick(&state, Some("A".into()), ZOMBIE_TICKS), None);
        assert_eq!(brain.tick(&state, Some("A".into()), ZOMBIE_TICKS), None);
    }

    #[test]
    fn quiet_then_rebaselines_while_rebuilding() {
        let state = active_state();
        let mut brain = WatchdogBrain::new(state.callback_count(), Some("A".into()));

        state.set_rebuilding(true);
        assert_eq!(brain.tick(&state, Some("B".into()), ZOMBIE_TICKS), None);

        // First post-rebuild tick adopts the new device silently
        state.set_rebuilding(false);
        assert_eq!(brain.tick(&state, Some("B".into()), ZOMBIE_TICKS), None);
        assert_eq!(brain.tick(&state, Some("B".into()), ZOMBIE_TICKS), None);
    }
}
