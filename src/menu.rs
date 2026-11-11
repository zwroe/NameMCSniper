use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

use anyhow::{Result, bail, ensure};
use chrono::{TimeDelta, Utc};
use uuid::Uuid;

use crate::{
    Scenario, Timing, accounts, benchmark, can_start_before_clock_check, check_account,
    claim_group::LiveAccount,
    clock_check, default_list_path, drops,
    drops::DropWindow,
    http_client, live,
    minecraft::Minecraft,
    parse_name, parse_time, preview_timing, proxies, read_token,
    schedule::{Clock, NAMEMC_MIN_SPACING, Plan},
    show_plan, show_window, simulate, time_sync, utc,
};

const TITLE: &str = r"  _  _   _   __  __ ___ __  __  ___
 | \| | /_\ |  \/  | __|  \/  |/ __|
 | .` |/ _ \| |\/| | _|| |\/| | (__
 |_|\_/_/ \_\_|  |_|___|_|  |_|\___|";

const GREEN_ON_BLACK: &str = "\x1b[92;40m";
const RESET: &str = "\x1b[0m";

macro_rules! menu_line {
    ($($args:tt)*) => {
        anstream::println!("{GREEN_ON_BLACK}{}{RESET}", format_args!($($args)*))
    };
}

pub async fn run() -> Result<u8> {
    println!();
    for line in TITLE.lines() {
        menu_line!("{line:<42}");
    }
    menu_line!("  NameMCSniper");
    menu_line!("  +--------------------------------------+");
    menu_line!("  | Schedule a Minecraft Java rename     |");
    menu_line!("  +--------------------------------------+");

    loop {
        section("MAIN MENU");
        menu_line!("  1  Schedule a name change");
        menu_line!("  2  Check my Minecraft account");
        menu_line!("  3  Try an offline simulation");
        menu_line!("  4  Test proxies");
        menu_line!("  5  Measure dispatch timing");
        menu_line!("  6  Check UTC clock");
        menu_line!("  7  Test bearer tokens (tokens.txt)");
        menu_line!("  0  Exit");

        let result = match prompt("Choose 0–7 > ")?.as_deref() {
            None | Some("0") => return Ok(0),
            Some("1") => schedule_name().await,
            Some("2") => account_check().await,
            Some("3") => simulate(Scenario::Success).await.map(|_| ()),
            Some("4") => test_proxies().await,
            Some("5") => benchmark::run(benchmark::DEFAULT_SAMPLES).await.map(|_| ()),
            Some("6") => clock_check().await.map(|_| ()),
            Some("7") => check_token_list().await,
            _ => {
                println!("Choose one of the numbers above.");
                continue;
            }
        };
        if let Err(error) = result {
            eprintln!("Error: {error:#}");
        }
    }
}

async fn test_proxies() -> Result<()> {
    section("TEST PROXIES");
    println!("Use one proxy per line: host:port, host:port@username:password, or an HTTP(S) URL.");
    let Some(path) = list_path("proxies.txt")? else {
        return Ok(());
    };
    let working = path.with_file_name("proxies_working.txt");
    println!("Exports retain any proxy credentials.");
    menu_line!("  1  Export to {}", working.display());
    menu_line!("  0  Test only");
    let output = match prompt("Export choice or custom file path (Enter to skip) > ")?.as_deref() {
        None | Some("" | "0") => None,
        Some("1") => Some(working),
        Some(path) => Some(file_path(path)),
    };
    proxies::check_file(&path, output.as_deref()).await?;
    Ok(())
}

fn list_path(filename: &str) -> Result<Option<PathBuf>> {
    let default = default_list_path(filename)?;
    println!("Default list  {}", default.display());
    let path = match prompt("List path (Enter for default, 0 to go back) > ")?.as_deref() {
        None | Some("0") => return Ok(None),
        Some("") => default,
        Some(path) => file_path(path),
    };
    if path
        .metadata()
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        bail!(
            "List file missing at {}\nCopy {filename}.example to that path, then add your entries.",
            std::path::absolute(&path)?.display()
        );
    }
    Ok(Some(path))
}

fn file_path(path: &str) -> PathBuf {
    PathBuf::from(
        path.strip_prefix('"')
            .and_then(|path| path.strip_suffix('"'))
            .unwrap_or(path),
    )
}

async fn account_check() -> Result<()> {
    require_terminal()?;
    check_account(false).await?;
    Ok(())
}

async fn check_token_list() -> Result<()> {
    require_terminal()?;
    section("TEST BEARER TOKENS");
    println!("Save one Minecraft Services bearer token per line in tokens.txt.");
    if let Some(path) = list_path("tokens.txt")? {
        accounts::check_file(&path, http_client()?).await?;
    }
    Ok(())
}

async fn choose_live_accounts(target: &str) -> Result<Option<Vec<(LiveAccount, String)>>> {
    section("CHOOSE ACCOUNTS");
    loop {
        menu_line!("  1  Paste one token");
        menu_line!("  2  Use all eligible profiles from tokens.txt");
        menu_line!("  0  Back to main menu");
        match prompt("Choose 0–2 > ")?.as_deref() {
            None | Some("" | "0") => return Ok(None),
            Some("1") => {
                let minecraft = Minecraft::new(http_client()?, read_token(false)?)?;
                let profile = minecraft.profile().await?;
                let id = Uuid::parse_str(&profile.id)?;
                ensure!(
                    !profile.name.eq_ignore_ascii_case(target),
                    "This account already has the requested name."
                );
                return Ok(Some(vec![(LiveAccount { id, minecraft }, profile.name)]));
            }
            Some("2") => {
                let Some(path) = list_path("tokens.txt")? else {
                    continue;
                };
                let eligible = accounts::check_file(&path, http_client()?)
                    .await?
                    .into_eligible_accounts(target)?;
                return Ok(Some(
                    eligible
                        .into_iter()
                        .map(|account| {
                            (
                                LiveAccount {
                                    id: account.id,
                                    minecraft: account.minecraft,
                                },
                                account.profile.name,
                            )
                        })
                        .collect(),
                ));
            }
            _ => println!("Choose one of the numbers above."),
        }
    }
}

async fn schedule_name() -> Result<()> {
    section("SCHEDULE A NAME CHANGE");
    let name = loop {
        let Some(value) = prompt("Target Java username (Enter to go back) > ")? else {
            return Ok(());
        };
        if value.is_empty() {
            return Ok(());
        }
        match parse_name(&value) {
            Ok(name) => break name,
            Err(error) => println!("{error}"),
        }
    };

    let mut timing = Timing::Absolute(None);
    let estimate = if name.len() == 3 {
        println!("Looking up {name} on 3NAME...");
        timing = preview_timing().await;
        let clock = timing.current().unwrap_or_default();
        match drops::lookup(&http_client()?, &name, || clock.now()).await {
            Ok(window) => {
                show_window(&name, &window, "3NAME estimate");
                if window.end - window.start > TimeDelta::seconds(30) {
                    println!("This window spans over 30 seconds. Choose a specific time below.");
                }
                Some(window)
            }
            Err(error) => {
                println!("3NAME estimate unavailable. {error:#}");
                None
            }
        }
    } else {
        println!("3NAME tracks three-character names. Choose a time below.");
        None
    };
    let Some((plan, timing)) = choose_time(estimate, timing)? else {
        return Ok(());
    };

    let first = plan.slots[0];
    section("Offline preview");
    println!("  Target     {name}");
    if matches!(timing, Timing::Relative(_)) {
        println!("  Timing     Two minutes from selection");
    } else {
        show_plan(&plan);
        if plan.slots.len() > 1 {
            println!(
                "  The second attempt runs if Minecraft reports the name unavailable on the first attempt."
            );
            println!("  It also runs if the first slot passes before a request is sent.");
        }
    }
    loop {
        menu_line!("  1  Continue to live setup");
        menu_line!("  0  Back to main menu");
        match prompt("Choose 0 or 1 > ")?.as_deref() {
            None | Some("" | "0") => return Ok(()),
            Some("1") => break,
            _ => println!("Choose 0 or 1."),
        }
    }

    require_terminal()?;
    ensure!(
        can_start_before_clock_check(&plan, timing),
        "That time has already passed."
    );
    let Some(accounts) = choose_live_accounts(&name).await? else {
        return Ok(());
    };
    ensure!(
        can_start_before_clock_check(&plan, timing),
        "That time has already passed."
    );

    section("CONFIRM LIVE RENAME");
    for (account, profile) in &accounts {
        println!("  Account  {profile} ({})", account.id);
    }
    if accounts.len() > 1 {
        println!(
            "  Requests Up to {} per attempt, one per eligible profile",
            accounts.len()
        );
    }
    println!("  Target   {name}");
    if let Timing::Relative(clock) = timing {
        println!(
            "  Time     In about {} seconds",
            (first - clock.now()).num_seconds().max(0)
        );
    } else if plan.slots.len() > 1 {
        show_plan(&plan);
    } else {
        println!("  Time     {}", utc(first));
    }
    if prompt("Type RENAME to confirm, or press Enter to cancel > ")?.as_deref() != Some("RENAME") {
        println!("Live rename cancelled.");
        return Ok(());
    }
    let accounts = accounts.into_iter().map(|(account, _)| account).collect();
    live(accounts, &name, &plan, 10, timing).await?;
    Ok(())
}

fn choose_time(estimate: Option<DropWindow>, timing: Timing) -> Result<Option<(Plan, Timing)>> {
    loop {
        let now = timing.current().map_or_else(|_| Utc::now(), Clock::now);
        let latest = estimate.map(|window| window.end).filter(|end| *end > now);
        let earliest = estimate
            .filter(|window| !window.in_progress && window.start < window.end)
            .map(|window| window.start)
            .filter(|start| *start > now);

        section("CHOOSE A TIME");
        menu_line!("  1  Two minutes from now");
        menu_line!("  2  Paste a NameMC drop window or enter a time");
        if let Some(time) = latest {
            menu_line!("  3  Use 3NAME Latest   {}", utc(time));
        }
        if let Some(time) = earliest {
            menu_line!("  4  Use 3NAME Earliest {}", utc(time));
        }
        menu_line!("  0  Back to main menu");
        let plan = match prompt("Timing choice > ")?.as_deref() {
            None | Some("0") => return Ok(None),
            Some("1") => {
                let clock = Clock::anchored(TimeDelta::zero());
                return Ok(Some((
                    Plan::at(clock.now() + TimeDelta::minutes(2)),
                    Timing::Relative(clock),
                )));
            }
            Some("2") => {
                let Some(plan) = choose_manual_time(timing)? else {
                    continue;
                };
                plan
            }
            Some("3") if latest.is_some() => Plan::at(latest.unwrap()),
            Some("4") if earliest.is_some() => Plan::at(earliest.unwrap()),
            _ => {
                println!("Choose one of the numbers above.");
                continue;
            }
        };
        let earliest_now = timing
            .current()
            .map_or_else(|_| Utc::now() - time_sync::MAX_OFFSET, Clock::now);
        if plan.slots.last().is_some_and(|time| *time > earliest_now) {
            return Ok(Some((plan, timing)));
        }
        println!("That time has already passed. Choose another time.");
    }
}

fn choose_manual_time(timing: Timing) -> Result<Option<Plan>> {
    println!("Paste one NameMC timestamp or both lines from its Drop Window.");
    println!("Example  11/9/2026 • 2:38:39 PM");
    println!("Exact times also accept RFC 3339 with an offset or Unix milliseconds.");
    let Some(mut first) = prompt("Time or first window line (Enter to go back) > ")? else {
        return Ok(None);
    };
    if first
        .trim_end_matches(':')
        .eq_ignore_ascii_case("Drop Window")
    {
        let Some(value) = prompt("First window line > ")? else {
            return Ok(None);
        };
        first = value;
    }
    if first.is_empty() {
        return Ok(None);
    }
    if let Ok(time) = parse_time(&first) {
        return Ok(Some(Plan::at(time)));
    }
    let first = match drops::parse_namemc_time(&first) {
        Ok(time) => time,
        Err(error) => {
            println!("{error}");
            return Ok(None);
        }
    };
    let Some(last) = prompt("Last window line (Enter for one time) > ")? else {
        return Ok(None);
    };
    let last = if last.is_empty() {
        None
    } else {
        match drops::parse_namemc_time(&last) {
            Ok(time) => Some(time),
            Err(error) => {
                println!("{error}");
                return Ok(None);
            }
        }
    };
    let Some(zone) = prompt("NameMC time zone (Enter for this computer, or UTC / ±HH:MM) > ")?
    else {
        return Ok(None);
    };
    let window = match drops::namemc_window(first, last, &zone) {
        Ok(window) => window,
        Err(error) => {
            println!("{error}");
            return Ok(None);
        }
    };
    if window.start == window.end {
        return Ok(Some(Plan::at(window.start)));
    }

    println!("NameMC drop window (UTC)");
    println!("  Earliest  {}", utc(window.start));
    println!("  Latest    {}", utc(window.end));
    let now = timing.current().map_or_else(|_| Utc::now(), Clock::now);
    match Plan::from_namemc(window, now) {
        Ok(plan) => {
            if window.start <= now {
                println!("The earliest bound has passed. Scheduling the latest.");
            } else if window.end - window.start < NAMEMC_MIN_SPACING {
                println!(
                    "The bounds are less than {} seconds apart. Scheduling the latest.",
                    NAMEMC_MIN_SPACING.num_seconds()
                );
            }
            Ok(Some(plan))
        }
        Err(error) => {
            println!("{error}");
            Ok(None)
        }
    }
}

fn section(title: &str) {
    println!();
    menu_line!("  {title}");
}

fn prompt(label: &str) -> Result<Option<String>> {
    anstream::print!("{GREEN_ON_BLACK}  {label}{RESET}");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Ok(None);
    }
    Ok(Some(input.trim().to_owned()))
}

fn require_terminal() -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stderr().is_terminal(),
        "Open NameMCSniper in a terminal to use a Minecraft account."
    );
    Ok(())
}
