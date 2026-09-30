// Playback position estimate for the Now Playing progress bar.
//
// Media APIs only report the position at discrete moments: macOS MediaRemote
// refreshes the elapsed time on play/pause/seek, and nowhear reports it on
// player queries and seeks. Between reports the position is extrapolated from
// the wall clock while playing.

use std::time::Instant;

#[derive(Clone, Debug, Default)]
pub struct PlaybackClock {
    track: String,
    anchor_pos: f64,
    anchor_at: Option<Instant>,
    duration: f64,
    playing: bool,
    last_reported: Option<f64>,
}

impl PlaybackClock {
    /// Feeds the latest state from the media API. `reported` may be stale; it only
    /// re-anchors the clock when it changes, the track changes, or play/pause flips.
    pub fn sync(&mut self, track: &str, reported: Option<f64>, duration: Option<f64>, playing: bool, now: Instant) {
        let reported = reported.filter(|p| p.is_finite() && *p >= 0.0);

        if track != self.track {
            self.track = track.to_string();
            self.anchor_pos = reported.unwrap_or(0.0);
            self.anchor_at = Some(now);
        } else if reported != self.last_reported {
            self.anchor_pos = reported.unwrap_or_else(|| self.position(now));
            self.anchor_at = Some(now);
        } else if playing != self.playing {
            // Pause or resume without a fresh position: freeze or restart from the estimate.
            self.anchor_pos = self.position(now);
            self.anchor_at = Some(now);
        }

        self.last_reported = reported;
        self.playing = playing;
        self.duration = duration.filter(|d| d.is_finite() && *d > 0.0).unwrap_or(0.0);
    }

    /// A seek reported as an event (nowhear) rather than through `sync`.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn seek(&mut self, position: f64, now: Instant) {
        if position.is_finite() && position >= 0.0 {
            self.anchor_pos = position;
            self.anchor_at = Some(now);
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn position(&self, now: Instant) -> f64 {
        let mut pos = self.anchor_pos;
        if self.playing {
            if let Some(at) = self.anchor_at {
                pos += now.saturating_duration_since(at).as_secs_f64();
            }
        }
        if self.duration > 0.0 {
            pos = pos.min(self.duration);
        }
        pos.max(0.0)
    }

    /// (position, duration) in whole seconds; both 0 when the length is unknown.
    pub fn snapshot(&self, now: Instant) -> (u32, u32) {
        if self.duration <= 0.0 {
            return (0, 0);
        }
        (self.position(now) as u32, self.duration as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn extrapolates_while_playing_with_a_stale_report() {
        let t0 = Instant::now();
        let mut c = PlaybackClock::default();
        c.sync("song", Some(10.0), Some(200.0), true, t0);
        c.sync("song", Some(10.0), Some(200.0), true, at(t0, 5)); // same stale value
        assert_eq!(c.snapshot(at(t0, 5)), (15, 200));
        assert_eq!(c.snapshot(at(t0, 500)), (200, 200), "clamped to duration");
    }

    #[test]
    fn pause_freezes_and_resume_continues_from_estimate() {
        let t0 = Instant::now();
        let mut c = PlaybackClock::default();
        c.sync("song", Some(0.0), Some(100.0), true, t0);
        c.sync("song", Some(0.0), Some(100.0), false, at(t0, 30)); // paused, API did not refresh
        assert_eq!(c.snapshot(at(t0, 90)).0, 30);
        c.sync("song", Some(0.0), Some(100.0), true, at(t0, 90));
        assert_eq!(c.snapshot(at(t0, 95)).0, 35);
    }

    #[test]
    fn fresh_reports_seeks_and_track_changes_reanchor() {
        let t0 = Instant::now();
        let mut c = PlaybackClock::default();
        c.sync("a", Some(5.0), Some(100.0), true, t0);
        c.sync("a", Some(50.0), Some(100.0), true, at(t0, 2));
        assert_eq!(c.snapshot(at(t0, 3)).0, 51);

        c.seek(80.0, at(t0, 4));
        assert_eq!(c.snapshot(at(t0, 6)).0, 82);

        c.sync("b", Some(50.0), Some(240.0), true, at(t0, 10)); // new track, same stale value
        assert_eq!(c.snapshot(at(t0, 10)), (50, 240));
        c.sync("c", None, Some(60.0), true, at(t0, 20));
        assert_eq!(c.snapshot(at(t0, 21)), (1, 60));
    }

    #[test]
    fn unknown_or_invalid_values_hide_the_bar() {
        let t0 = Instant::now();
        let mut c = PlaybackClock::default();
        c.sync("song", Some(f64::NAN), None, true, t0);
        assert_eq!(c.snapshot(at(t0, 5)), (0, 0));
        c.sync("song", Some(-3.0), Some(f64::INFINITY), true, t0);
        assert_eq!(c.snapshot(t0), (0, 0));
        c.reset();
        assert_eq!(c.snapshot(t0), (0, 0));
    }
}
