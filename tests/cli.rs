use std::fs::{self, OpenOptions};
use std::io::Write;
use std::process::{self, Command, Output, Stdio};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_namemcsniper"))
        .args(args)
        .env_remove("NAMEMCSNIPER_TOKEN")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn run_menu(input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_namemcsniper"))
        .arg("menu")
        .env_remove("NAMEMCSNIPER_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn menu_previews_a_manual_schedule_without_credentials() {
    let output = run_menu("1\nTest_Name\n2\n2030-06-01T14:00:00.250-04:00\n0\n0\n");
    assert!(output.status.success());
    let preview = String::from_utf8(output.stdout).unwrap();
    assert!(preview.contains("Offline preview"), "{preview}");
    assert!(preview.contains("2030-06-01T18:00:00.250Z"), "{preview}");
}

#[test]
fn menu_previews_both_bounds_of_a_pasted_namemc_window() {
    let output = run_menu(
        "1\r\nTest_Name\r\n2\r\nDrop Window\r\n11/9/2035 • 2:38:39 PM\r\n\
         11/11/2035 • 10:18:24 AM\r\n-05:00\r\n0\r\n0\r\n",
    );
    assert!(output.status.success());
    let messages = String::from_utf8(output.stdout).unwrap();
    let (_, preview) = messages
        .split_once("Offline preview")
        .unwrap_or_else(|| panic!("Expected offline preview in {messages}"));
    assert!(!messages.contains("Window time >"), "{messages}");
    assert!(
        preview.contains("Attempt 1  2035-11-09T19:38:39Z"),
        "{messages}"
    );
    assert!(
        preview.contains("Attempt 2  2035-11-11T15:18:24Z"),
        "{messages}"
    );
}

#[test]
fn menu_previews_a_single_namemc_timestamp_in_utc() {
    let output = run_menu("1\nTest_Name\n2\n11/9/2035 • 12:03:01 AM\n\nUTC\n0\n0\n");
    assert!(output.status.success());
    let messages = String::from_utf8(output.stdout).unwrap();
    let (_, preview) = messages
        .split_once("Offline preview")
        .unwrap_or_else(|| panic!("Expected offline preview in {messages}"));
    assert!(
        preview.contains("Attempt 1  2035-11-09T00:03:01Z"),
        "{messages}"
    );
    assert!(!preview.contains("Attempt 2"), "{messages}");
}

#[test]
fn menu_previews_only_the_latest_bound_after_the_window_starts() {
    let output =
        run_menu("1\nTest_Name\n2\n1/1/2000 • 12:00:00 AM\n11/11/2035 • 10:18:24 AM\nUTC\n0\n0\n");
    assert!(output.status.success());
    let messages = String::from_utf8(output.stdout).unwrap();
    let (_, preview) = messages
        .split_once("Offline preview")
        .unwrap_or_else(|| panic!("Expected offline preview in {messages}"));
    assert!(
        preview.contains("Attempt 1  2035-11-11T10:18:24Z"),
        "{messages}"
    );
    assert!(!preview.contains("Attempt 2"), "{messages}");
}

#[test]
fn closed_input_exits_the_menu_and_defaults_to_help_without_arguments() {
    assert!(run(&["menu"]).status.success());

    let output = run(&[]);
    let messages = String::from_utf8([output.stdout, output.stderr].concat()).unwrap();
    assert!(messages.contains("Usage:"), "{messages}");
}

#[test]
fn menu_refuses_to_authenticate_over_a_pipe() {
    let output = run_menu("1\nTest_Name\n1\n1\n0\n");
    let messages = String::from_utf8([output.stdout, output.stderr].concat()).unwrap();
    assert!(
        messages.to_ascii_lowercase().contains("terminal"),
        "{messages}"
    );
}

#[test]
fn proxy_test_entry_points_report_invalid_entries_without_echoing_credentials() {
    let path = std::env::temp_dir().join(format!("namemcsniper-proxy-test-{}.txt", process::id()));
    let mut fixture = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    writeln!(
        fixture,
        "http://fixture-user:fixture-private-token@127.0.0.1:not-a-port"
    )
    .unwrap();
    drop(fixture);
    let path_arg = path.to_str().unwrap();

    let cli = run(&["test-proxies", path_arg]);
    let menu = run_menu(&format!("4\n{path_arg}\n0\n"));
    fs::remove_file(&path).unwrap();
    let missing = run(&["test-proxies", path_arg]);

    assert_eq!(cli.status.code(), Some(1));
    assert!(menu.status.success());
    for output in [cli, menu] {
        let messages = String::from_utf8([output.stdout, output.stderr].concat()).unwrap();
        assert!(
            messages.to_ascii_lowercase().contains("invalid"),
            "{messages}"
        );
        assert!(!messages.contains("fixture-user"), "{messages}");
        assert!(!messages.contains("fixture-private-token"), "{messages}");
    }
    assert_eq!(missing.status.code(), Some(1));
    let error = String::from_utf8(missing.stderr).unwrap();
    assert!(error.to_ascii_lowercase().contains("proxy list"), "{error}");
}

#[test]
fn labels_a_simulated_rename_as_simulated() {
    let output = run(&["simulate"]);
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("[simulated] Name changed.")
    );
}

#[test]
fn checks_a_token_file_without_echoing_invalid_credentials() {
    let path = std::env::temp_dir().join(format!("namemcsniper-token-test-{}.txt", process::id()));
    let mut fixture = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    writeln!(fixture, "Bearer fixture-private invalid token").unwrap();
    drop(fixture);

    let output = run(&["check-tokens", path.to_str().unwrap()]);
    fs::remove_file(&path).unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report = String::from_utf8([output.stdout, output.stderr].concat()).unwrap();
    assert!(report.contains("Line 1"), "{report}");
    assert!(!report.contains("fixture-private"), "{report}");
}

#[test]
fn rejects_an_invalid_group_before_loading_credentials() {
    let first = "01234567-89ab-4cde-8fab-0123456789ab";
    let second = "11234567-89ab-4cde-8fab-0123456789ab";
    let base = [
        "snipe",
        "Test_Name",
        "--live",
        "--at",
        "2030-06-01T18:00:00Z",
        "--account",
        first,
    ];
    for (extra, expected) in [
        (&["--account", first][..], "each Java profile only once"),
        (&["--account", second][..], "Use --tokens-file"),
        (
            &[
                "--account",
                second,
                "--tokens-file",
                "missing.txt",
                "--attempts",
                "2",
            ][..],
            "one scheduled attempt per selected profile",
        ),
    ] {
        let args = [&base[..], extra].concat();
        let output = run(&args);
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn live_token_file_can_select_all_profiles_without_account_flags() {
    let path = std::env::temp_dir().join(format!("namemcsniper-missing-all-{}.txt", process::id()));
    let args = [
        "snipe",
        "Test_Name",
        "--live",
        "--tokens-file",
        path.to_str().unwrap(),
        "--at",
        "2030-06-01T18:00:00Z",
    ];
    let output = run(&args);
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("Could not open the token list"), "{error}");

    let repeated = [&args[..], &["--attempts", "2"]].concat();
    let output = run(&repeated);
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("one scheduled attempt per selected profile"),
        "{error}"
    );
}

#[test]
fn benchmark_reports_each_slot_as_observed_or_missed() {
    let output = run(&["benchmark", "--samples", "3"]);
    assert!(output.status.success());
    let report = String::from_utf8(output.stdout).unwrap();
    let counts = report
        .lines()
        .find_map(|line| line.strip_prefix("Scheduled 3  Observed "))
        .expect("benchmark reports its sample counts");
    let (observed, missed) = counts.split_once("  Missed ").unwrap();
    let observed: u16 = observed.parse().unwrap();
    let missed: u16 = missed.parse().unwrap();
    assert_eq!(observed + missed, 3, "{report}");
    if observed > 0 {
        assert!(report.contains("Local dispatch delay"), "{report}");
    }

    let invalid = run(&["benchmark", "--samples", "0"]);
    assert_eq!(invalid.status.code(), Some(2));
}

#[test]
fn previews_a_schedule_without_credentials() {
    let output = run(&[
        "snipe",
        "Test_Name",
        "--at",
        "2030-06-01T14:00:00.250-04:00",
        "--until",
        "2030-06-01T14:00:25.250-04:00",
        "--attempts",
        "3",
    ]);
    assert!(output.status.success());
    let preview = String::from_utf8(output.stdout).unwrap();
    assert!(preview.contains("Offline preview"));
    assert!(preview.contains("2030-06-01T18:00:00.250Z"));
    assert!(preview.contains("2030-06-01T18:00:12.750Z"));
    assert!(preview.contains("2030-06-01T18:00:25.250Z"));
}

#[test]
fn cli_rejects_an_until_window_wider_than_30_seconds() {
    let output = run(&[
        "snipe",
        "Test_Name",
        "--at",
        "2030-06-01T18:00:00Z",
        "--until",
        "2030-06-01T18:00:31Z",
        "--attempts",
        "2",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("wider than 30 seconds"), "{error}");
}

#[test]
fn live_mode_requires_an_account_and_rejects_a_past_time_before_preflight() {
    let missing = run(&["snipe", "Test_Name", "--live"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(
        String::from_utf8(missing.stderr)
            .unwrap()
            .contains("--account")
    );

    let past = run(&[
        "snipe",
        "Test_Name",
        "--live",
        "--account",
        "01234567-89ab-4cde-8fab-0123456789ab",
        "--at",
        "2020-06-01T18:00:00Z",
    ]);
    assert_eq!(past.status.code(), Some(1));
    assert!(
        String::from_utf8(past.stderr)
            .unwrap()
            .contains("scheduled slots have already passed")
    );
}

#[test]
fn rejects_unsafe_names_and_timezone_free_times() {
    for args in [
        ["drop", "foo/bar", "--at", "2030-06-01T14:00:00Z"],
        ["drop", "GoodName", "--at", "2030-06-01T14:00:00"],
    ] {
        assert_eq!(run(&args).status.code(), Some(2));
    }
}
