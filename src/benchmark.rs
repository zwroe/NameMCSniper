use std::time::Duration;

use anyhow::{Result, ensure};
use chrono::TimeDelta;

use crate::{
    minecraft::ClaimOutcome,
    schedule::{self, Clock, LiveSlots},
};

const SAMPLE_SPACING_MS: i64 = 57;
pub const DEFAULT_SAMPLES: u16 = 40;

pub async fn run(samples: u16) -> Result<u8> {
    ensure!(samples > 0, "Choose at least one timing sample.");
    println!("Offline timing check");
    println!("Measures local scheduler dispatch with simulated requests.");

    let clock = Clock::anchored(TimeDelta::zero());
    let start = clock.now();
    let slots: Vec<_> = (1..=samples)
        .map(|sample| start + TimeDelta::milliseconds(i64::from(sample) * SAMPLE_SPACING_MS))
        .collect();
    let mut clock = LiveSlots::new(Duration::ZERO, clock);
    let attempts = schedule::execute(
        &slots,
        async |slot| clock.wait(slot).await,
        async || ClaimOutcome::Unavailable,
    )
    .await;

    let observed = attempts.len();
    let missed = usize::from(samples) - observed;
    println!("Scheduled {samples}  Observed {observed}  Missed {missed}");
    if observed == 0 {
        println!("The scheduler missed every slot.");
        return Ok(0);
    }

    let mut delays: Vec<_> = attempts
        .iter()
        .map(|attempt| {
            (attempt.sent - attempt.slot)
                .num_microseconds()
                .expect("dispatch delay is bounded by slot grace")
        })
        .collect();
    delays.sort_unstable();
    println!(
        "Local dispatch delay  p50 {:.3} ms  p95 {:.3} ms  worst {:.3} ms",
        nearest_rank(&delays, 50) as f64 / 1000.0,
        nearest_rank(&delays, 95) as f64 / 1000.0,
        delays[observed - 1] as f64 / 1000.0,
    );
    Ok(0)
}

fn nearest_rank(sorted: &[i64], percentile: usize) -> i64 {
    sorted[(percentile * sorted.len()).div_ceil(100) - 1]
}

#[cfg(test)]
mod tests {
    use super::nearest_rank;

    #[test]
    fn percentiles_select_the_nearest_rank() {
        let values: Vec<_> = (1..=20).map(|value| value * 100).collect();
        assert_eq!(nearest_rank(&values, 50), 1_000);
        assert_eq!(nearest_rank(&values, 95), 1_900);
        assert_eq!(nearest_rank(&values, 100), 2_000);
        assert_eq!(nearest_rank(&[123], 95), 123);
    }
}
