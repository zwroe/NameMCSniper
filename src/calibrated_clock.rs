use std::{
    fmt::{self, Display},
    time::{Duration, Instant},
};

use chrono::{DateTime, TimeDelta, Utc};

use crate::{schedule::Clock, time_sync::ClockSample};

const MAX_AGE: Duration = Duration::from_secs(10 * 60);
const MAX_UNCERTAINTY: Duration = Duration::from_millis(350);
const MAX_CLOCK_DIVERGENCE: TimeDelta = TimeDelta::milliseconds(100);
const HOLDOVER_DRIFT: f64 = 100.0 / 1_000_000.0;

#[derive(Clone, Copy)]
pub struct CalibratedClock {
    sample: ClockSample,
}

#[derive(Clone, Copy)]
pub struct ClockQuality {
    pub clock: Clock,
    pub age: Duration,
    pub uncertainty: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalibrationError {
    Expired,
    Uncertain(Duration),
    ClockChanged,
}

impl Display for CalibrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expired => f.write_str("The last UTC calibration is at least ten minutes old."),
            Self::Uncertain(uncertainty) => write!(
                f,
                "The estimated UTC uncertainty is {:.0} ms, above the {} ms scheduling limit.",
                uncertainty.as_secs_f64() * 1000.0,
                MAX_UNCERTAINTY.as_millis()
            ),
            Self::ClockChanged => {
                f.write_str("The wall and monotonic clocks diverged. Check UTC again.")
            }
        }
    }
}

impl std::error::Error for CalibrationError {}

impl CalibratedClock {
    pub fn new(sample: ClockSample) -> Self {
        Self { sample }
    }

    pub fn sample(self) -> ClockSample {
        self.sample
    }

    pub fn current(self) -> Result<ClockQuality, CalibrationError> {
        self.evaluate(Instant::now(), Utc::now())
    }

    fn evaluate(
        self,
        tick: Instant,
        wall: DateTime<Utc>,
    ) -> Result<ClockQuality, CalibrationError> {
        let sample = self.sample;
        let age = tick
            .checked_duration_since(sample.observed_at)
            .ok_or(CalibrationError::ClockChanged)?;
        if age >= MAX_AGE {
            return Err(CalibrationError::Expired);
        }

        let elapsed = tick
            .checked_duration_since(sample.checked_at)
            .and_then(|duration| TimeDelta::from_std(duration).ok())
            .ok_or(CalibrationError::ClockChanged)?;
        let expected_wall = sample
            .reference
            .checked_sub_signed(sample.offset)
            .and_then(|baseline| baseline.checked_add_signed(elapsed))
            .ok_or(CalibrationError::ClockChanged)?;
        if (wall - expected_wall).abs() > MAX_CLOCK_DIVERGENCE {
            return Err(CalibrationError::ClockChanged);
        }

        // A 100 ppm holdover allowance adds 6 ms of estimated uncertainty per minute.
        let uncertainty = sample
            .uncertainty
            .saturating_add(age.mul_f64(HOLDOVER_DRIFT));
        if uncertainty > MAX_UNCERTAINTY {
            return Err(CalibrationError::Uncertain(uncertainty));
        }

        Ok(ClockQuality {
            clock: Clock::from_reference(sample.reference, sample.checked_at),
            age,
            uncertainty,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tick: Instant, wall: DateTime<Utc>) -> ClockSample {
        let offset = TimeDelta::seconds(50);
        ClockSample {
            offset,
            round_trip: Duration::from_millis(20),
            sources: 2,
            checked_at: tick,
            observed_at: tick,
            reference: wall + offset,
            disagreement: TimeDelta::milliseconds(5),
            uncertainty: Duration::from_millis(30),
        }
    }

    fn wall_time() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    #[test]
    fn retains_a_fifty_second_correction_during_holdover() {
        let tick = Instant::now();
        let wall = wall_time();
        let calibration = CalibratedClock::new(sample(tick, wall));
        let age = Duration::from_secs(300);
        let quality = calibration
            .evaluate(tick + age, wall + TimeDelta::minutes(5))
            .unwrap();
        assert_eq!(quality.age, age);
        assert_eq!(quality.uncertainty, Duration::from_millis(60));
        assert_eq!(
            calibration.sample().reference,
            wall + TimeDelta::seconds(50)
        );
        assert!(quality.clock.now() >= calibration.sample().reference);
    }

    #[test]
    fn expires_at_ten_minutes_from_the_oldest_observation() {
        let tick = Instant::now();
        let wall = wall_time();
        let mut sample = sample(tick, wall);
        sample.observed_at = tick - Duration::from_secs(1);
        let elapsed = MAX_AGE - Duration::from_secs(1);
        let result = CalibratedClock::new(sample)
            .evaluate(tick + elapsed, wall + TimeDelta::from_std(elapsed).unwrap());
        assert!(matches!(result, Err(CalibrationError::Expired)));
    }

    #[test]
    fn rejects_holdover_when_estimated_uncertainty_grows_past_the_limit() {
        let tick = Instant::now();
        let wall = wall_time();
        let mut sample = sample(tick, wall);
        sample.uncertainty = Duration::from_millis(340);
        let calibration = CalibratedClock::new(sample);
        assert!(calibration.evaluate(tick, wall).is_ok());
        let result = calibration.evaluate(
            tick + Duration::from_secs(200),
            wall + TimeDelta::seconds(200),
        );
        assert!(matches!(
            result,
            Err(CalibrationError::Uncertain(uncertainty))
                if uncertainty == Duration::from_millis(360)
        ));
    }

    #[test]
    fn rejects_a_wall_clock_step_or_paused_monotonic_clock() {
        let tick = Instant::now();
        let wall = wall_time();
        let calibration = CalibratedClock::new(sample(tick, wall));
        for change in [TimeDelta::milliseconds(-101), TimeDelta::seconds(30)] {
            let result = calibration.evaluate(
                tick + Duration::from_secs(1),
                wall + TimeDelta::seconds(1) + change,
            );
            assert!(matches!(result, Err(CalibrationError::ClockChanged)));
        }
    }
}
