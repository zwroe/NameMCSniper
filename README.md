# NameMCSniper

![NameMCSniper banner](https://github.com/user-attachments/assets/c9ff5adc-7f55-45e2-bad1-11ac2c397dc0)

NameMCSniper is a Rust terminal app for scheduling name changes on existing Minecraft Java Edition profiles. Open the green-on-black menu to preview a schedule, check your accounts, or practice with an offline simulation.

##  Recommended Proxies

[BirdProxies.com](https://www.birdproxies.com/@NAMEMCSNIPER)

Get 10% off + 15% extra data using the link above!

## Get started

On Windows, double-click `namemcsniper.exe` to open the menu. To build it yourself, install stable Rust with rustup. Follow its prompts for the MSVC build tools on Windows. On Debian or Ubuntu, install `pkg-config` and `libssl-dev`. [Rust installation guide](https://rust-lang.org/tools/install/)

From the Rust checkout, build the release executable.

```sh
cargo build --locked --release
```

Open `target\release\namemcsniper.exe` on Windows or run `./target/release/namemcsniper` on Linux or macOS. To put `namemcsniper` on your PATH, run `cargo install --locked --path .` from the checkout.

Rust release ZIPs include the executable, license, README, and starter lists. Choose Windows x64, Linux x64, or macOS ARM64. A branch push also builds test ZIPs under Actions. [GitHub Releases](https://github.com/zwroe/NameMCSniper/releases) · [GitHub Actions](https://github.com/zwroe/NameMCSniper/actions)

### Use the menu

1. Choose **3 · Try an offline simulation** to see a scripted rename result. This uses no account or network connection.
2. Choose **2 · Check my Minecraft account** for one bearer token or **7 · Test bearer tokens (tokens.txt)** for several. For a list, press Enter to load the displayed file. Confirm the profile UUID and rename eligibility.
3. Choose **6 · Check UTC clock** before scheduling an absolute time.
4. Choose **1 · Schedule a name change**, enter the target, and pick a time. Choose **Paste a NameMC drop window or enter a time** for NameMC or manual timing. For a three-character name, the menu also looks up 3NAME and offers any usable future bounds.
5. Review the preview and choose **Continue to live setup**. Paste one token or choose **Use all eligible profiles from tokens.txt**. Check the target, time, and profiles before typing `RENAME`.

For NameMC, paste both **Drop Window** lines or enter a single timestamp such as `11/9/2026 • 2:38:39 PM`. With one timestamp, press Enter at the last-line prompt. Use the time zone NameMC used to display the timestamps. Press Enter for your computer's zone, or enter `UTC` or an offset such as `-05:00`.

The menu previews both future bounds in UTC when they are at least 10 seconds apart. If Minecraft reports the name unavailable at the first attempt, or the first slot is missed before sending, the app counts down to the latest bound and checks your profiles again before dispatch. If the earliest bound has passed or the bounds are closer than 10 seconds, it schedules the latest only. Keep the app running and use tokens that will still be valid at the later time.

### Why Rust

I wanted a Windows executable I could open from Explorer and more control over when the rename request leaves the app. The Rust rebuild prepares requests before the scheduled time, then uses a monotonic timer for the final wait. For an absolute schedule, it checks UTC again if it detects a system clock jump.

## Check accounts and proxies

### Bearer tokens

Use a fresh Minecraft Services bearer token from your own Java Edition account. In your signed-in browser, open the **Network** panel and select **Change Profile Name** under Minecraft Java Edition. Look for `GET /minecraft/profile/namechange` to `api.minecraftservices.com` and copy the value after `Bearer ` from the `Authorization` header if present. Leave the rename form open without submitting it. [Minecraft My Games](https://www.minecraft.net/en-us/msaprofile/mygames)

Paste a single token at the hidden prompt. For multiple accounts, copy `tokens.txt.example` to `tokens.txt` beside the executable and add one token per line. A `Bearer ` prefix is optional. The checker skips blank lines and `#` comments, accepts up to 64 entries, and reports the profile UUID and eligibility for each token it checks. A valid token can belong to a profile on Minecraft's 30-day rename cooldown. [Token template](tokens.txt.example) · [Minecraft Help](https://help.minecraft.net/hc/en-us/articles/4408950195341-View-or-Change-Your-In-Game-Profile-Name-in-Minecraft)

### Proxy tests

Copy `proxies.txt.example` to `proxies.txt` beside the executable and add one HTTP or HTTPS proxy per line, up to 256 entries. Use a bare `host:port`, an HTTP(S) URL such as `http://username:password@host:port`, or `host:port@username:password`. The two forms without a scheme use HTTP. URL-encode reserved characters in proxy credentials. [Proxy template](proxies.txt.example)

```text
proxy.example:8080
http://username:example-password@proxy.example:8080
proxy.example:8080@username:example-password
https://proxy.example:8443
```

Choose **4 · Test proxies**. A passing proxy reached Minecraft Services over HTTPS and returned the expected `401` for an unauthenticated request. The results show elapsed time and redacted addresses. You can export passing entries to a new `proxies_working.txt`. The export retains credentials. Live renames connect directly to Minecraft Services.

The menu checks for `tokens.txt` and `proxies.txt` in the working folder first, then beside the executable. You can enter a different path. If a list is missing, the menu shows the path where you can save it. The shell command `check-tokens` uses the same search order when you omit the path. The shell command `test-proxies` defaults to the working folder. Keep the lists and any exports private.

## Run from a shell

These examples use `namemcsniper` on your PATH. In PowerShell, use `.\namemcsniper.exe` if the executable is in the current folder.

```sh
namemcsniper --help
namemcsniper simulate --scenario rate-limited
namemcsniper drop Ab9
namemcsniper snipe TargetName --at '2030-06-01T18:00:00.250Z'
namemcsniper check
namemcsniper check-tokens tokens.txt
namemcsniper test-proxies proxies.txt --output proxies_working.txt
namemcsniper clock-check
namemcsniper benchmark --samples 100
```

`simulate`, `benchmark`, and `snipe` without `--live` run offline. `drop NAME` looks up a three-character name on 3NAME. Use `drop NAME --at TIME` to inspect a manual time offline. Time inputs accept RFC 3339 with `Z` or a numeric offset, such as `2030-06-01T14:00:00.250-04:00`, or Unix milliseconds.

### Schedule a live rename

Replace the example name, time, and UUID with your own. `--live` authorizes a real rename. Select a single Java profile with `--account` and supply its token at the hidden prompt, via `--token-stdin`, or through `NAMEMCSNIPER_TOKEN`. The app verifies the token's profile UUID and rename eligibility before dispatch.

```sh
namemcsniper snipe TargetName --live --account 01234567-89ab-4cde-8fab-0123456789ab --at '2030-06-01T18:00:00.250Z'
```

For a group, choose **Use all eligible profiles from tokens.txt** in the menu and accept the displayed path or enter your own. The app selects each distinct eligible profile from the list of up to 64 token entries and skips anyone who already holds the target name. In scripts, pass `--tokens-file` by itself for the same selection, or add `--account` flags to select a subset. Both forms below use one scheduled slot and dispatch one concurrent request per ready profile.

```sh
namemcsniper snipe TargetName --live --tokens-file tokens.txt --at '2030-06-01T18:00:00.250Z'
namemcsniper snipe TargetName --live --tokens-file tokens.txt --account 01234567-89ab-4cde-8fab-0123456789ab --account 11234567-89ab-4cde-8fab-0123456789ab --at '2030-06-01T18:00:00.250Z'
```

## Timing and live results

- Check the width of the 3NAME estimate before scheduling. Automatic CLI scheduling and explicit `--at`/`--until` ranges accept up to 30 seconds. A single slot in a range uses the latest bound. For wider estimates, the menu can use a future bound as one exact time, or you can supply your own `--at`. [3NAME](https://3name.xyz/) · [MCsniperGO claim-range notes](https://github.com/Kqzz/MCsniperGO#claim-range)
- Absolute live schedules require at least two agreeing public NTP sources. The app refreshes its UTC estimate during long waits and can reuse a recent verified correction through a brief outage. It stops the schedule if it can no longer verify UTC. The menu's two-minute shortcut uses elapsed time.
- Final account checks are scheduled 20 seconds before the first slot for one to four profiles. Larger groups start earlier, up to 2 minutes 45 seconds before the slot for 64 profiles. If less time remains, checks start as soon as possible. Use tokens that will still be valid then and keep the machine awake. The benchmark reports median, 95th percentile, worst local dispatch delay, and missed slots. The drop estimate and network delay also affect when Minecraft processes a request.
- The default is one slot. When you pass exactly one `--account` UUID, `--attempts` allows up to three slots and `--interval` sets the minimum spacing from 10 to 30 seconds. A confirmed unavailable response can advance to the next scheduled slot. The only write is `PUT /minecraft/profile/name/{name}` on Minecraft Services. [MCsniperGO rename call](https://github.com/Kqzz/MCsniperGO/blob/abba9c2f3c16cfdae9c0ee83d0dff3c87769239a/pkg/mc/account.go#L269)
- HTTP 429 during token validation or final account checks stops the run before dispatch. If a group write returns 429, requests already in flight can finish and the app skips follow-up profile reads. The app shows `Retry-After` when Minecraft provides it. The CLI returns `3` for a rate-limited check or a final rate-limit outcome. Live runs return `4` for an uncertain outcome and `130` when cancelled. Check the affected profile before trying again if the result is uncertain or you cancel during a request.

Live runs print a log path with the schedule, clock checks, dispatch timing, and per-profile results. On Windows, the default directory is `%LOCALAPPDATA%\NameMCSniper\logs`. Logs exclude bearer tokens and proxy credentials.

## Moving from Python

Use the guided menu in place of `python Main.py easy`. For scripts, replace `python Main.py snipe-at` with `namemcsniper snipe NAME --at TIME`. You can reuse the old `tokens.txt` and HTTP(S) `proxies.txt` files in the new checkers. [Python v3.0.0 release](https://github.com/zwroe/NameMCSniper/releases/tag/v3.0.0)

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

GitHub Actions runs these checks on Linux, Windows, and macOS. [MIT license](LICENSE)

To publish a Rust release, push a new tag matching the version in `Cargo.toml`, such as `v4.0.0-rc.1`. The release workflow builds all three ZIPs, extracts each one, and runs `simulate` from the extracted binary before publishing the packages and `SHA256SUMS`. It marks release candidate tags as prereleases. [Release workflow](.github/workflows/release.yml)
