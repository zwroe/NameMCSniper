mod accounts;
mod benchmark;
mod calibrated_clock;
mod claim_group;
mod drops;
mod menu;
mod minecraft;
mod proxies;
mod run_log;
mod schedule;
mod time_sync;

use std::{
    cell::Cell,
    fmt::Display,
    io::{self, BufRead, IsTerminal, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use clap::{ArgGroup, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use reqwest::{Client, ClientBuilder, redirect::Policy, tls::Version};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    accounts::MAX_TOKEN_ENTRIES,
    calibrated_clock::CalibratedClock,
    claim_group::LiveAccount,
    drops::DropWindow,
    minecraft::{ClaimOutcome, Minecraft, ReadRateLimit, valid_username},
    run_log::RunLog,
    schedule::{Attempt, Clock, LiveSlots, Plan},
    time_sync::{ClockCheckError, ClockSample},
};

const TOKEN_ENV: &str = "NAMEMCSNIPER_TOKEN";
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_PREFLIGHT_LEAD: Duration = Duration::from_secs(20);
const PREFLIGHT_MARGIN: Duration = Duration::from_secs(5);
const CLOCK_CHECK_HEADROOM: TimeDelta = TimeDelta::seconds(13);
const CLOCK_REFRESH_INTERVAL: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
enum Timing {
    Absolute(Option<CalibratedClock>),
    Relative(Clock),
}

impl Timing {
    fn current(self) -> Result<Clock> {
        match self {
            Self::Relative(clock) => Ok(clock),
            Self::Absolute(Some(calibration)) => Ok(calibration.current()?.clock),
            Self::Absolute(None) => bail!("UTC has not been verified."),
        }
    }
}

#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "Open without arguments for the guided menu."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the guided terminal menu
    Menu,
    /// Show a 3NAME estimate or normalize a manually supplied time
    Drop {
        #[arg(value_parser = parse_name)]
        name: String,
        #[command(flatten)]
        times: Times,
    },
    /// Preview a schedule offline, or explicitly attempt a live name change
    #[command(
        alias = "snipe-at",
        group(ArgGroup::new("live_source").args(["account", "tokens_file"]).multiple(true))
    )]
    Snipe {
        #[arg(value_parser = parse_name)]
        name: String,
        #[command(flatten)]
        times: Times,
        /// Maximum requests to send during the run
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=3))]
        attempts: u8,
        /// Minimum seconds between request starts
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u8).range(10..=30))]
        interval: u8,
        /// Authorize a real rename on the specified Java profile
        #[arg(long, requires = "live_source")]
        live: bool,
        /// Java profile UUID. Repeat to select specific profiles from --tokens-file
        #[arg(long, requires = "live")]
        account: Vec<Uuid>,
        /// Use all eligible profiles from a private token list unless --account is supplied
        #[arg(
            long,
            requires = "live",
            conflicts_with = "token_stdin",
            value_name = "FILE"
        )]
        tokens_file: Option<PathBuf>,
        /// Read a Minecraft Services bearer token from stdin
        #[arg(long, requires = "live")]
        token_stdin: bool,
    },
    /// Check your Java profile UUID and name-change eligibility
    #[command(alias = "check-accounts")]
    Check {
        /// Read a Minecraft Services bearer token from stdin
        #[arg(long)]
        token_stdin: bool,
    },
    /// Validate bearer tokens and report Java profile eligibility
    #[command(alias = "test-tokens")]
    CheckTokens {
        /// Token list file (defaults to tokens.txt in the working or executable folder)
        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// Test HTTP(S) proxies for connectivity to Minecraft Services
    TestProxies {
        /// Text file with one proxy per line
        #[arg(value_name = "FILE", default_value = "proxies.txt")]
        file: PathBuf,
        /// Save the working entries to a new file
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Compare the system clock with public UTC time sources
    ClockCheck,
    /// Measure dispatch timing locally with simulated requests
    Benchmark {
        /// Number of scheduled samples
        #[arg(long, default_value_t = benchmark::DEFAULT_SAMPLES, value_parser = clap::value_parser!(u16).range(1..=200))]
        samples: u16,
    },
    /// Run the attempt logic against a scripted outcome, entirely offline
    Simulate {
        #[arg(long, value_enum, default_value_t = Scenario::Success)]
        scenario: Scenario,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Scenario {
    Success,
    Unavailable,
    RateLimited,
    Uncertain,
    Rejected,
}

#[derive(Args)]
struct Times {
    /// Target time as RFC 3339 with an offset, or Unix milliseconds
    #[arg(long, value_parser = parse_time, value_name = "TIME")]
    at: Option<DateTime<Utc>>,
    /// Latest planned time, with up to 1s of timer grace. Requires --at
    #[arg(long, requires = "at", value_parser = parse_time, value_name = "TIME")]
    until: Option<DateTime<Utc>>,
}

fn parse_name(name: &str) -> Result<String> {
    ensure!(
        valid_username(name),
        "A Java username must have 3–16 letters, digits, or underscores."
    );
    Ok(name.to_owned())
}

fn parse_time(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(millis) = value.parse::<i64>() {
        ensure!(
            value.trim_start_matches('-').len() >= 12,
            "Pass Unix milliseconds or a timestamp with an explicit offset, such as 2027-02-01T18:30:00-05:00."
        );
        return DateTime::from_timestamp_millis(millis)
            .context("Unix milliseconds are out of range.");
    }
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.to_utc())
        .map_err(|_| anyhow::anyhow!("Use RFC 3339 with an explicit offset, such as 2027-02-01T18:30:00-05:00, or Unix milliseconds."))
}

fn http_client_builder() -> ClientBuilder {
    Client::builder()
        .https_only(true)
        .min_tls_version(Version::TLS_1_2)
        .no_proxy()
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .user_agent(concat!("NameMCSniper/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(3))
        .timeout(HTTP_REQUEST_TIMEOUT)
}

fn http_client() -> Result<Client> {
    http_client_builder()
        .build()
        .context("Could not initialize the HTTPS client.")
}

fn read_token(token_stdin: bool) -> Result<String> {
    if token_stdin {
        let mut token = Zeroizing::new(String::new());
        io::stdin().lock().take(8193).read_line(&mut token)?;
        ensure!(token.len() <= 8192, "The bearer token is too long.");
        return Ok(std::mem::take(&mut *token));
    }
    match std::env::var(TOKEN_ENV) {
        Ok(token) => return Ok(token),
        Err(std::env::VarError::NotPresent) => {}
        Err(_) => bail!("{TOKEN_ENV} must be valid text."),
    }
    ensure!(
        io::stdin().is_terminal() && io::stderr().is_terminal(),
        "Supply a Minecraft Services bearer token through {TOKEN_ENV} or --token-stdin."
    );
    rpassword::prompt_password("Minecraft Services bearer token (hidden): ")
        .context("Could not read the bearer token.")
}

fn utc(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn show_window(name: &str, window: &DropWindow, source: &str) {
    println!("{name}  ({source})");
    let start = if window.in_progress {
        "Observed"
    } else {
        "Earliest"
    };
    println!("  {start:8} {}", utc(window.start));
    if window.start != window.end {
        println!("  Latest   {}", utc(window.end));
        let width = (window.end - window.start).num_milliseconds();
        let hours = width / 3_600_000;
        let minutes = width / 60_000 % 60;
        let seconds = width / 1_000 % 60;
        let millis = width % 1_000;
        println!("  Width    {hours}h {minutes:02}m {seconds:02}.{millis:03}s");
    }
}

fn show_plan(plan: &Plan) {
    for (index, time) in plan.slots.iter().enumerate() {
        println!("  Attempt {}  {}", index + 1, utc(*time));
    }
}

fn outcome_message(outcome: &ClaimOutcome, simulated: bool) -> String {
    match outcome {
        ClaimOutcome::Claimed => "Name changed.".to_owned(),
        ClaimOutcome::Unavailable => "Name unavailable.".to_owned(),
        ClaimOutcome::RateLimited(retry_after) => {
            let wait = retry_after
                .map(|duration| {
                    let seconds = duration
                        .as_secs()
                        .saturating_add(u64::from(duration.subsec_nanos() != 0));
                    format!(" Retry-After is {seconds} seconds.")
                })
                .unwrap_or_default();
            format!("Minecraft rate limited the request. Stopping.{wait}")
        }
        ClaimOutcome::Rejected(reason) => format!("Minecraft rejected the request. {reason}"),
        ClaimOutcome::Uncertain(reason) => {
            let follow_up = if simulated {
                ""
            } else {
                " Check the account before attempting another change."
            };
            format!("Claim outcome is uncertain. {reason}{follow_up}")
        }
    }
}

fn report(attempts: &[Attempt], simulated: bool, log: &mut Option<RunLog>) -> u8 {
    let label = if simulated { "[simulated] " } else { "" };
    for attempt in attempts {
        let message = outcome_message(&attempt.outcome, simulated);
        let offset = (attempt.sent - attempt.slot)
            .num_microseconds()
            .unwrap_or_default() as f64
            / 1000.0;
        let result = format!(
            "{}  +{offset:.3} ms  {} ms  {label}{message}",
            utc(attempt.sent),
            attempt.elapsed.as_millis()
        );
        println!("{result}");
        record(log, format_args!("Slot {}  {result}", utc(attempt.slot)));
    }
    match attempts.last().map(|attempt| &attempt.outcome) {
        None => {
            eprintln!("No requests sent. The scheduled slots were missed.");
            record(log, "No requests sent. The scheduled slots were missed.");
            1
        }
        Some(_) if simulated => 0,
        Some(ClaimOutcome::Claimed) => 0,
        Some(ClaimOutcome::RateLimited(_)) => 3,
        Some(ClaimOutcome::Uncertain(_)) => 4,
        Some(_) => 1,
    }
}

fn record(log: &mut Option<RunLog>, message: impl Display) {
    if let Some(file) = log
        && let Err(error) = file.event(message)
    {
        eprintln!("Could not write the run log. {error}");
        *log = None;
    }
}

fn describe_clock(calibration: CalibratedClock) -> Result<String> {
    let sample = calibration.sample();
    let quality = calibration.current()?;
    let offset = sample.offset.num_microseconds().unwrap_or_default() as f64 / 1000.0;
    let direction = if offset < 0.0 { "ahead of" } else { "behind" };
    let difference = if offset.abs() >= 1000.0 {
        format!("{:.2} seconds", offset.abs() / 1000.0)
    } else {
        format!("{:.3} ms", offset.abs())
    };
    let round_trip = sample.round_trip.as_secs_f64() * 1000.0;
    let disagreement = sample.disagreement.num_microseconds().unwrap_or_default() as f64 / 1000.0;
    let uncertainty = quality.uncertainty.as_secs_f64() * 1000.0;
    Ok(format!(
        "System clock is {difference} {direction} UTC. {} NTP sources agree within {disagreement:.1} ms. Round trip {round_trip:.1} ms. Sample age {} s. Estimated uncertainty {uncertainty:.0} ms.",
        sample.sources,
        quality.age.as_secs()
    ))
}

async fn clock_check() -> Result<u8> {
    println!("Checking UTC against public time sources...");
    match select_calibration(None, time_sync::synchronize().await, false) {
        Ok((calibration, _)) => {
            println!("{}", describe_clock(calibration)?);
            Ok(0)
        }
        Err(error) => {
            eprintln!("{error}");
            Ok(1)
        }
    }
}

fn select_calibration(
    previous: Option<CalibratedClock>,
    result: std::result::Result<ClockSample, ClockCheckError>,
    holdover_allowed: bool,
) -> Result<(CalibratedClock, Option<String>)> {
    let fresh = match result {
        Err(error @ ClockCheckError::OffsetTooLarge(_)) => {
            bail!("{error} Sync the system clock before scheduling an absolute time.")
        }
        Err(error) => Err(anyhow::anyhow!("{error}")),
        Ok(sample) => {
            let calibration = CalibratedClock::new(sample);
            if !holdover_allowed
                && previous.is_some_and(|old| sample.checked_at <= old.sample().checked_at)
            {
                Err(anyhow::anyhow!(
                    "A new UTC calibration is required after an interrupted wait."
                ))
            } else {
                calibration
                    .current()
                    .map(|_| calibration)
                    .map_err(Into::into)
            }
        }
    };
    match fresh {
        Ok(calibration) => Ok((calibration, None)),
        Err(error) if holdover_allowed => {
            if let Some(calibration) = previous {
                calibration
                    .current()
                    .with_context(|| format!("UTC refresh failed. {error}"))?;
                Ok((
                    calibration,
                    Some(format!("{error} Keeping the last verified correction.")),
                ))
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

async fn refresh_clock(
    timing: Timing,
    holdover_allowed: bool,
    log: &mut Option<RunLog>,
) -> Result<Timing> {
    let Timing::Absolute(previous) = timing else {
        return Ok(timing);
    };
    println!("Checking UTC against public time sources...");
    let (calibration, notice) =
        select_calibration(previous, time_sync::synchronize().await, holdover_allowed)
            .context("Absolute schedule paused because UTC could not be verified.")?;
    if let Some(notice) = notice {
        println!("{notice}");
        record(log, notice);
    }
    let message = describe_clock(calibration)?;
    println!("{message}");
    record(log, message);
    Ok(Timing::Absolute(Some(calibration)))
}

async fn preview_timing() -> Timing {
    match select_calibration(None, time_sync::synchronize().await, false) {
        Ok((calibration, _)) => Timing::Absolute(Some(calibration)),
        Err(error) => {
            println!("{error} Showing the estimate using the system clock.");
            Timing::Absolute(None)
        }
    }
}

fn preflight_lead(profile_count: usize) -> TimeDelta {
    let batches = profile_count.div_ceil(claim_group::READ_CONCURRENCY) as u32;
    let read_budget = HTTP_REQUEST_TIMEOUT * (2 * batches);
    TimeDelta::from_std(MIN_PREFLIGHT_LEAD.max(read_budget + PREFLIGHT_MARGIN))
        .expect("preflight budget fits in TimeDelta")
}

async fn wait_for_preflight(
    first: DateTime<Utc>,
    profile_count: usize,
    timing: Timing,
    log: &mut Option<RunLog>,
) -> Result<Timing> {
    let mut timing = refresh_clock(timing, true, log).await?;
    let mut refresh_at = Instant::now() + CLOCK_REFRESH_INTERVAL;
    let preflight = first - preflight_lead(profile_count);
    if preflight > timing.current()?.now() {
        println!(
            "Waiting to check {profile_count} profile(s) at {}. Press Ctrl+C to cancel.",
            utc(preflight)
        );
    }
    loop {
        let clock = match timing.current() {
            Ok(clock) => clock,
            Err(_) => {
                timing = refresh_clock(timing, false, log).await?;
                refresh_at = Instant::now() + CLOCK_REFRESH_INTERVAL;
                timing.current()?
            }
        };
        let now = clock.now();
        if now >= preflight {
            break;
        }
        let until = match timing {
            Timing::Relative(_) => preflight,
            Timing::Absolute(_) => {
                let until_refresh = refresh_at.saturating_duration_since(Instant::now());
                preflight.min(now + TimeDelta::from_std(until_refresh)?)
            }
        };
        let completed =
            schedule::countdown_until(clock, first, until, || timing.current().is_ok()).await;
        if !completed || (Instant::now() >= refresh_at && clock.now() < preflight) {
            timing = refresh_clock(timing, completed, log).await?;
            refresh_at = Instant::now() + CLOCK_REFRESH_INTERVAL;
        }
    }
    if let Timing::Absolute(Some(calibration)) = timing
        && calibration.sample().checked_at.elapsed() >= Duration::from_secs(60)
        && first - timing.current()?.now() > CLOCK_CHECK_HEADROOM
    {
        timing = refresh_clock(timing, true, log).await?;
    }
    Ok(timing)
}

fn can_start_before_clock_check(plan: &Plan, timing: Timing) -> bool {
    let earliest_now = timing
        .current()
        .map_or_else(|_| Utc::now() - time_sync::MAX_OFFSET, Clock::now);
    plan.can_start(earliest_now)
}

async fn lock_slot_clock(
    timing: &mut Timing,
    slot: DateTime<Utc>,
    log: &mut Option<RunLog>,
) -> Result<Clock> {
    let countdown_end = slot - TimeDelta::seconds(1);
    loop {
        if let Ok(clock) = timing.current()
            && schedule::countdown_until(clock, slot, countdown_end, || timing.current().is_ok())
                .await
        {
            return Ok(clock);
        }
        *timing = refresh_clock(*timing, false, log).await?;
    }
}

async fn live(
    accounts: Vec<LiveAccount>,
    name: &str,
    plan: &Plan,
    interval: u8,
    timing: Timing,
) -> Result<u8> {
    let mut log = match RunLog::open() {
        Ok(file) => {
            println!("Run log  {}", file.path().display());
            Some(file)
        }
        Err(error) => {
            eprintln!("Could not open a run log. {error:#}");
            None
        }
    };
    record(&mut log, format_args!("Live run for {name}."));
    for account in &accounts {
        record(&mut log, format_args!("Authorized profile {}", account.id));
    }
    if matches!(timing, Timing::Relative(_)) {
        record(
            &mut log,
            "Relative schedule based on elapsed time. Displayed UTC times use the system clock at selection.",
        );
    }
    for &slot in &plan.slots {
        record(&mut log, format_args!("Planned attempt  {}", utc(slot)));
    }

    let in_flight = Cell::new(false);
    let execute = execute_live_plan(
        &accounts, name, plan, interval, timing, &in_flight, &mut log,
    );

    let result = tokio::select! {
        biased;
        signal = tokio::signal::ctrl_c() => {
            signal.context("Could not listen for Ctrl+C.").map(|()| {
                if in_flight.get() {
                    eprintln!("Cancelled during a name-change request. Check the selected accounts before attempting another change.");
                } else {
                    eprintln!("Cancelled.");
                }
                130
            })
        }
        result = execute => result,
    };
    match &result {
        Ok(130) if in_flight.get() => record(
            &mut log,
            "Cancelled during a name-change request. Outcome uncertain.",
        ),
        Ok(130) => record(&mut log, "Cancelled."),
        Ok(code) => record(&mut log, format_args!("Finished with exit code {code}.")),
        Err(error) => record(&mut log, format_args!("Stopped. {error:#}")),
    }
    result
}

async fn execute_live_plan(
    accounts: &[LiveAccount],
    name: &str,
    plan: &Plan,
    interval: u8,
    timing: Timing,
    in_flight: &Cell<bool>,
    log: &mut Option<RunLog>,
) -> Result<u8> {
    let group = accounts.len() > 1;
    ensure!(
        can_start_before_clock_check(plan, timing),
        "The scheduled slots have already passed."
    );
    let mut timing = timing;
    let mut slots = LiveSlots::new(Duration::from_secs(u64::from(interval)), Clock::default());
    let mut attempted = false;
    for (index, batch_slots) in plan.preflight_batches().enumerate() {
        let first = batch_slots[0];
        if index > 0 {
            let message = format!("Next attempt scheduled for {}.", utc(first));
            println!("{message}");
            record(log, message);
        }
        let prepared = claim_group::prepare(accounts, name, batch_slots.len())?;
        timing = wait_for_preflight(first, prepared.len(), timing, log).await?;
        let now = timing.current()?.now();
        ensure!(
            attempted || plan.can_start(now),
            "The scheduled slots have already passed."
        );
        if !schedule::slot_is_open(*batch_slots.last().expect("nonempty batch"), now) {
            let message = format!("Missed scheduled slot {}.", utc(first));
            println!("{message}");
            record(log, message);
            continue;
        }
        let (mut armed, skipped) = claim_group::preflight(prepared, name).await?;
        for note in skipped {
            println!("{note}");
            record(log, note);
        }
        if timing
            .current()
            .is_ok_and(|clock| clock.now() < first - TimeDelta::seconds(1))
        {
            println!(
                "Ready to attempt {name} with {} verified profile(s).",
                armed.len()
            );
        }
        let mut clock_error = None;
        let mut details = Vec::new();
        let attempts = schedule::execute(
            batch_slots,
            async |slot| {
                if clock_error.is_some() {
                    return None;
                }
                let clock = match lock_slot_clock(&mut timing, slot, log).await {
                    Ok(clock) => clock,
                    Err(error) => {
                        clock_error = Some(error);
                        return None;
                    }
                };
                slots.use_clock(clock);
                let stamp = slots.wait(slot).await?;
                if let Err(error) = timing.current() {
                    clock_error = Some(error);
                    return None;
                }
                Some(stamp)
            },
            async || {
                in_flight.set(true);
                let batch = claim_group::dispatch(&mut armed, name).await;
                in_flight.set(false);
                if group {
                    details = batch.details;
                }
                batch.outcome
            },
        )
        .await;
        for result in details {
            let message = outcome_message(&result.outcome, false);
            let line = format!("{} ({})  {message}", result.name, result.id);
            println!("{line}");
            record(log, line);
        }
        let status = if attempts.is_empty() {
            1
        } else {
            report(&attempts, false, log)
        };
        if let Some(error) = clock_error {
            return Err(error.context("Absolute schedule paused before dispatch."));
        }
        if let Some(last) = attempts.last() {
            attempted = true;
            if !matches!(last.outcome, ClaimOutcome::Unavailable) {
                return Ok(status);
            }
        } else if plan.slots.len() > batch_slots.len() {
            let message = format!("Missed scheduled slot {}.", utc(first));
            println!("{message}");
            record(log, message);
        }
    }
    Ok(if attempted {
        1
    } else {
        report(&[], false, log)
    })
}

async fn check_account(token_stdin: bool) -> Result<u8> {
    let mc = Minecraft::new(http_client()?, read_token(token_stdin)?)?;
    let profile = mc.profile().await?;
    let account =
        Uuid::parse_str(&profile.id).context("Minecraft returned an invalid profile UUID.")?;
    let eligibility = if mc.eligible().await? {
        "eligible"
    } else {
        "unavailable"
    };
    println!(
        "Java profile  {}\nProfile UUID  {account}\nName change   {eligibility}",
        profile.name
    );
    Ok(0)
}

fn default_list_path(filename: &str) -> Result<PathBuf> {
    let local = Path::new(filename);
    if local.is_file() {
        return Ok(local.to_path_buf());
    }
    Ok(std::env::current_exe()
        .context("Could not find the application folder.")?
        .with_file_name(filename))
}

async fn check_tokens(file: Option<PathBuf>) -> Result<u8> {
    let path = match file {
        Some(path) => path,
        None => default_list_path("tokens.txt")?,
    };
    let batch = accounts::check_file(&path, http_client()?).await?;
    Ok(if batch.rate_limit.is_some() {
        3
    } else {
        u8::from(batch.failures != 0)
    })
}

fn validate_live_selection(ids: &[Uuid], file: Option<&Path>, attempts: u8) -> Result<()> {
    ensure!(
        ids.len() <= MAX_TOKEN_ENTRIES,
        "Select at most {MAX_TOKEN_ENTRIES} Java profiles with --account."
    );
    ensure!(
        !ids.iter()
            .enumerate()
            .any(|(index, id)| ids[..index].contains(id)),
        "Specify each Java profile only once."
    );
    if ids.len() != 1 {
        ensure!(
            file.is_some(),
            "Use --tokens-file when selecting multiple profiles."
        );
        ensure!(
            attempts == 1,
            "Group mode uses one scheduled attempt per selected profile. Omit --attempts."
        );
    }
    Ok(())
}

async fn load_live_accounts(
    ids: &[Uuid],
    file: Option<&Path>,
    token_stdin: bool,
    target: &str,
) -> Result<Vec<LiveAccount>> {
    let client = http_client()?;
    let Some(file) = file else {
        return Ok(vec![LiveAccount {
            id: ids[0],
            minecraft: Minecraft::new(client, read_token(token_stdin)?)?,
        }]);
    };
    let mut batch = accounts::check_file(file, client).await?;
    if ids.is_empty() {
        return Ok(batch
            .into_eligible_accounts(target)?
            .into_iter()
            .map(|account| LiveAccount {
                id: account.id,
                minecraft: account.minecraft,
            })
            .collect());
    }
    if let Some(rate_limit) = batch.rate_limit {
        return Err(rate_limit.into());
    }
    ids.iter()
        .map(|id| {
            let index = batch
                .accounts
                .iter()
                .position(|account| account.id == *id)
                .with_context(|| format!("No validated token found for profile {id}."))?;
            let account = batch.accounts.swap_remove(index);
            ensure!(
                account.eligible,
                "Minecraft reports that {} cannot change its name yet.",
                account.profile.name
            );
            Ok(LiveAccount {
                id: *id,
                minecraft: account.minecraft,
            })
        })
        .collect()
}

async fn simulate(scenario: Scenario) -> Result<u8> {
    let window = DropWindow::manual(Utc::now(), None)?;
    let plan = Plan::new(&window, false, 3, 10)?;
    println!("Offline simulation");
    let attempts = schedule::execute(
        &plan.slots,
        async |slot| Some(slot),
        async || match scenario {
            Scenario::Success => ClaimOutcome::Claimed,
            Scenario::Unavailable => ClaimOutcome::Unavailable,
            Scenario::RateLimited => ClaimOutcome::RateLimited(Some(Duration::from_secs(30))),
            Scenario::Uncertain => ClaimOutcome::Uncertain("Simulated timeout.".to_owned()),
            Scenario::Rejected => ClaimOutcome::Rejected("Simulated account cooldown.".to_owned()),
        },
    )
    .await;
    Ok(report(&attempts, true, &mut None))
}

async fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => menu::run().await,
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(0)
        }
        Some(Command::Menu) => menu::run().await,
        Some(Command::Drop { name, times }) => {
            let (window, source) = match times.at {
                Some(start) => (DropWindow::manual(start, times.until)?, "manual"),
                None => {
                    ensure!(
                        name.len() == 3,
                        "3NAME only tracks three-character usernames."
                    );
                    let timing = preview_timing().await;
                    let clock = timing.current().unwrap_or_default();
                    (
                        drops::lookup(&http_client()?, &name, || clock.now()).await?,
                        "3NAME estimate",
                    )
                }
            };
            show_window(&name, &window, source);
            Ok(0)
        }
        Some(Command::Check { token_stdin }) => check_account(token_stdin).await,
        Some(Command::CheckTokens { file }) => check_tokens(file).await,
        Some(Command::TestProxies { file, output }) => {
            proxies::check_file(&file, output.as_deref()).await
        }
        Some(Command::ClockCheck) => clock_check().await,
        Some(Command::Benchmark { samples }) => benchmark::run(samples).await,
        Some(Command::Snipe {
            name,
            times,
            attempts,
            interval,
            live: live_requested,
            account,
            tokens_file,
            token_stdin,
        }) => {
            if live_requested {
                validate_live_selection(&account, tokens_file.as_deref(), attempts)?;
            }
            let bounded = times.until.is_some() || (live_requested && times.at.is_none());
            let mut timing = Timing::Absolute(None);
            let (window, source) = match times.at {
                Some(start) => (DropWindow::manual(start, times.until)?, "manual"),
                None if live_requested => {
                    ensure!(
                        name.len() == 3,
                        "3NAME only tracks three-character usernames. Pass --at to choose a time."
                    );
                    timing = refresh_clock(timing, false, &mut None).await?;
                    let clock = timing.current()?;
                    let window = drops::lookup(&http_client()?, &name, || clock.now())
                        .await
                        .context(
                            "Could not schedule from 3NAME. Pass --at to choose a time manually.",
                        )?;
                    (window, "3NAME estimate")
                }
                None => (
                    DropWindow::manual(Utc::now() + TimeDelta::seconds(2), None)?,
                    "example schedule",
                ),
            };
            show_window(&name, &window, source);
            let plan = Plan::new(&window, bounded, attempts, interval)?;
            show_plan(&plan);
            if !live_requested {
                println!("Offline preview. Run simulate to try the outcome handling.");
                return Ok(0);
            }
            ensure!(
                can_start_before_clock_check(&plan, timing),
                "The scheduled slots have already passed."
            );
            let accounts =
                load_live_accounts(&account, tokens_file.as_deref(), token_stdin, &name).await?;
            for account in &accounts {
                println!("Live name change authorized for profile {}.", account.id);
            }
            live(accounts, &name, &plan, interval, timing).await
        }
        Some(Command::Simulate { scenario }) => simulate(scenario).await,
    }
}

fn cli_exit_code(result: Result<u8>) -> u8 {
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            if error.is::<ReadRateLimit>() { 3 } else { 1 }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    ExitCode::from(cli_exit_code(run(Cli::parse()).await))
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    fn previous() -> CalibratedClock {
        let tick = Instant::now();
        let wall = Utc::now();
        let offset = TimeDelta::seconds(50);
        CalibratedClock::new(ClockSample {
            offset,
            round_trip: Duration::from_millis(20),
            sources: 2,
            checked_at: tick,
            observed_at: tick,
            reference: wall + offset,
            disagreement: TimeDelta::milliseconds(3),
            uncertainty: Duration::from_millis(25),
        })
    }

    #[test]
    fn a_transient_refresh_failure_keeps_the_same_calibration() {
        let calibration = previous();
        let (retained, notice) = select_calibration(
            Some(calibration),
            Err(ClockCheckError::NoAgreement(0)),
            true,
        )
        .unwrap();
        assert_eq!(
            retained.sample().checked_at,
            calibration.sample().checked_at
        );
        assert!(retained.current().unwrap().clock.now() > Utc::now() + TimeDelta::seconds(49));
        assert!(notice.is_some());
    }

    #[test]
    fn an_absolute_schedule_needs_calibration_and_rechecks_after_interruptions() {
        assert!(select_calibration(None, Err(ClockCheckError::NoAgreement(0)), true).is_err());
        let calibration = previous();
        assert!(select_calibration(Some(calibration), Ok(calibration.sample()), false).is_err());
    }

    #[test]
    fn preflight_budget_scales_past_the_first_batch_of_profiles() {
        assert_eq!(preflight_lead(1), TimeDelta::seconds(20));
        assert_eq!(preflight_lead(4), TimeDelta::seconds(20));
        assert_eq!(preflight_lead(5), TimeDelta::seconds(25));
        assert_eq!(preflight_lead(MAX_TOKEN_ENTRIES), TimeDelta::seconds(165));
    }
}
