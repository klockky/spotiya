use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};
use tokio::time::Instant;

const HOLD: Duration = Duration::from_secs(60);
const LEVEL: f64 = 0.2;
const ATTACK_FRAMES: f64 = librespot::playback::SAMPLE_RATE as f64 * 0.08;
const RELEASE_FRAMES: f64 = librespot::playback::SAMPLE_RATE as f64 * 0.3;

pub struct Ducking {
    origin: Instant,
    until: AtomicU32,
    pub volume: crate::volume_guard::VolumeGuard,
}
impl Ducking {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            until: AtomicU32::new(0),
            volume: crate::volume_guard::VolumeGuard::new(),
        }
    }
    pub fn set_busy(&self, busy: bool) {
        let until = if busy {
            (self.origin.elapsed().as_millis() as u32)
                .wrapping_add(HOLD.as_millis() as u32)
                .max(1)
        } else {
            0
        };
        self.until.store(until, Ordering::Release);
        self.volume.activity(busy);
        signal_external(busy);
    }
    pub fn active(&self) -> bool {
        let until = self.until.load(Ordering::Acquire);
        until != 0 && until.wrapping_sub(self.origin.elapsed().as_millis() as u32) as i32 > 0
    }
}

/// Soloist renders audio in its own process, so [`Gain`] never sees its samples.
/// Our apulse (soloist/apulse-duck.c) ducks the stream while this file exists and is
/// fresh; rewriting it on every busy event keeps its mtime inside apulse's hold time.
pub const EXTERNAL: &str = "/run/spotiya-duck";
fn signal_external(busy: bool) {
    if cfg!(test) {
        return;
    }
    if busy {
        let _ = std::fs::write(EXTERNAL, b"");
    } else {
        let _ = std::fs::remove_file(EXTERNAL);
    }
}

pub struct Gain(f64);
impl Gain {
    pub fn new() -> Self {
        Self(1.0)
    }
    pub fn apply(&mut self, samples: &mut [f64], busy: bool) {
        let target = if busy { LEVEL } else { 1.0 };
        if self.0 == 1.0 && target == 1.0 {
            return;
        }
        let step = (1.0 - LEVEL)
            / if target < self.0 {
                ATTACK_FRAMES
            } else {
                RELEASE_FRAMES
            };
        for frame in samples.chunks_exact_mut(usize::from(librespot::playback::NUM_CHANNELS)) {
            self.0 = if target < self.0 {
                (self.0 - step).max(target)
            } else {
                (self.0 + step).min(target)
            };
            for sample in frame {
                *sample *= self.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn idle_disconnect_and_missing_idle_event_restore_volume() {
        let duck = Ducking::new();
        assert!(!duck.active());
        duck.set_busy(true);
        assert!(duck.active());
        tokio::time::advance(HOLD - Duration::from_secs(1)).await;
        assert!(duck.active());
        duck.set_busy(true);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(duck.active());
        duck.set_busy(false);
        assert!(!duck.active());
        duck.set_busy(true);
        tokio::time::advance(HOLD).await;
        assert!(!duck.active());
    }
    #[test]
    fn ramps_are_stereo_linked_bounded_and_independent_of_packet_size() {
        let mut gain = Gain::new();
        let mut whole = vec![1.0; 10000];
        gain.apply(&mut whole, true);
        assert!(whole[0] < 1.0 && whole[0] > 0.99);
        assert_eq!(whole[9999], LEVEL);
        assert!(whole.windows(2).all(|w| w[0] >= w[1]));
        assert!(whole.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]));
        let mut split = vec![1.0; 10000];
        let mut split_gain = Gain::new();
        for chunk in split.chunks_mut(128) {
            split_gain.apply(chunk, true);
        }
        assert_eq!(whole, split);
        let mut restored = vec![1.0; 30000];
        gain.apply(&mut restored, false);
        assert!(restored[0] > LEVEL && restored[0] < 0.21);
        assert_eq!(restored[29999], 1.0);
        assert!(restored.windows(2).all(|w| w[0] <= w[1]));
        let original = vec![-0.5, 0.5, 0.0, 1.0];
        let mut untouched = original.clone();
        gain.apply(&mut untouched, false);
        assert_eq!(untouched, original);
    }
}
