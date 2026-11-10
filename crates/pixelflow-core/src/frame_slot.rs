//! A replaceable single-frame slot: latest-frame-wins transport
//! between two threads.
//!
//! The slot holds at most one frame. Publishing a frame replaces
//! whatever is still waiting, so a slow consumer always gets the
//! newest frame, never a stale one, and the producer never blocks.
//! Frames replaced before any consumer took them are counted, not
//! hidden: [`FrameSlot::dropped`] reports how many.
//!
//! The slot is also the wait primitive for the consumer:
//! [`FrameSlot::take_wait`] blocks until a frame arrives or the
//! timeout passes, so the consumer polls on a bounded wait instead
//! of a busy loop.

use crate::Frame;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// One replaceable frame, shared between a producer thread and a
/// consumer thread.
#[derive(Default)]
pub struct FrameSlot {
    frame: Mutex<Option<Frame>>,
    changed: Condvar,
    dropped: AtomicU64,
}

impl FrameSlot {
    /// An empty slot.
    #[must_use]
    pub fn new() -> FrameSlot {
        FrameSlot::default()
    }

    /// Store a frame, replacing whatever is still waiting. A
    /// replaced frame counts as dropped. Wakes one waiting
    /// consumer, if any.
    pub fn publish(&self, frame: Frame) {
        let mut slot = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        *slot = Some(frame);
        self.changed.notify_one();
    }

    /// Take the newest waiting frame, if any. Never blocks.
    #[must_use]
    pub fn take(&self) -> Option<Frame> {
        let mut slot = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.take()
    }

    /// Take the newest waiting frame, waiting up to `timeout` for
    /// one to arrive. Returns `None` when nothing arrives in time.
    #[must_use]
    pub fn take_wait(&self, timeout: Duration) -> Option<Frame> {
        let slot = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut slot = if slot.is_none() {
            self.changed
                .wait_timeout(slot, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0
        } else {
            slot
        };
        slot.take()
    }

    /// Frames replaced by a newer publish before any consumer took
    /// them.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PixelFormat;
    use std::sync::Arc;

    fn frame_with_first_byte(value: u8) -> Result<Frame, String> {
        Frame::new(1, 1, PixelFormat::Rgb8, vec![value, 0, 0]).map_err(|e| e.to_string())
    }

    #[test]
    fn publish_replaces_the_waiting_frame() -> Result<(), String> {
        let slot = FrameSlot::new();
        slot.publish(frame_with_first_byte(1)?);
        slot.publish(frame_with_first_byte(2)?);
        let frame = slot.take().ok_or("expected a frame")?;
        assert_eq!(frame.data[0], 2, "the newest frame wins");
        assert_eq!(slot.dropped(), 1, "the replaced frame is counted");
        assert!(slot.take().is_none(), "the slot is empty after taking");
        Ok(())
    }

    #[test]
    fn a_slow_consumer_gets_only_the_newest_frame() -> Result<(), String> {
        let slot = FrameSlot::new();
        for value in 1..=5 {
            slot.publish(frame_with_first_byte(value)?);
        }
        let frame = slot.take().ok_or("expected a frame")?;
        assert_eq!(frame.data[0], 5, "the newest of five publishes wins");
        assert_eq!(slot.dropped(), 4, "the four replaced frames are counted");
        Ok(())
    }

    #[test]
    fn take_wait_times_out_when_nothing_arrives() {
        let slot = FrameSlot::new();
        assert!(
            slot.take_wait(Duration::from_millis(20)).is_none(),
            "no frame arrives, so the wait times out"
        );
    }

    #[test]
    fn take_wait_returns_a_frame_published_after_the_wait_starts() -> Result<(), String> {
        let slot = Arc::new(FrameSlot::new());
        let publisher = slot.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            if let Ok(frame) = frame_with_first_byte(7) {
                publisher.publish(frame);
            }
        });
        let frame = slot
            .take_wait(Duration::from_secs(5))
            .ok_or("the published frame arrives")?;
        assert_eq!(frame.data[0], 7);
        Ok(())
    }
}
