use crate::state::SharedState;
use crossbeam_channel::Receiver;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;

pub(crate) struct TapWriter {
    producer: Producer<f32>,
}

pub(crate) struct TapReader {
    consumer: Consumer<f32>,
    capacity: usize,
}

pub(crate) fn new_tap(capacity: usize) -> (TapWriter, TapReader) {
    let (producer, consumer) = RingBuffer::new(capacity);
    (TapWriter { producer }, TapReader { consumer, capacity })
}

impl TapWriter {
    pub(crate) fn push(&mut self, samples: &[f32]) {
        let available = self.producer.slots();
        if available == 0 {
            return;
        }
        let to_write = samples.len().min(available);
        let skip = samples.len() - to_write;
        if let Ok(chunk) = self.producer.write_chunk_uninit(to_write) {
            chunk.fill_from_iter(samples[skip..].iter().copied());
        }
    }
}

impl TapReader {
    fn fill_latest(&mut self, amount: usize, out: &mut Vec<f32>) {
        out.clear();
        let output_len = amount.min(self.capacity);
        let available = self.consumer.slots();
        if available == 0 {
            return;
        }
        if available > output_len {
            let skip = available - output_len;
            if let Ok(chunk) = self.consumer.read_chunk(skip) {
                chunk.commit_all();
            }
        }
        let to_read = self.consumer.slots().min(output_len);
        if to_read == 0 {
            return;
        }
        if let Ok(chunk) = self.consumer.read_chunk(to_read) {
            let (first, second) = chunk.as_slices();
            out.extend_from_slice(first);
            out.extend_from_slice(second);
            chunk.commit_all();
        }
    }
}

/// A `Send` handle to the post-resample output samples, for visualization.
///
/// Obtained once from [`Vox::take_tap`](crate::Vox::take_tap). Move it to your
/// render thread and poll [`latest`](Self::latest) for the most recent samples.
pub struct TapHandle {
    tap: TapReader,
    swap: Receiver<TapReader>,
    state: Arc<SharedState>,
    buf: Vec<f32>,
}

impl TapHandle {
    pub(crate) fn new(tap: TapReader, swap: Receiver<TapReader>, state: Arc<SharedState>) -> Self {
        TapHandle {
            tap,
            swap,
            state,
            buf: Vec::new(),
        }
    }

    /// Returns up to `amount` of the most recent interleaved output samples.
    ///
    /// Oldest samples are dropped first when the tap overflows. The returned
    /// slice borrows an internal buffer and is valid until the next call.
    pub fn latest(&mut self, amount: usize) -> &[f32] {
        while let Ok(t) = self.swap.try_recv() {
            self.tap = t;
        }
        self.tap.fill_latest(amount, &mut self.buf);
        &self.buf
    }

    /// Live output sample rate at the time of the last stream build.
    pub fn sample_rate(&self) -> u32 {
        self.state.output_rate()
    }

    /// Live output channel count at the time of the last stream build.
    pub fn channels(&self) -> usize {
        self.state.output_channels()
    }
}

// Fail build if TapHandle somehow becomes !Send
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<TapHandle>();
};

#[cfg(test)]
mod tests {
    use super::*;

    const CAPACITY: usize = 10;

    fn fresh_tap() -> (TapWriter, TapReader) {
        new_tap(CAPACITY)
    }

    #[test]
    fn basic_write_and_read() {
        let (mut w, mut r) = fresh_tap();
        w.push(&[1.0, 2.0, 3.0]);
        let mut buf = Vec::new();
        r.fill_latest(3, &mut buf);
        assert_eq!(buf, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn drops_oldest_when_overflow() {
        let (mut w, mut r) = fresh_tap();
        let mut buf = Vec::new();

        let samples: Vec<f32> = (0..CAPACITY + 3).map(|i| i as f32).collect();
        w.push(&samples);
        r.fill_latest(CAPACITY, &mut buf);
        let expected: Vec<f32> = (3..CAPACITY + 3).map(|i| i as f32).collect();
        assert_eq!(buf, expected);
    }

    #[test]
    fn read_less_than_available() {
        let (mut w, mut r) = fresh_tap();
        let mut buf = Vec::new();

        w.push(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        r.fill_latest(2, &mut buf);
        assert_eq!(buf, [4.0, 5.0]);
    }

    #[test]
    fn read_empty_buffer() {
        let (_, mut r) = fresh_tap();
        let mut buf = Vec::new();

        r.fill_latest(5, &mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn read_amount_greater_than_capacity() {
        let (mut w, mut r) = fresh_tap();
        let mut buf = Vec::new();

        let samples: Vec<f32> = (0..CAPACITY).map(|i| i as f32).collect();
        w.push(&samples);
        r.fill_latest(CAPACITY * 2, &mut buf);
        assert_eq!(buf, samples);
        assert!(buf.len() <= CAPACITY);
    }

    #[test]
    fn multiple_push_and_read_cycles() {
        let (mut w, mut r) = fresh_tap();
        let mut buf = Vec::new();

        w.push(&[1.0, 2.0, 3.0]);
        r.fill_latest(2, &mut buf);
        assert_eq!(buf, [2.0, 3.0]);

        w.push(&[4.0, 5.0]);
        r.fill_latest(3, &mut buf);
        assert_eq!(buf, [4.0, 5.0]);

        w.push(&[6.0, 7.0, 8.0]);
        r.fill_latest(10, &mut buf);
        assert_eq!(buf, [6.0, 7.0, 8.0]);
    }

    #[test]
    fn fill_latest_clears_output() {
        let (mut w, mut r) = fresh_tap();
        let mut buf = vec![99.0; 5];

        w.push(&[1.0, 2.0]);
        r.fill_latest(2, &mut buf);
        assert_eq!(buf, [1.0, 2.0]);
    }

    #[test]
    fn push_when_full_noop() {
        let small: usize = 3;
        let (mut w, mut r) = new_tap(small);

        w.push(&[1.0, 2.0, 3.0]);
        w.push(&[4.0, 5.0, 6.0]);
        w.push(&[7.0, 8.0, 9.0]);

        let mut buf = Vec::new();
        r.fill_latest(small, &mut buf);
        assert_eq!(buf, [1.0, 2.0, 3.0]);
    }

    // -- TapHandle tests -------------------------------------------

    fn fresh_handle() -> (TapWriter, TapHandle) {
        let state = Arc::new(SharedState::default());
        state.set_output_format(44100, 2);

        let (w, r) = new_tap(CAPACITY);
        let (_tx, swap) = crossbeam_channel::unbounded();
        let handle = TapHandle::new(r, swap, state);
        (w, handle)
    }

    #[test]
    fn handle_drains_swap_channel() {
        let state = Arc::new(SharedState::default());
        state.set_output_format(48000, 1);
        let (_w1, r1) = new_tap(CAPACITY);
        let (tx, swap) = crossbeam_channel::unbounded();

        let (mut w2, r2) = new_tap(CAPACITY);
        w2.push(&[42.0]);
        tx.send(r2).unwrap();

        let mut h = TapHandle::new(r1, swap, state);
        let result = h.latest(1);
        assert_eq!(result, [42.0]);
    }

    #[test]
    fn handle_sample_rate_and_channels() {
        let (_, h) = fresh_handle();
        assert_eq!(h.sample_rate(), 44100);
        assert_eq!(h.channels(), 2);
    }

    #[test]
    fn handle_multiple_swaps_uses_newest() {
        let state = Arc::new(SharedState::default());
        state.set_output_format(48000, 2);
        let (_w0, r0) = new_tap(CAPACITY);
        let (tx, swap) = crossbeam_channel::unbounded();

        let (mut w1, r1) = new_tap(CAPACITY);
        w1.push(&[10.0]);
        tx.send(r1).unwrap();

        let (mut w2, r2) = new_tap(CAPACITY);
        w2.push(&[20.0, 30.0]);
        tx.send(r2).unwrap();

        let mut h = TapHandle::new(r0, swap, state);
        let result = h.latest(2);
        assert_eq!(result, [20.0, 30.0]);
    }
}
