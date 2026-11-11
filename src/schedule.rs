use std::time::Duration;

use anyhow::{Result, ensure};
use chrono::{DateTime, TimeDelta, Utc};
use tokio::time::{Instant, sleep, sleep_until};

use crate::{drops::DropWindow, minecraft::ClaimOutcome};

const SLOT_GRACE: TimeDelta = TimeDelta::seconds(1);
const MAX_RANGE: TimeDelta = TimeDelta::seconds(30);
pub const NAMEMC_MIN_SPACING: TimeDelta = TimeDelta::seconds(10);
const WALL_CLOCK_POLL: Duration = Duration::from_secs(30);
const WAKE_TOLERANCE: Duration = Duration::from_secs(2);
const PRECISE_WAIT: Duration = Duration::from_millis(25);
const SPIN_TAIL: Duration = Duration::from_micros(400);

#[derive(Clone, Copy, Default)]
pub struct Clock {
    anchor: Option<(DateTime<Utc>, Instant)>,
}

impl Clock {
    pub fn anchored(offset: TimeDelta) -> Self {
        Self::from_reference(Utc::now() + offset, std::time::Instant::now())
    }

    pub fn from_reference(reference: DateTime<Utc>, observed: std::time::Instant) -> Self {
        Self {
            anchor: Some((reference, Instant::from_std(observed))),
        }
    }

    pub fn now(self) -> DateTime<Utc> {
        self.at(Instant::now())
    }

    fn at(self, now: Instant) -> DateTime<Utc> {
        match self.anchor {
            Some((utc, tick)) => {
                utc + TimeDelta::from_std(now.saturating_duration_since(tick))
                    .expect("elapsed time fits in TimeDelta")
            }
            None => Utc::now(),
        }
    }

    fn deadline(self, slot: DateTime<Utc>) -> Option<Instant> {
        let (reference, tick) = self.anchor?;
        let offset = slot - reference;
        if let Ok(after) = offset.to_std() {
            tick.checked_add(after)
        } else {
            tick.checked_sub((-offset).to_std().ok()?)
        }
    }
}

pub struct Plan {
    pub slots: Vec<DateTime<Utc>>,
    recheck_each_slot: bool,
}

impl Plan {
    pub fn at(time: DateTime<Utc>) -> Self {
        Self {
            slots: vec![time],
            recheck_each_slot: false,
        }
    }

    pub fn new(window: &DropWindow, bounded: bool, budget: u8, interval: u8) -> Result<Self> {
        let spacing = TimeDelta::seconds(i64::from(interval));
        let slots = if bounded {
            let width = window.end - window.start;
            ensure!(
                width <= MAX_RANGE,
                "The window is wider than 30 seconds. Pick a time with --at."
            );
            let width_ms = width.num_milliseconds();
            let count = i64::from(budget).min(1 + width_ms / spacing.num_milliseconds());
            if count == 1 {
                vec![window.end]
            } else {
                let width_ns = width.num_nanoseconds().expect("range fits in 30 seconds");
                (0..count)
                    .map(|index| {
                        window.start + TimeDelta::nanoseconds(width_ns * index / (count - 1))
                    })
                    .collect()
            }
        } else {
            (0..budget)
                .map(|index| {
                    window
                        .start
                        .checked_add_signed(spacing * i32::from(index))
                        .ok_or_else(|| {
                            anyhow::anyhow!("An attempt time is outside the supported date range.")
                        })
                })
                .collect::<Result<_>>()?
        };
        Ok(Self {
            slots,
            recheck_each_slot: false,
        })
    }

    pub fn from_namemc(window: DropWindow, now: DateTime<Utc>) -> Result<Self> {
        ensure!(
            window.end > now,
            "The NameMC drop window has already ended."
        );
        let mut slots = Vec::with_capacity(2);
        if window.start > now && window.end - window.start >= NAMEMC_MIN_SPACING {
            slots.push(window.start);
        }
        slots.push(window.end);
        Ok(Self {
            recheck_each_slot: slots.len() > 1,
            slots,
        })
    }

    pub fn preflight_batches(&self) -> impl Iterator<Item = &[DateTime<Utc>]> {
        let size = if self.recheck_each_slot {
            1
        } else {
            self.slots.len()
        };
        self.slots.chunks(size)
    }

    pub fn can_start(&self, now: DateTime<Utc>) -> bool {
        slot_is_open(*self.slots.last().expect("plan has at least one slot"), now)
    }
}

pub fn slot_is_open(slot: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now - slot <= SLOT_GRACE
}

pub struct Attempt {
    pub slot: DateTime<Utc>,
    pub sent: DateTime<Utc>,
    pub elapsed: Duration,
    pub outcome: ClaimOutcome,
}

pub async fn execute(
    slots: &[DateTime<Utc>],
    mut wait_slot: impl AsyncFnMut(DateTime<Utc>) -> Option<DateTime<Utc>>,
    mut claim: impl AsyncFnMut() -> ClaimOutcome,
) -> Vec<Attempt> {
    let mut attempts = Vec::with_capacity(slots.len());
    for &slot in slots {
        let Some(sent) = wait_slot(slot).await else {
            continue;
        };
        let started = Instant::now();
        let outcome = claim().await;
        let may_continue = matches!(outcome, ClaimOutcome::Unavailable);
        attempts.push(Attempt {
            slot,
            sent,
            elapsed: started.elapsed(),
            outcome,
        });
        if !may_continue {
            break;
        }
    }
    attempts
}

pub async fn countdown_until(
    clock: Clock,
    target: DateTime<Utc>,
    until: DateTime<Utc>,
    mut clock_is_valid: impl FnMut() -> bool,
) -> bool {
    let mut next_update = Instant::now();
    loop {
        if !clock_is_valid() {
            return false;
        }
        let now = clock.now();
        let Ok(wait) = (until - now).to_std() else {
            return true;
        };
        if wait.is_zero() {
            return true;
        }
        let tick = Instant::now();
        if tick >= next_update {
            let remaining = (target - now).to_std().unwrap_or_default();
            let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
            let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
            println!("Next attempt in {hours:02}:{minutes:02}:{seconds:02}");
            next_update = tick + countdown_interval(remaining);
        }
        let wake = tick + wait.min(next_update - tick).min(WALL_CLOCK_POLL);
        sleep_until(wake).await;
        if wake.elapsed() > WAKE_TOLERANCE {
            return false;
        }
    }
}

fn countdown_interval(remaining: Duration) -> Duration {
    let hour = Duration::from_secs(3600);
    let minute = Duration::from_secs(60);
    if remaining > hour {
        (remaining - hour)
            .min(Duration::from_secs(300))
            .max(Duration::from_secs(1))
    } else if remaining > minute {
        (remaining - minute).min(minute).max(Duration::from_secs(1))
    } else {
        Duration::from_secs(1)
    }
}

pub struct LiveSlots {
    clock: Clock,
    min_spacing: Duration,
    next_send: Option<Instant>,
}

impl LiveSlots {
    pub fn new(min_spacing: Duration, clock: Clock) -> Self {
        Self {
            clock,
            min_spacing,
            next_send: None,
        }
    }

    pub fn use_clock(&mut self, clock: Clock) {
        self.clock = clock;
    }

    pub async fn wait(&mut self, slot: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let deadline = self.clock.deadline(slot);
        loop {
            let tick = Instant::now();
            let now = self.clock.at(tick);
            let spacing = self
                .next_send
                .map(|next_send| next_send.saturating_duration_since(tick))
                .unwrap_or_default();
            let delay = dispatch_delay(slot - now, spacing)?;
            if delay.is_zero() {
                self.next_send = Some(tick + self.min_spacing);
                return Some(now);
            }
            if delay > PRECISE_WAIT {
                sleep((delay - PRECISE_WAIT).min(WALL_CLOCK_POLL)).await;
            } else {
                let deadline = deadline.map_or(tick + delay, |slot| slot.max(tick + spacing));
                // The final wait stays off the async thread so Ctrl+C can cancel a live run.
                tokio::task::spawn_blocking(move || finish_at(deadline))
                    .await
                    .ok()?;
            }
        }
    }
}

fn dispatch_delay(wall_delay: TimeDelta, spacing: Duration) -> Option<Duration> {
    let until_grace_ends = (SLOT_GRACE + wall_delay).to_std().ok()?;
    if spacing > until_grace_ends {
        return None;
    }
    Some(wall_delay.to_std().unwrap_or_default().max(spacing))
}

fn finish_at(deadline: Instant) {
    let sleep_for = deadline
        .saturating_duration_since(Instant::now())
        .saturating_sub(SPIN_TAIL);
    if !sleep_for.is_zero() {
        std::thread::sleep(sleep_for);
    }
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    #[test]
    fn countdown_updates_more_often_as_the_attempt_approaches() {
        assert_eq!(
            countdown_interval(Duration::from_secs(4000)),
            Duration::from_secs(300)
        );
        assert_eq!(
            countdown_interval(Duration::from_secs(3630)),
            Duration::from_secs(30)
        );
        assert_eq!(
            countdown_interval(Duration::from_secs(63)),
            Duration::from_secs(3)
        );
        assert_eq!(
            countdown_interval(Duration::from_secs(30)),
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn a_corrected_clock_is_used_for_dispatch() {
        let clock = Clock::anchored(TimeDelta::seconds(5));
        let slot = clock.now();
        let mut dispatch = LiveSlots::new(Duration::ZERO, clock);
        let stamp = dispatch.wait(slot).await.unwrap();
        assert!(stamp >= slot);
        assert!(stamp > Utc::now() + TimeDelta::seconds(3));
    }

    #[test]
    fn a_dispatch_must_satisfy_the_wall_time_spacing_and_grace() {
        assert_eq!(
            dispatch_delay(TimeDelta::milliseconds(15), Duration::from_millis(10)),
            Some(Duration::from_millis(15))
        );
        assert_eq!(
            dispatch_delay(TimeDelta::milliseconds(-15), Duration::from_millis(10)),
            Some(Duration::from_millis(10))
        );
        assert_eq!(
            dispatch_delay(-SLOT_GRACE, Duration::ZERO),
            Some(Duration::ZERO)
        );
        assert_eq!(dispatch_delay(-SLOT_GRACE, Duration::from_micros(1)), None);
        assert_eq!(
            dispatch_delay(-SLOT_GRACE - TimeDelta::milliseconds(1), Duration::ZERO),
            None
        );
    }

    #[test]
    fn bounded_plan_covers_both_bounds_with_minimum_spacing() {
        let end = start() + TimeDelta::milliseconds(25_321);
        let window = DropWindow::manual(start(), Some(end)).unwrap();
        let plan = Plan::new(&window, true, 3, 10).unwrap();
        assert_eq!(
            plan.slots,
            [start(), start() + TimeDelta::microseconds(12_660_500), end]
        );
        assert_eq!(Plan::new(&window, true, 1, 10).unwrap().slots, [end]);
        assert_eq!(Plan::new(&window, true, 3, 30).unwrap().slots, [end]);
    }

    #[test]
    fn an_exact_time_allows_retries_and_a_wide_range_needs_an_explicit_time() {
        let end = start() + TimeDelta::seconds(31);
        let range = DropWindow::manual(start(), Some(end)).unwrap();
        assert!(Plan::new(&range, true, 3, 10).is_err());

        let exact = DropWindow::manual(start(), None).unwrap();
        let plan = Plan::new(&exact, false, 3, 10).unwrap();
        assert_eq!(
            plan.slots,
            [
                start(),
                start() + TimeDelta::seconds(10),
                start() + TimeDelta::seconds(20)
            ]
        );
        assert!(plan.can_start(start() + TimeDelta::seconds(21)));
        assert!(!plan.can_start(start() + TimeDelta::milliseconds(21_001)));
    }

    #[test]
    fn namemc_bounds_use_separate_preflights_and_keep_only_usable_slots() {
        let latest = start() + TimeDelta::days(2);
        let window = DropWindow::manual(start(), Some(latest)).unwrap();
        let now = start() - TimeDelta::minutes(1);
        let plan = Plan::from_namemc(window, now).unwrap();
        assert_eq!(plan.slots, [start(), latest]);
        assert_eq!(
            plan.preflight_batches().collect::<Vec<_>>(),
            [&[start()][..], &[latest][..]]
        );
        assert_eq!(Plan::from_namemc(window, start()).unwrap().slots, [latest]);
        assert!(Plan::from_namemc(window, latest).is_err());

        let nearby = start() + TimeDelta::seconds(9);
        let narrow = DropWindow::manual(start(), Some(nearby)).unwrap();
        assert_eq!(Plan::from_namemc(narrow, now).unwrap().slots, [nearby]);
    }

    #[tokio::test]
    async fn skips_missed_slots_and_stops_at_rate_limit() {
        let window = DropWindow::manual(start(), None).unwrap();
        let plan = Plan::new(&window, false, 3, 10).unwrap();
        let attempts = execute(
            &plan.slots,
            async |slot| (slot != start()).then_some(slot),
            async || ClaimOutcome::RateLimited(Some(Duration::from_secs(30))),
        )
        .await;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].slot, plan.slots[1]);
    }

    #[tokio::test]
    async fn retries_unavailable_but_stops_at_uncertain_result() {
        let window = DropWindow::manual(start(), None).unwrap();
        let plan = Plan::new(&window, false, 3, 10).unwrap();
        let mut replies = [
            ClaimOutcome::Unavailable,
            ClaimOutcome::Uncertain("lost response".into()),
        ]
        .into_iter();
        let attempts = execute(
            &plan.slots,
            async |slot| Some(slot),
            async || replies.next().expect("no third request"),
        )
        .await;
        assert_eq!(attempts.len(), 2);
    }
}
