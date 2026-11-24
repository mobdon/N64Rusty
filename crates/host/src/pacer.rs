//! Frame pacing at the game's own rate. Deadlines advance by a fixed period
//! from the previous deadline, not from "now", so the rate does not drift.
//! After a stall of a whole frame or more it starts again from now instead of
//! running frames back to back to catch up, so game speed never goes above
//! the original.

use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Pacer {
    period: Duration,
    deadline: Option<Instant>,
}

impl Pacer {
    pub fn new(rate_hz: f64) -> Self {
        Pacer {
            period: Duration::from_secs_f64(1.0 / rate_hz),
            deadline: None,
        }
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    /// Call when a frame starts at `now`; returns when the next one should.
    pub fn tick(&mut self, now: Instant) -> Instant {
        let next = match self.deadline {
            Some(d) if now < d + self.period => d + self.period,
            _ => now + self.period,
        };
        self.deadline = Some(next);
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_fixed_rate_through_jitter_and_resyncs_after_a_stall() {
        let mut p = Pacer::new(50.0);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        assert_eq!(p.tick(t0), t0 + ms(20));
        // Woken 3 ms late: the next deadline still lands on the 20 ms grid.
        assert_eq!(p.tick(t0 + ms(23)), t0 + ms(40));
        assert_eq!(p.tick(t0 + ms(40)), t0 + ms(60));
        // A 100 ms stall: start again from now, no burst of catch-up frames.
        assert_eq!(p.tick(t0 + ms(160)), t0 + ms(180));
    }
}
