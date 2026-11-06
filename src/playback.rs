use librespot::{core::SpotifyUri, playback::player::PlayerEvent};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    Loading,
    Playing,
    Paused,
    Unavailable,
}
pub struct Playback {
    phase: Phase,
    request: Option<u64>,
    track: Option<SpotifyUri>,
    position: u32,
    at: Instant,
}
impl Default for Playback {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            request: None,
            track: None,
            position: 0,
            at: Instant::now(),
        }
    }
}
impl Playback {
    pub fn soloist(&mut self, state: &crate::soloist::State) {
        self.phase = if !state.connected || !state.active {
            Phase::Idle
        } else {
            match state.status.as_str() {
                "playing" => Phase::Playing,
                "paused" => Phase::Paused,
                "buffering" => Phase::Loading,
                _ => Phase::Idle,
            }
        };
        self.position = state.position();
        self.at = Instant::now();
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn active(&self) -> bool {
        matches!(self.phase, Phase::Playing | Phase::Paused)
    }
    pub fn playing(&self) -> bool {
        self.phase == Phase::Playing
    }
    pub fn paused(&self) -> bool {
        self.phase == Phase::Paused
    }
    pub fn loading(&self) -> bool {
        self.phase == Phase::Loading
    }
    pub fn error(&self) -> &str {
        if self.phase == Phase::Unavailable {
            "Track unavailable; see spotify-connect.log"
        } else {
            ""
        }
    }
    pub fn position_ms(&self) -> u32 {
        let elapsed = if self.playing() {
            self.at.elapsed()
        } else {
            Duration::ZERO
        };
        self.position
            .saturating_add((elapsed.as_secs().min(u64::from(u32::MAX) / 1000) * 1000) as u32)
    }
    fn matches(&self, request: u64, track: &SpotifyUri) -> bool {
        self.request == Some(request) && self.track.as_ref().is_none_or(|current| current == track)
    }
    pub fn update(&mut self, event: &PlayerEvent) -> bool {
        use PlayerEvent::*;
        match event {
            PlayRequestIdChanged { play_request_id } => {
                self.reset();
                self.request = Some(*play_request_id);
                self.phase = Phase::Loading;
            }
            Loading {
                play_request_id,
                track_id,
                position_ms,
            } => {
                if self.request != Some(*play_request_id) {
                    return false;
                }
                self.track = Some(track_id.clone());
                self.phase = Phase::Loading;
                self.position = *position_ms;
                self.at = Instant::now();
            }
            Playing {
                play_request_id,
                track_id,
                position_ms,
            }
            | Paused {
                play_request_id,
                track_id,
                position_ms,
            } => {
                if !self.matches(*play_request_id, track_id) {
                    return false;
                }
                self.track = Some(track_id.clone());
                self.phase = if matches!(event, Playing { .. }) {
                    Phase::Playing
                } else {
                    Phase::Paused
                };
                self.position = *position_ms;
                self.at = Instant::now();
            }
            Unavailable {
                play_request_id,
                track_id,
            } => {
                // Preload failures carry the current request ID but a different track.
                let current = self.matches(*play_request_id, track_id);
                log::warn!(
                    "receiver: track unavailable ({track_id}, request {play_request_id}, current={current})"
                );
                if !current {
                    return false;
                }
                self.phase = Phase::Unavailable;
                self.position = 0;
            }
            Stopped {
                play_request_id,
                track_id,
            }
            | EndOfTrack {
                play_request_id,
                track_id,
            } => {
                if !self.matches(*play_request_id, track_id) {
                    return false;
                }
                self.reset();
            }
            SessionDisconnected { .. } => self.reset(),
            PositionChanged {
                play_request_id,
                track_id,
                position_ms,
            }
            | PositionCorrection {
                play_request_id,
                track_id,
                position_ms,
            }
            | Seeked {
                play_request_id,
                track_id,
                position_ms,
            } => {
                if !self.active() || !self.matches(*play_request_id, track_id) {
                    return false;
                }
                self.position = *position_ms;
                self.at = Instant::now();
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn track(n: u8) -> SpotifyUri {
        SpotifyUri::from_uri(&format!("spotify:track:{n:022}")).unwrap()
    }
    fn load(state: &mut Playback, request: u64) {
        assert!(state.update(&PlayerEvent::PlayRequestIdChanged {
            play_request_id: request
        }));
        assert!(state.update(&PlayerEvent::Loading {
            play_request_id: request,
            track_id: track(1),
            position_ms: 1000
        }));
    }
    fn playing(request: u64) -> PlayerEvent {
        PlayerEvent::Playing {
            play_request_id: request,
            track_id: track(1),
            position_ms: 1000,
        }
    }
    #[tokio::test(start_paused = true)]
    async fn loading_and_unavailable_do_not_claim_playback_or_advance_time() {
        let mut state = Playback::default();
        load(&mut state, 1);
        assert!(state.loading() && !state.active() && !state.playing());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(state.position_ms(), 1000);
        state.update(&PlayerEvent::Unavailable {
            play_request_id: 1,
            track_id: track(1),
        });
        assert!(!state.active() && !state.loading() && !state.playing());
        assert!(!state.error().is_empty());
        assert_eq!(state.position_ms(), 0);
        load(&mut state, 2);
        assert!(state.error().is_empty());
        state.update(&playing(2));
        assert!(state.active() && state.playing());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(state.position_ms(), 3000);
        state.update(&PlayerEvent::Paused {
            play_request_id: 2,
            track_id: track(1),
            position_ms: 3000,
        });
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(state.active() && state.paused() && !state.playing());
        assert_eq!(state.position_ms(), 3000);
    }
    #[test]
    fn failed_preload_and_stale_events_do_not_interrupt_the_current_track() {
        let mut state = Playback::default();
        load(&mut state, 2);
        state.update(&playing(2));
        for event in [
            PlayerEvent::Unavailable {
                play_request_id: 2,
                track_id: track(2),
            },
            PlayerEvent::Unavailable {
                play_request_id: 1,
                track_id: track(1),
            },
            PlayerEvent::Stopped {
                play_request_id: 1,
                track_id: track(1),
            },
            PlayerEvent::PositionChanged {
                play_request_id: 1,
                track_id: track(1),
                position_ms: 9000,
            },
            playing(1),
        ] {
            assert!(!state.update(&event));
            assert!(state.playing() && state.error().is_empty());
            assert_eq!(state.position_ms(), 1000);
        }
        state.update(&PlayerEvent::EndOfTrack {
            play_request_id: 2,
            track_id: track(1),
        });
        assert!(!state.active());
        assert!(!state.update(&playing(2)));
        load(&mut state, 3);
        state.update(&playing(3));
        state.update(&PlayerEvent::SessionDisconnected {
            connection_id: String::new(),
            user_name: String::new(),
        });
        assert!(!state.active() && !state.loading());
        assert!(!state.update(&playing(3)));
    }
}
