use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, FixedOffset, Local, LocalResult, NaiveDateTime, TimeZone, Timelike, Utc};
use scraper::{Html, Selector};
use std::time::Duration;

const MAX_PAGE_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// 3NAME reported an active interval and no longer displays its original start.
    pub in_progress: bool,
}

impl DropWindow {
    pub fn manual(start: DateTime<Utc>, end: Option<DateTime<Utc>>) -> Result<Self> {
        let end = end.unwrap_or(start);
        ensure!(start <= end, "drop start must be at or before the end");
        Ok(Self {
            start,
            end,
            in_progress: false,
        })
    }
}

pub fn parse_namemc_time(value: &str) -> Result<NaiveDateTime> {
    const EXAMPLE: &str = "Use a NameMC time such as 11/9/2026 • 2:38:39 PM.";
    let (date, time) = value.split_once(['•', '·']).context(EXAMPLE)?;
    let date = date.trim();
    let (_, year) = date.rsplit_once('/').context(EXAMPLE)?;
    ensure!(
        year.len() == 4 && year.bytes().all(|byte| byte.is_ascii_digit()) && year != "0000",
        "{EXAMPLE}"
    );
    let time = time.trim().replace('∶', ":");
    let parsed = NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%m/%d/%Y %I:%M:%S %p")
        .context(EXAMPLE)?;
    ensure!(parsed.nanosecond() < 1_000_000_000, "{EXAMPLE}");
    Ok(parsed)
}

pub fn namemc_window(
    first: NaiveDateTime,
    last: Option<NaiveDateTime>,
    zone: &str,
) -> Result<DropWindow> {
    let zone = zone.trim();
    if zone.is_empty() || zone.eq_ignore_ascii_case("local") {
        return namemc_window_in_zone(first, last, Local);
    }
    if zone.eq_ignore_ascii_case("UTC") || zone.eq_ignore_ascii_case("Z") {
        return DropWindow::manual(first.and_utc(), last.map(|time| time.and_utc()));
    }

    const ZONE_HELP: &str = "Use local, UTC, or a UTC offset such as -05:00.";
    ensure!(
        matches!(
            zone.as_bytes(),
            [
                b'+' | b'-',
                b'0'..=b'9',
                b'0'..=b'9',
                b':',
                b'0'..=b'9',
                b'0'..=b'9'
            ]
        ),
        "{ZONE_HELP}"
    );
    let offset = zone.parse::<FixedOffset>().context(ZONE_HELP)?;
    namemc_window_in_zone(first, last, offset)
}

fn namemc_window_in_zone(
    first: NaiveDateTime,
    last: Option<NaiveDateTime>,
    zone: impl TimeZone,
) -> Result<DropWindow> {
    let to_utc = |time| match zone.from_local_datetime(&time) {
        LocalResult::Single(time) => Ok(time.to_utc()),
        LocalResult::Ambiguous(..) => {
            bail!("This local time occurs twice. Enter the UTC offset for the intended occurrence.")
        }
        LocalResult::None => {
            bail!("This time does not exist in your local time zone. Check the time and zone.")
        }
    };
    DropWindow::manual(to_utc(first)?, last.map(to_utc).transpose()?)
}

pub async fn lookup(
    client: &reqwest::Client,
    name: &str,
    now: impl FnOnce() -> DateTime<Utc>,
) -> Result<DropWindow> {
    ensure!(
        name.len() == 3 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "3NAME only tracks three-character usernames"
    );
    let name = name.to_ascii_lowercase();
    let mut response = client
        .get(format!("https://3name.xyz/name/{name}"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("could not reach 3NAME")?;
    ensure!(
        response.status().is_success(),
        "3NAME returned HTTP {}",
        response.status().as_u16()
    );
    if let Some(length) = response.content_length() {
        ensure!(length <= MAX_PAGE_BYTES as u64, "3NAME page is too large");
    }

    let mut page = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("could not read 3NAME page")?
    {
        ensure!(
            chunk.len() <= MAX_PAGE_BYTES - page.len(),
            "3NAME page is too large"
        );
        page.extend_from_slice(&chunk);
    }
    let html = std::str::from_utf8(&page).context("3NAME page is not UTF-8")?;
    parse_window(html, now())
        .with_context(|| format!("could not find a usable 3NAME drop interval for {name}"))
}

fn parse_window(html: &str, now: DateTime<Utc>) -> Result<DropWindow> {
    let document = Html::parse_document(html);
    let end = read_bound(&document, "#upper-bound-update", "data-upper-bound")?
        .context("3NAME did not publish a drop interval")?;
    ensure!(end > now, "3NAME's drop interval has already ended");

    if let Some(start) = read_bound(&document, "#lower-bound-update", "data-lower-bound")? {
        return DropWindow::manual(start, Some(end));
    }

    let heading = Selector::parse("h1").expect("valid heading selector");
    if document
        .select(&heading)
        .any(|element| element.text().collect::<String>().trim() == "Dropping Now")
    {
        return Ok(DropWindow {
            start: now,
            end,
            in_progress: true,
        });
    }

    bail!("3NAME's drop interval is missing its start time")
}

fn read_bound(document: &Html, selector: &str, attribute: &str) -> Result<Option<DateTime<Utc>>> {
    let selector = Selector::parse(selector).expect("valid 3NAME selector");
    let Some(element) = document.select(&selector).next() else {
        return Ok(None);
    };
    let value = element
        .attr(attribute)
        .with_context(|| format!("3NAME timestamp is missing {attribute}"))?;
    let milliseconds = value
        .parse::<i64>()
        .with_context(|| format!("invalid 3NAME timestamp in {attribute}"))?;
    let time = DateTime::from_timestamp_millis(milliseconds)
        .with_context(|| format!("3NAME timestamp in {attribute} is out of range"))?;
    Ok(Some(time))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_229_040_000;

    fn at(milliseconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(milliseconds).unwrap()
    }

    #[test]
    fn reads_namemc_screenshot_bounds_in_the_selected_offset() {
        let first = parse_namemc_time("11/9/2026 • 2:38:39 PM").unwrap();
        let last = parse_namemc_time("11/11/2026 • 10:18:24 AM").unwrap();
        let window = namemc_window(first, Some(last), "-05:00").unwrap();

        assert_eq!(window.start.to_rfc3339(), "2026-11-09T19:38:39+00:00");
        assert_eq!(window.end.to_rfc3339(), "2026-11-11T15:18:24+00:00");
        assert!(!window.in_progress);
    }

    #[test]
    fn parses_namemc_midnight_and_noon_with_alternate_separators() {
        let midnight = parse_namemc_time(" 11/9/2026 · 12∶00∶00 am ").unwrap();
        let noon = parse_namemc_time("11/9/2026 • 12:00:00 PM").unwrap();
        let window = namemc_window(midnight, Some(noon), "UTC").unwrap();

        assert_eq!(window.start.to_rfc3339(), "2026-11-09T00:00:00+00:00");
        assert_eq!(window.end.to_rfc3339(), "2026-11-09T12:00:00+00:00");
        let exact = namemc_window(noon, None, "+05:30").unwrap();
        assert_eq!(exact.start.to_rfc3339(), "2026-11-09T06:30:00+00:00");
        assert_eq!(exact.start, exact.end);
    }

    #[test]
    fn rejects_invalid_namemc_times_zones_and_reversed_bounds() {
        for value in [
            "11/31/2026 • 2:38:39 PM",
            "11/9/26 • 2:38:39 PM",
            "11/9/2026 • 14:38:39 PM",
            "11/9/2026 • 2:38:60 PM",
            "11/9/2026 • 2:38:39",
        ] {
            assert!(parse_namemc_time(value).is_err(), "{value}");
        }

        let first = parse_namemc_time("11/9/2026 • 2:38:39 PM").unwrap();
        let last = parse_namemc_time("11/11/2026 • 10:18:24 AM").unwrap();
        for zone in ["EST", "-0500", "+24:00", "-05:60", "+05:00 trailing"] {
            assert!(namemc_window(first, Some(last), zone).is_err(), "{zone}");
        }
        assert!(namemc_window(last, Some(first), "Z").is_err());
    }

    #[test]
    fn reads_both_bounds_without_losing_milliseconds() {
        let html = r#"
            <span data-lower-bound='1794229045774' id='lower-bound-update'></span>
            <span id="upper-bound-update" data-upper-bound="1794266832014"></span>
        "#;
        let window = parse_window(html, at(NOW)).unwrap();
        assert_eq!(window.start, at(1_794_229_045_774));
        assert_eq!(window.end, at(1_794_266_832_014));
        assert!(!window.in_progress);
    }

    #[test]
    fn active_interval_starts_now_and_keeps_its_end() {
        let html = r#"
            <h1>Dropping Now</h1>
            <span id="upper-bound-update" data-upper-bound="1794266832014"></span>
        "#;
        let window = parse_window(html, at(NOW)).unwrap();
        assert_eq!(window.start, at(NOW));
        assert_eq!(window.end, at(1_794_266_832_014));
        assert!(window.in_progress);
    }

    #[test]
    fn verified_time_distinguishes_active_and_expired_windows_despite_clock_skew() {
        let html = format!(
            r#"<h1>Dropping Now</h1><span id="upper-bound-update" data-upper-bound="{}"></span>"#,
            NOW + 20_000
        );
        let verified_now = at(NOW);
        let fast_pc_now = at(NOW + 50_000);
        assert_eq!(
            parse_window(&html, verified_now).unwrap().start,
            verified_now
        );
        assert!(parse_window(&html, fast_pc_now).is_err());

        let verified_later = at(NOW + 30_000);
        let slow_pc_later = at(NOW + 30_000 - 50_000);
        assert!(parse_window(&html, verified_later).is_err());
        assert!(parse_window(&html, slow_pc_later).is_ok());
    }

    #[test]
    fn rejects_unavailable_expired_reversed_or_malformed_data() {
        let pages = [
            "<h1>Invalid Query</h1>",
            r#"<span id="upper-bound-update" data-upper-bound="1794266832014"></span>"#,
            r#"<h1>Dropping Now</h1><span id="upper-bound-update" data-upper-bound="1794229040000"></span>"#,
            r#"<h1>Dropping Now</h1><span id="upper-bound-update" data-upper-bound="not-a-timestamp"></span>"#,
            r#"<h1>Dropping Now</h1><span id="upper-bound-update" data-upper-bound="9223372036854775807"></span>"#,
            r#"<span id="lower-bound-update"></span><span id="upper-bound-update" data-upper-bound="1794266832014"></span>"#,
            r#"<span id="lower-bound-update" data-lower-bound="1794266832015"></span>
               <span id="upper-bound-update" data-upper-bound="1794266832014"></span>"#,
        ];
        for html in pages {
            assert!(parse_window(html, at(NOW)).is_err(), "{html}");
        }
    }

    #[test]
    fn validates_manual_windows_and_accepts_exact_times() {
        let now = at(NOW);
        let exact = DropWindow::manual(now, None).unwrap();
        assert_eq!(exact.start, now);
        assert_eq!(exact.end, now);
        assert!(!exact.in_progress);
        assert!(DropWindow::manual(now, Some(at(NOW - 1))).is_err());
    }
}
