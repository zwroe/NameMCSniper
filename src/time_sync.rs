use std::cmp::Reverse;
use std::collections::VecDeque;
use std::fmt::{self, Display};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant as MonotonicInstant};

use chrono::{DateTime, TimeDelta, Utc};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::net::{UdpSocket, lookup_host};
use tokio::time::{Instant, timeout_at};

const SERVERS: [&str; 3] = ["time.cloudflare.com", "time.windows.com", "time.nist.gov"];
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
// Reserve NIST's four-second minimum even if the previous batch used its full timeout.
const MIN_QUERY_SPACING: Duration = Duration::from_secs(6);
const MAX_ROUND_TRIP: Duration = Duration::from_millis(500);
const MAX_UPSTREAM_UNCERTAINTY: Duration = Duration::from_secs(2);
pub const MAX_OFFSET: TimeDelta = TimeDelta::minutes(5);
const MAX_DISAGREEMENT: TimeDelta = TimeDelta::milliseconds(50);
const MAX_LOCAL_CLOCK_STEP: TimeDelta = TimeDelta::milliseconds(100);
const HISTORY_CLOCK_STEP: TimeDelta = TimeDelta::milliseconds(5);
const MAX_HISTORY_AGE: Duration = Duration::from_secs(5 * 60);
const SAMPLES_PER_SOURCE: usize = 4;
const CLOCK_TOLERANCE: f64 = 15.0 / 1_000_000.0;
const HEADER_SIZE: usize = 48;
const NTP_EPOCH_OFFSET: i64 = 2_208_988_800;
const NTP_ERA_SECONDS: i64 = 1 << 32;

struct QueryState {
    started: Option<MonotonicInstant>,
    last_seen: Option<ClockReading>,
    accepted: Option<ClockSample>,
    history: [VecDeque<Observation>; SERVERS.len()],
}

impl QueryState {
    const fn new() -> Self {
        Self {
            started: None,
            last_seen: None,
            accepted: None,
            history: [const { VecDeque::new() }; SERVERS.len()],
        }
    }

    fn observe_clock(&mut self, now: ClockReading) {
        if self
            .last_seen
            .is_some_and(|last| !last.continuous_with(now, HISTORY_CLOCK_STEP))
        {
            self.forget_samples();
        }
        self.last_seen = Some(now);
        for history in &mut self.history {
            history.retain(|item| {
                now.tick.saturating_duration_since(item.at.tick) <= MAX_HISTORY_AGE
                    && item.at.continuous_with(now, HISTORY_CLOCK_STEP)
            });
        }
    }

    fn forget_samples(&mut self) {
        self.accepted = None;
        for history in &mut self.history {
            history.clear();
        }
    }

    fn calibrate(
        &mut self,
        fresh: &mut [Observation],
        checked: ClockReading,
    ) -> Result<ClockSample, ClockCheckError> {
        let replies = fresh.len();
        let agreement = agreeing_sources(fresh).ok_or(ClockCheckError::NoAgreement(replies))?;
        let current = summarize(agreement, checked)?;
        let mut selected = Vec::with_capacity(agreement.len());
        for &observation in agreement {
            let history = &mut self.history[observation.source];
            if history.len() == SAMPLES_PER_SOURCE {
                history.pop_front();
            }
            history.push_back(observation);
            let best = history
                .iter()
                .filter(|prior| {
                    (prior.measurement.offset - observation.measurement.offset).abs()
                        <= MAX_DISAGREEMENT
                })
                .min_by_key(|item| (item.measurement.round_trip, Reverse(item.at.tick)))
                .expect("the current observation is eligible");
            selected.push(*best);
        }

        let sample = agreeing_sources(&mut selected)
            .filter(|filtered| filtered.len() == agreement.len())
            .and_then(|filtered| summarize(filtered, checked).ok())
            .filter(|filtered| filtered.uncertainty <= current.uncertainty)
            .unwrap_or(current);
        self.accepted = Some(sample);
        Ok(sample)
    }
}

static LAST_QUERY: Mutex<QueryState> = Mutex::new(QueryState::new());

#[derive(Clone, Copy, Debug)]
pub struct ClockSample {
    pub offset: TimeDelta,
    pub round_trip: Duration,
    pub sources: usize,
    pub checked_at: MonotonicInstant,
    pub observed_at: MonotonicInstant,
    pub reference: DateTime<Utc>,
    pub disagreement: TimeDelta,
    pub uncertainty: Duration,
}

#[derive(Debug)]
pub enum ClockCheckError {
    TryAgain,
    LocalClockChanged,
    NoAgreement(usize),
    OffsetTooLarge(TimeDelta),
}

impl Display for ClockCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TryAgain => f.write_str("Wait six seconds between NTP checks."),
            Self::LocalClockChanged => {
                f.write_str("The system clock changed during the NTP check.")
            }
            Self::NoAgreement(0) => f.write_str("No usable replies from the NTP servers."),
            Self::NoAgreement(1) => f.write_str("Only one NTP server returned a usable reply."),
            Self::NoAgreement(replies) => write!(
                f,
                "{replies} NTP servers replied, but their offsets disagreed."
            ),
            Self::OffsetTooLarge(offset) => write!(
                f,
                "NTP sources agree on an offset of {:+.2} seconds. The app can correct up to {} seconds.",
                offset.num_milliseconds() as f64 / 1000.0,
                MAX_OFFSET.num_seconds()
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct Measurement {
    offset: TimeDelta,
    round_trip: Duration,
    upstream_uncertainty: Duration,
}

#[derive(Clone, Copy)]
struct ClockReading {
    wall: DateTime<Utc>,
    tick: MonotonicInstant,
}

impl ClockReading {
    fn now() -> Self {
        Self {
            wall: Utc::now(),
            tick: MonotonicInstant::now(),
        }
    }

    fn continuous_with(self, later: Self, tolerance: TimeDelta) -> bool {
        later
            .tick
            .checked_duration_since(self.tick)
            .and_then(|elapsed| TimeDelta::from_std(elapsed).ok())
            .and_then(|elapsed| self.wall.checked_add_signed(elapsed))
            .is_some_and(|expected| (later.wall - expected).abs() <= tolerance)
    }
}

#[derive(Clone, Copy)]
struct Observation {
    source: usize,
    at: ClockReading,
    measurement: Measurement,
}

pub async fn synchronize() -> Result<ClockSample, ClockCheckError> {
    let started = ClockReading::now();
    {
        let mut state = LAST_QUERY.lock().map_err(|_| ClockCheckError::TryAgain)?;
        state.observe_clock(started);
        if state
            .started
            .is_some_and(|last| started.tick.saturating_duration_since(last) < MIN_QUERY_SPACING)
        {
            return state
                .accepted
                .filter(|sample| {
                    ClockReading {
                        wall: sample.reference - sample.offset,
                        tick: sample.checked_at,
                    }
                    .continuous_with(started, HISTORY_CLOCK_STEP)
                })
                .ok_or(ClockCheckError::TryAgain);
        }
        state.started = Some(started.tick);
        state.accepted = None;
    }

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut queries: FuturesUnordered<_> = SERVERS
        .into_iter()
        .enumerate()
        .map(|(source, host)| async move {
            let measurement = query(host).await?;
            Some(Observation {
                source,
                at: ClockReading::now(),
                measurement,
            })
        })
        .collect();
    let mut fresh = Vec::with_capacity(SERVERS.len());
    while let Ok(Some(observation)) = timeout_at(deadline, queries.next()).await {
        if let Some(observation) = observation {
            fresh.push(observation);
        }
    }
    let checked = ClockReading::now();
    let mut state = LAST_QUERY.lock().map_err(|_| ClockCheckError::TryAgain)?;
    if state.started != Some(started.tick) {
        return Err(ClockCheckError::TryAgain);
    }
    state.observe_clock(checked);
    if !started.continuous_with(checked, HISTORY_CLOCK_STEP) {
        state.forget_samples();
        return Err(ClockCheckError::LocalClockChanged);
    }
    state.calibrate(&mut fresh, checked)
}

async fn query(host: &str) -> Option<Measurement> {
    let mut addresses = lookup_host((host, 123)).await.ok()?;
    let first = addresses.next()?;
    let server = if first.is_ipv4() {
        first
    } else {
        addresses.find(SocketAddr::is_ipv4).unwrap_or(first)
    };
    exchange(server).await
}

async fn exchange(server: SocketAddr) -> Option<Measurement> {
    let bind_address = if server.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = UdpSocket::bind(bind_address).await.ok()?;
    socket.connect(server).await.ok()?;

    let mut request = [0; HEADER_SIZE];
    request[0] = (4 << 3) | 3;
    let started = MonotonicInstant::now();
    let local_sent = Utc::now();
    let transmit = encode_timestamp(local_sent);
    request[40..48].copy_from_slice(&transmit);
    if socket.send(&request).await.ok()? != request.len() {
        return None;
    }

    let mut response = [0; 512];
    let length = socket.recv(&mut response).await.ok()?;
    let elapsed = started.elapsed();
    let local_received = local_sent.checked_add_signed(TimeDelta::from_std(elapsed).ok()?)?;
    if (Utc::now() - local_received).abs() > MAX_LOCAL_CLOCK_STEP {
        return None;
    }

    parse_response(&response[..length], transmit, local_sent, local_received)
}

fn parse_response(
    response: &[u8],
    originate: [u8; 8],
    local_sent: DateTime<Utc>,
    local_received: DateTime<Utc>,
) -> Option<Measurement> {
    let header = response.get(..HEADER_SIZE)?;
    let leap = header[0] >> 6;
    let version = (header[0] >> 3) & 7;
    let mode = header[0] & 7;
    if leap == 3
        || !matches!(version, 3 | 4)
        || mode != 4
        || !(1..=15).contains(&header[1])
        || header[24..32] != originate
    {
        return None;
    }

    let server_received = decode_timestamp(header[32..40].try_into().ok()?, local_sent)?;
    let server_sent = decode_timestamp(header[40..48].try_into().ok()?, local_sent)?;
    let elapsed = (local_received - local_sent).to_std().ok()?;
    let processing = (server_sent - server_received).to_std().ok()?;
    let round_trip = elapsed.checked_sub(processing)?;
    let root_delay = i32::from_be_bytes(header[4..8].try_into().ok()?).max(0) as u32;
    let root_dispersion = u32::from_be_bytes(header[8..12].try_into().ok()?);
    let upstream_uncertainty = fixed_duration(root_delay) / 2 + fixed_duration(root_dispersion);

    // RFC 5905 computes the offset from the two directions of the exchange.
    let offset = ((server_received - local_sent) + (server_sent - local_received)) / 2;
    if round_trip > MAX_ROUND_TRIP || upstream_uncertainty > MAX_UPSTREAM_UNCERTAINTY {
        return None;
    }
    Some(Measurement {
        offset,
        round_trip,
        upstream_uncertainty,
    })
}

fn fixed_duration(raw: u32) -> Duration {
    Duration::from_nanos((u64::from(raw) * 1_000_000_000).div_ceil(1 << 16))
}

fn encode_timestamp(time: DateTime<Utc>) -> [u8; 8] {
    let seconds = (time.timestamp() + NTP_EPOCH_OFFSET).rem_euclid(NTP_ERA_SECONDS) as u32;
    let fraction = ((u64::from(time.timestamp_subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut stamp = [0; 8];
    stamp[..4].copy_from_slice(&seconds.to_be_bytes());
    stamp[4..].copy_from_slice(&fraction.to_be_bytes());
    stamp
}

fn decode_timestamp(stamp: [u8; 8], reference: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if stamp == [0; 8] {
        return None;
    }
    let seconds = i64::from(u32::from_be_bytes(stamp[..4].try_into().ok()?));
    let fraction = u64::from(u32::from_be_bytes(stamp[4..].try_into().ok()?));
    let reference_seconds = reference.timestamp() + NTP_EPOCH_OFFSET;
    let era = (reference_seconds - seconds + NTP_ERA_SECONDS / 2).div_euclid(NTP_ERA_SECONDS);
    let unix_seconds = era * NTP_ERA_SECONDS + seconds - NTP_EPOCH_OFFSET;
    let nanos = ((fraction * 1_000_000_000) >> 32) as u32;
    DateTime::from_timestamp(unix_seconds, nanos)
}

fn agreeing_sources(observations: &mut [Observation]) -> Option<&[Observation]> {
    let spread = |items: &[Observation]| {
        items[items.len() - 1].measurement.offset - items[0].measurement.offset
    };
    observations.sort_unstable_by_key(|item| item.measurement.offset);
    if observations.len() == SERVERS.len() && spread(observations) <= MAX_DISAGREEMENT {
        return Some(observations);
    }

    observations
        .windows(2)
        .min_by_key(|pair| spread(pair))
        .filter(|pair| spread(pair) <= MAX_DISAGREEMENT)
}

fn summarize(
    agreement: &[Observation],
    checked: ClockReading,
) -> Result<ClockSample, ClockCheckError> {
    let first = agreement[0].measurement;
    let last = agreement[agreement.len() - 1].measurement;
    let disagreement = last.offset - first.offset;
    let (offset, round_trip) = if agreement.len() == 3 {
        let middle = agreement[1].measurement;
        (middle.offset, middle.round_trip)
    } else {
        (
            first.offset + disagreement / 2,
            first.round_trip.max(last.round_trip),
        )
    };
    if offset.abs() > MAX_OFFSET {
        return Err(ClockCheckError::OffsetTooLarge(offset));
    }

    let observed_at = agreement.iter().map(|item| item.at.tick).min().unwrap();
    let path_uncertainty = agreement
        .iter()
        .map(|item| item.measurement.round_trip / 2 + item.measurement.upstream_uncertainty)
        .max()
        .unwrap();
    let age = checked.tick.saturating_duration_since(observed_at);
    let uncertainty = path_uncertainty
        + disagreement
            .to_std()
            .expect("sorted observations have nonnegative spread")
        + age.mul_f64(CLOCK_TOLERANCE);
    Ok(ClockSample {
        offset,
        round_trip,
        sources: agreement.len(),
        checked_at: checked.tick,
        observed_at,
        reference: checked.wall + offset,
        disagreement,
        uncertainty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn reading() -> ClockReading {
        ClockReading {
            wall: timestamp(),
            tick: MonotonicInstant::now(),
        }
    }

    fn observation(source: usize, at: ClockReading, offset: i64, rtt: u64) -> Observation {
        Observation {
            source,
            at,
            measurement: Measurement {
                offset: TimeDelta::milliseconds(offset),
                round_trip: Duration::from_millis(rtt),
                upstream_uncertainty: Duration::ZERO,
            },
        }
    }

    fn response(
        local_sent: DateTime<Utc>,
        server_received: DateTime<Utc>,
        server_sent: DateTime<Utc>,
    ) -> [u8; HEADER_SIZE] {
        let mut bytes = [0; HEADER_SIZE];
        bytes[0] = (4 << 3) | 4;
        bytes[1] = 2;
        bytes[24..32].copy_from_slice(&encode_timestamp(local_sent));
        bytes[32..40].copy_from_slice(&encode_timestamp(server_received));
        bytes[40..48].copy_from_slice(&encode_timestamp(server_sent));
        bytes
    }

    #[tokio::test]
    async fn udp_exchange_accepts_a_reply_for_its_own_request() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let offset = TimeDelta::milliseconds(500);
        let reply = async {
            let mut request = [0; 512];
            let (length, peer) = server.recv_from(&mut request).await.unwrap();
            assert_eq!(length, HEADER_SIZE);
            assert_eq!(request[0], (4 << 3) | 3);
            let originate = request[40..48].try_into().unwrap();
            let local_sent = decode_timestamp(originate, Utc::now()).unwrap();
            let server_time = local_sent + offset;
            let mut reply = response(local_sent, server_time, server_time);
            reply[24..32].copy_from_slice(&originate);
            assert_eq!(server.send_to(&reply, peer).await.unwrap(), HEADER_SIZE);
        };

        let (measurement, ()) = tokio::time::timeout(QUERY_TIMEOUT, async {
            tokio::join!(exchange(address), reply)
        })
        .await
        .unwrap();
        let measurement = measurement.unwrap();
        let elapsed = TimeDelta::from_std(measurement.round_trip).unwrap();
        let expected = offset - elapsed / 2;
        assert!((measurement.offset - expected).abs() <= TimeDelta::nanoseconds(4));
    }

    #[test]
    fn computes_offset_and_network_round_trip() {
        let local_sent = timestamp();
        let server_received = local_sent + TimeDelta::milliseconds(375);
        let server_sent = server_received + TimeDelta::milliseconds(125);
        let local_received = local_sent + TimeDelta::milliseconds(375);
        let mut bytes = response(local_sent, server_received, server_sent);
        bytes[4..8].copy_from_slice(&16_384_i32.to_be_bytes());
        bytes[8..12].copy_from_slice(&8_192_u32.to_be_bytes());
        let measurement = parse_response(
            &bytes,
            encode_timestamp(local_sent),
            local_sent,
            local_received,
        )
        .unwrap();
        assert_eq!(measurement.offset, TimeDelta::milliseconds(250));
        assert_eq!(measurement.round_trip, Duration::from_millis(250));
        assert_eq!(measurement.upstream_uncertainty, Duration::from_millis(250));
    }

    #[test]
    fn rejects_bad_headers_originate_and_timestamps() {
        let sent = timestamp();
        let received = sent + TimeDelta::milliseconds(125);
        let reply = response(sent, sent, sent);
        let parse = |bytes: &[u8]| parse_response(bytes, encode_timestamp(sent), sent, received);
        assert!(parse(&reply).is_some());
        assert!(parse(&reply[..47]).is_none());
        for (index, byte) in [
            (0, 0xe4),
            (0, 0x14),
            (0, 0x23),
            (1, 0),
            (1, 16),
            (24, reply[24] ^ 1),
        ] {
            let mut invalid = reply;
            invalid[index] = byte;
            assert!(parse(&invalid).is_none());
        }
        let mut invalid = reply;
        invalid[32..40].fill(0);
        assert!(parse(&invalid).is_none());
        let mut invalid = reply;
        invalid[8..12].copy_from_slice(&(3_u32 << 16).to_be_bytes());
        assert!(parse(&invalid).is_none());
        assert!(parse(&response(sent, sent, sent + TimeDelta::seconds(1))).is_none());
        assert!(parse(&response(sent, received, sent)).is_none());
        assert!(
            parse_response(
                &reply,
                encode_timestamp(sent),
                sent,
                sent + TimeDelta::seconds(1)
            )
            .is_none()
        );
    }

    #[test]
    fn decodes_timestamps_across_the_2036_era_rollover() {
        let rollover = NTP_ERA_SECONDS - NTP_EPOCH_OFFSET;
        let before = DateTime::from_timestamp(rollover - 1, 875_000_000).unwrap();
        let after = DateTime::from_timestamp(rollover, 125_000_000).unwrap();
        assert_eq!(
            decode_timestamp(encode_timestamp(after), before),
            Some(after)
        );
        assert_eq!(
            decode_timestamp(encode_timestamp(before), after),
            Some(before)
        );
    }

    #[test]
    fn requires_two_agreeing_sources_and_rejects_an_outlier() {
        let at = reading();
        let sample = |source, offset| observation(source, at, offset, 20);
        let mut state = QueryState::new();
        assert!(matches!(
            state.calibrate(&mut [sample(0, 10)], at),
            Err(ClockCheckError::NoAgreement(1))
        ));
        assert!(matches!(
            state.calibrate(&mut [sample(0, 100), sample(1, -100)], at),
            Err(ClockCheckError::NoAgreement(2))
        ));
        let result = state
            .calibrate(&mut [sample(0, 810), sample(1, 30), sample(2, 10)], at)
            .unwrap();
        assert_eq!(result.sources, 2);
        assert_eq!(result.offset, TimeDelta::milliseconds(20));
        let result = state
            .calibrate(&mut [sample(0, 0), sample(1, 30), sample(2, 10)], at)
            .unwrap();
        assert_eq!(result.sources, 3);
        assert_eq!(result.offset, TimeDelta::milliseconds(10));
    }

    #[test]
    fn accepts_agreed_skew_beyond_thirty_seconds_and_reports_over_limit() {
        let at = reading();
        let sample = |source, offset| observation(source, at, offset, 30);
        let mut state = QueryState::new();
        let mut matching = [sample(0, 47_940), sample(1, 47_944), sample(2, 47_941)];
        let calibrated = state.calibrate(&mut matching, at).unwrap();
        assert_eq!(calibrated.offset, TimeDelta::milliseconds(47_941));

        let mut outside_limit = [sample(0, 301_000), sample(1, 301_001)];
        assert!(matches!(
            state.calibrate(&mut outside_limit, at),
            Err(ClockCheckError::OffsetTooLarge(_))
        ));
    }

    #[test]
    fn prefers_recent_low_delay_samples_after_a_fresh_agreement() {
        let old = reading();
        let now = ClockReading {
            wall: old.wall + TimeDelta::minutes(1),
            tick: old.tick + Duration::from_secs(60),
        };
        let mut state = QueryState::new();
        state.observe_clock(old);
        state
            .calibrate(
                &mut [observation(0, old, 20, 10), observation(1, old, 22, 12)],
                old,
            )
            .unwrap();
        state.observe_clock(now);
        let mut fresh = [observation(0, now, 24, 40), observation(1, now, 26, 60)];
        let calibrated = state.calibrate(&mut fresh, now).unwrap();
        assert_eq!(calibrated.offset, TimeDelta::milliseconds(21));
        assert_eq!(calibrated.round_trip, Duration::from_millis(12));
        assert_eq!(calibrated.disagreement, TimeDelta::milliseconds(2));
        assert_eq!(calibrated.uncertainty, Duration::from_micros(8_900));
        assert_eq!(calibrated.checked_at, now.tick);
        assert_eq!(calibrated.observed_at, old.tick);
        assert_eq!(calibrated.reference, now.wall + calibrated.offset);
        assert!(matches!(
            state.calibrate(&mut [fresh[0]], now),
            Err(ClockCheckError::NoAgreement(1))
        ));
        assert!(matches!(
            state.calibrate(&mut [fresh[0], observation(1, now, 150, 50)], now),
            Err(ClockCheckError::NoAgreement(2))
        ));
    }

    #[test]
    fn drops_history_after_clock_discontinuity_or_expiration() {
        let old = reading();
        let mut state = QueryState::new();
        state.observe_clock(old);
        state
            .calibrate(
                &mut [observation(0, old, 20, 10), observation(1, old, 22, 12)],
                old,
            )
            .unwrap();
        let later = ClockReading {
            wall: old.wall + TimeDelta::seconds(60),
            tick: old.tick + Duration::from_secs(10),
        };
        state.observe_clock(later);
        assert!(state.history.iter().all(VecDeque::is_empty));
        assert!(state.accepted.is_none());
        state
            .calibrate(
                &mut [observation(0, later, 20, 10), observation(1, later, 22, 12)],
                later,
            )
            .unwrap();
        state.observe_clock(ClockReading {
            wall: later.wall + TimeDelta::minutes(46),
            tick: later.tick + Duration::from_secs(46 * 60),
        });
        assert!(state.history.iter().all(VecDeque::is_empty));
    }
}
