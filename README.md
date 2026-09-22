# srun-portal

Clean-room Rust implementation of the Srun portal client protocol, written
against a specification reverse engineered from the `portal.rar` binary
(`portal-core@1.9.15`). That specification is **not** part of this repository;
the `§` references below point into it, and every rule it states that this crate
depends on is pinned by a test here.

The contract this crate aims at is **request equality**: for the same inputs it
must emit the same HTTP requests, byte for byte, as the reference client. Every
cryptographic and query-construction rule is pinned by tests built from the
captured vectors, and the whole flow is differentially tested against the
reference binary (see [Verification](#verification)).

```
cargo build --release
./target/release/srun-portal 'https://net.szu.edu.cn/srun_portal_pc?ac_id=1'   # interactive
cargo run --example live_check -- 'https://net.szu.edu.cn/srun_portal_pc?ac_id=1' [username]
cargo test
```

The portal URL argument is optional: `argv[1]`, then the `portal_url` key of the
configuration file, then an interactive prompt, in that order (SPEC §3.1). The
configuration lives in `srun-portal.toml` — see
[Configuration](#configuration) for where that file is looked for.

## Layout

| module | SPEC | responsibility |
|---|---|---|
| `crypto` | §6 | variant base64 (`btoa`/`atob`), the XXTEA `XEncode` variant, HMAC-MD5, SHA-1, the `{SRBX1}` `info` payload |
| `transport` | §4, §9 | `URLSearchParams` encoding, JSONP transport, endpoint URLs with the v4/v6 host swap |
| `html` | §3.2 | the `$('#id').html()` subset needed to read the portal page |
| `portal_config` | §3.2 | `PortalConfig` / `PortalFlags` decoded from that page |
| `api` | §4, §7, §8 | request construction and sending for every endpoint |
| `translate` | §10 | error-code to message dispatch |
| `config` | §3.1 | the legacy `~/.srun_portal.json` (read-only), portal-URL validation/resolution |
| `settings` | §3.1 | configuration file: discovery, layering, first-run creation |
| `credentials` | — | where the unattended password is kept: the system facility, or a `0600` file |
| `keyring` | — | the three credential facilities: Secret Service, Credential Manager, keychain |
| `sys` | §3.1, §3.3, §7.1 | passwd lookup, interface enumeration, device identity, SIGINT, stdin prompts |
| `util` | §1, §8 | `wait`, `promiseAny` |
| `format` | §1 | `formatFlow`, `formatTime` |
| `runtime` | §5, §8, §9 | the authentication state machine |
| `reconnect` | §5, §8 | one non-interactive "check, log in when needed" pass |
| `service` | — | the background task per system: systemd, launchd, Task Scheduler |
| `cli` | §5, §10 | interactive shell: banner, prompts, retry loop |

`src/main.rs` is the `srun-portal` binary; `examples/live_check.rs` performs the
read-only live checks of SPEC §13.3.

Dependencies: `md-5`, `sha1`, `hex`, `serde_json`, `toml` (configuration file),
`directories` (the per-platform configuration directory), `ureq` (blocking HTTP,
rustls TLS), `libc` (unix only: passwd entry, `getifaddrs`, termios, signals) and
`windows-sys` (windows only: `GetAdaptersAddresses`, the console mode, the
console control handler, the Credential Manager). No crate is used for the
credential facilities: Linux and macOS go through the `secret-tool` and
`security` CLIs, which is what keeps them wallet-agnostic and the dependency set
this small. No base64 or XXTEA crate is used either: SPEC §11 requires the
variant algorithms of §6.1/§6.2, not the standard ones.

## Fidelity: SPEC §12 item by item

| # | item | disposition |
|---|---|---|
| 1 | `rad_user_info` without `sysver` crashes the reference | **deliberate deviation**: `apiVersion` becomes `None` instead of throwing; the derivation (`sysver.split('.')[2]`, else `srun_ver.split(' ')[2]` minus `B`) is otherwise exact |
| 2 | `os` / `name` semantics inverted | reproduced: `os=os.type()` (`Linux`), `name=os.platform()` (`linux`) |
| 3 | `XEncode` is lossy for non-ASCII | reproduced (`s()` consumes UTF-16 code units, `l()` emits their bytes); a test pins the lossiness |
| 4 | `hmac` is HMAC-MD5, not `md5(pwd + token)` | reproduced; the captured `password` vector is the test |
| 5 | `acid` comes from the HTML, not the URL | reproduced; reproduced live too (`ac_id=1` in the URL → `acid=12` from the page) |
| 6 | double-stack authentication is really serial | reproduced: local stack first, then the other stack, local result returned; a test asserts the order and the `double_stack` flag per stack |
| 7 | `time` is always whole seconds | reproduced: `time` is an integer second and the same string feeds the query and the `sign` |
| 8 | `getNotice`'s parameter is literally named `per-page=` | reproduced, including the doubled `=` (`/v2/srun_portal_message?per-page==100`) |
| 9 | `alert()` is undefined under Node, so three double-stack branches throw | not reproduced (there is no `alert` analogue here); a misconfigured double stack simply degrades to the SPEC §9 condition |
| 10 | `getUserOtherStackIP()` is fire-and-forget, so the first double-stack decision races it | reproduced: the probe is a detached thread writing a shared slot that the decision reads without waiting |
| 11 | authentication failure re-prompts forever | reproduced by the CLI loop (`Auth failed: …` then prompt again until SIGINT) |
| 12 | `createEnvFile` only creates, `updateENV` always overwrites | **deliberate deviation**: `~/.srun_portal.json` is no longer written. The `portal_url` key of the [configuration file](#configuration) records the URL instead, and the legacy file is only read — as a seed for a first-run configuration and as a last-resort URL |
| 13 | `encode('') === ''`, so `info` would be `"{SRBX1}"` | the primitive short-circuits are reproduced (`xencode("")`/`btoa("")` are empty), but SPEC §6.4's own composition cannot reach it: `JSON.stringify({…})` is never the empty string. Recorded rather than faked |
| 14 | `signOutNormal` exists but the CLI only uses `signOutDM` | reproduced: `Runtime::sign_out_normal` is public API, `Runtime::sign_out` is the DM path the CLI uses |

## Other decisions worth knowing

* **HTTP timeouts**: the spec fixes none; this client uses 5 s connect / 10 s
  read so a dead portal cannot hang the state machine. A configuration file can
  replace both (`connect_timeout_ms`, `read_timeout_ms`), which is a startup
  setting: it applies from the first request on.
* **Salt expiry** (SPEC §4/§15.3 item 8): when `get_challenge` returns `expire`
  and the window has elapsed before the login request is built, the salt is
  re-fetched and the request rebuilt. The live portal does not currently send
  `expire` (observed), so the check is inert there and the reference's
  "assume it is fast enough" behaviour is preserved.
* **Salt validation**: `challenge` must be a string; a missing one is reported
  instead of being silently hashed into a wrong password.
* **`usernameWithDomain`**: `user_name` plus `@domain`, and the bare `user_name`
  when the portal reports an empty domain. The captured DM vector
  (`…10.9.9.9…` with `username=testuser`) is only reproducible with the bare
  form, so this is evidence-backed rather than a preference.
* **Domain input**: the interactive username field accepts `user@domain`;
  everything after the first `@` is passed to `authByPassword` as the domain.
  `PortalError::DomainNeedsAt` stays for callers that pass a malformed domain.
* **`#showInfoList` is ignored by the CLI**, as the reference does (it renders a
  fixed panel); the field is still parsed and exposed for callers.
* **Notice and agreement endpoints** (`/v2/srun_portal_message`,
  `/v1/srun_portal_agree_new`) are implemented in `api` but are **not** issued
  by this flow: the reference does not request them during login or sign-out,
  and issuing extra requests would break request-set equality. SPEC §10's
  `Get notice failed` / `Get protocol failed` strings are kept in
  `messages` for that surface.
* **Error text catalogue**: `lang/*.js` is data (SPEC Appendix B), so the
  dictionary starts empty and `Translate` falls back to the raw string; the
  dispatch algorithm (precedence, `E2901`, `E0000`, 5-character truncation,
  `messageFormat`, lookup order) is complete and tested. Populate with
  `Translate::insert` / `with_dictionary` for localised text.
* **`atob`** is only used by tests; SPEC §6.1 does not fully pin the
  out-of-range behaviour, so it stops at the first symbol outside the alphabet.
* **Presentation**: the banner, the 25-character panel, its labels
  (`Username`, `IP`, `Used Flow`, `Used Time`, `Balance`, `Product Name`),
  `Auth Success!`, `Sign out success!`, `SignOut failed: …`,
  `Auth failed: …` and the prompt wordings follow the reference's observable
  output; the spinner frames, emoji markers, ANSI colours and the cursor
  rewrites of its prompt library are not reproduced. Nothing there carries
  protocol meaning.
* **Formatters**: `formatFlow` / `formatTime` were matched to the reference's
  output by a black-box probe (binary units, two decimals, `0 B`; `小时`/`分`/`秒`
  parts, hours never folded into days, `0 秒` for zero). The probe table is the
  test data in `format.rs`.
* **Configuration file path**: `srun-portal.toml` follows each platform's
  directory convention and can be shadowed by the file next to the executable
  (see [Configuration](#configuration)). The legacy `~/.srun_portal.json` is
  still located through the passwd entry of the real uid (SPEC §3.1's
  recommendation), with a `$HOME` fallback when there is no passwd entry.

## Configuration

The client reads at most two files, both named `srun-portal.toml`:

| location | path |
|---|---|
| **system** | `$XDG_CONFIG_HOME/srun-portal/srun-portal.toml` (Linux, `~/.config` by default), `~/Library/Application Support/srun-portal/srun-portal.toml` (macOS), `%APPDATA%\srun-portal\config\srun-portal.toml` (Windows) |
| **project** | next to the executable — the portable deployment |

The project file is the project-level configuration: when it exists it overrides
the system file key by key, so a deployment can pin one or two keys without
copying the user's settings. The URL a run resolves is recorded in the file that
wins (the project file when there is one), so the next run reads the same portal
back. `--config <path>` (or `SRUN_PORTAL_CONFIG`) replaces both: that file is
then the whole configuration, and is created when it is missing.

When neither file exists the CLI asks where to put one — next to the executable
or in the system directory — and remembers the answer by creating the file. That
question is skipped without a terminal (a piped stdin chooses the system
directory silently) and by `--portable`. A legacy `~/.srun_portal.json` is
imported as the initial `portal_url` at that point, and is never written any
more.

```toml
# srun-portal configuration.
#   portal_url          full portal URL, e.g. "https://net.szu.edu.cn/srun_portal_pc?ac_id=1"
#   username            account name used as the default for the login prompt
#   domain              domain suffix used when the account carries none (with or
#                       without '@'; empty or unset means no suffix, which is what
#                       the portal's own capture does)
#   callback            JSONP callback name (default "jsonp")
#   connect_timeout_ms  HTTP connect timeout in milliseconds (default 5000)
#   read_timeout_ms     HTTP read timeout in milliseconds (default 10000)
#   reconnect_interval_secs   background reconnect interval in seconds (default 300; 0: no interval)
#   reconnect_boot_delay_secs delay after startup before the first check (default 30; 0: not at startup)
```

Every key is optional, unknown keys are preserved when the file is rewritten,
and the file is only rewritten when a value actually changed. A file this crate
**creates** is seeded with those defaults written out — the timeouts, `domain`
(empty), and both backgrounds at `0` — so a fresh configuration shows every key
and what it does rather than being an empty document.

### Editing the configuration

Every key can be read and written through subcommands, so the file does not have
to be edited by hand:

```
srun-portal config show                          # every effective setting, with a one-line description
srun-portal config list                          # only the keys the file itself sets
srun-portal config set username testuser           # one setting
srun-portal config set domain szu reconnect_interval_secs 300   # several at once
srun-portal config unset domain                  # fall back to the default
```

`show` prints the **effective** settings — what the client will actually use
after layering and defaults — while `list` prints what this one file contains.
`set` validates both the key name and the value's type before writing anything,
so a typo leaves the file untouched: an unknown key or a non-numeric
`connect_timeout_ms` is a usage error (`exit 2`), not a corrupt file. `unset`
removes the keys named, and refuses names it does not know.

The file written is the one the run resolves to (the project file when there is
one, else the system file) — the same file the interactive flow records the
resolved portal URL in, and the same one `--config` addresses. `set`/`unset`
never touch the keyring or the stored password; `service forget` is what removes
that.

`domain` is the suffix appended to an account that carries none. The empty
default is the evidenced one: the portal's own captured login sends a bare
`user_name` (`username=testuser`, no `@…`), and the page's `#domain` element is
not what fills this in — it holds the portal's domain *menu*
(`{"@hlw":"互联网","@xyw":"校园内网"}`) and is parsed but unused by the flow, while
this key is what the interactive prompt accepts as `user@domain`. A portal that
requires a suffix therefore needs it set here; the default does not guess one.

## Background reconnect

`reconnect` is the interactive flow minus the interaction: it fetches the portal
configuration, asks the portal whether this device is already online, and logs
in only when it is not — then exits. It never prompts, so it is safe to run from
a scheduler, and it prints one line per event (`Online.` / `Reconnected.` / the
rendered error) for a journal.

```
srun-portal reconnect                       # one pass; uses the configuration file
srun-portal service install                 # register a task that runs it for you
srun-portal service status                  # what is installed, and what systemd/launchd/schtasks says
srun-portal service uninstall               # remove it
srun-portal service forget                  # delete the stored password
```

`service install` uses each system's own facility, always **per user** — the
portal session, the configuration and the password all belong to one account:

| system | mechanism | artifacts |
|---|---|---|
| Linux | systemd **user** service + timer | `$XDG_CONFIG_HOME/systemd/user/srun-portal.{service,timer}` |
| macOS | launchd **LaunchAgent** | `~/Library/LaunchAgents/com.srun-portal.reconnect.plist` |
| Windows | Task Scheduler (`schtasks.exe`) | `srun-portal-reconnect` (interval) and `srun-portal-reconnect-boot` (at logon) |

The schedule defaults to **on** — `reconnect_interval_secs = 300` and
`reconnect_boot_delay_secs = 30` — because installing a task that never runs
would defeat the point. Each half is independently switchable, and a `0` (or
removing the key) turns that half off:

| `reconnect_interval_secs` | `reconnect_boot_delay_secs` | what is installed |
|---|---|---|
| unset (300) | unset (30) | both |
| set | `0` | the interval alone |
| `0` | set | the startup run alone |
| `0` | `0` | the task only, started by hand (`systemctl --user start srun-portal.service`, `launchctl kickstart`, `schtasks /Run`); Linux writes no timer at all, see below |

Two measured caveats are baked into the Linux units. `OnUnitActiveSec=` (the
interval) **never arms on its own** — a timer whose only trigger counts from the
previous run has nothing to count from, and `list-timers` reports an empty
`NEXT` after `enable --now` — so an interval-only timer also gets
`OnActiveSec=1s`, which *does* arm from timer activation. And a timer with **no
trigger at all is rejected outright** (`Timer unit lacks value setting`, landing
in `LoadState=bad-setting` with `systemctl start` failing too), and no
placeholder date fixes it: a far-future `OnCalendar` does not even parse. So when
both cadences are off, Linux installs **only the service** and no timer, and the
service starts fine by hand. A startup run uses
`OnStartupSec=` (which `systemd.timer(5)` documents as "relative to when the
service manager was started" — with linger enabled that is boot, with nobody
logged in); macOS uses `StartInterval` / `RunAtLoad`; Windows creates one task
per enabled schedule (`/SC MINUTE`, `/SC ONLOGON`), because `schtasks` cannot
express both as one trigger. Windows counts minutes, so its interval rounds
**up** to one minute.

Installing a task does not by itself change any login-wide setting: the Linux
`loginctl enable-linger` step is skipped when neither cadence is set, since
there would be no automatic start for it to serve.

Unattended runs need a password, so it is the one thing that *is* stored — in the
system's own credential facility, which is what exists for exactly this:

| system | facility | reached through |
|---|---|---|
| Linux | Secret Service (`org.freedesktop.secrets`) | `secret-tool` (libsecret) — so KWallet, gnome-keyring or ksecretd, whichever the user already runs |
| Windows | Credential Manager | `CredReadW` / `CredWriteW` / `CredDeleteW` |
| macOS | login keychain | `security(1)` |

The entry is keyed by `service=srun-portal` / `account=<username>`, and nothing
is written to the configuration directory. `install` asks for the password once
and proves it with a dry run before storing anything. `SRUN_PORTAL_PASSWORD`
overrides whatever is stored, and `service forget` deletes it.

Where no facility can be used — a headless machine with no session bus, for
instance — the password falls back to a `0600` file beside the configuration,
`srun-portal.credentials.toml`, and **says so**, naming the reason. That file is
the only fallback; the configuration file itself never holds a password.
`service status` reports which of the two is in use, or that the environment
variable is overriding both. `service uninstall` leaves the stored password
alone (`service forget` is the command that removes it).

Starting before a login needs a privilege step, which is reported rather than
taken: Linux uses `loginctl enable-linger <user>` (attempted unprivileged by
`install`, printed as a `pkexec …` command when refused), macOS would need a
root LaunchDaemon, Windows an `/SC ONSTART` administrator task.

## Out of scope

SPEC §15.3's borrow list is **not** implemented — it is explicitly beyond the
specification's behaviour contract: interface binding (`SO_BINDTODEVICE`),
explicit auth-server IP override, the dorm `eportal` protocol family, UA
spoofing, and the captcha probe. The [configuration file](#configuration)
carries the portal URL, the account conveniences, the transport timeouts and the
[background task](#background-reconnect) cadence only — not a device, an
interface or an `ac_id` override, which remain out of scope. The crate is built
so those can be added without touching the protocol layer (`Endpoints` already
models per-stack authorities; `RuntimeOptions` is plain data).

The background task is deliberately **not** a resident daemon: `reconnect` is a
one-shot process the platform's own scheduler repeats (SPEC §15.3's "resident
monitor/reconnect daemon" borrow is exactly the thing this avoids). There is no
long-lived event loop to supervise, a crash is just a missed tick, and the
system that already manages startup and intervals does that work.

## Clean-room statement

The implementation was written from that specification alone. The reference
binary was only ever *executed* (differentially, and to read its terminal
output); its payload was not inspected, no extracted source was consulted, and
no third-party client's code was read or copied.

## Verification

### 1. Unit and integration tests

```
$ cargo test --all-targets
running 131 tests  ... test result: ok. 131 passed; 0 failed
running  10 tests  ... test result: ok.  10 passed; 0 failed   (tests/flow.rs)
running  12 tests  ... test result: ok.  12 passed; 0 failed   (tests/vectors.rs)
```

* `tests/vectors.rs` pins the captured values: the `password` HMAC, the `info`
  blob (so the whole base64/XXTEA/JSON chain), the `chksum`, the DM `sign`, the
  **complete login query string** and the complete DM query string, plus the
  §7.1 field order.
* `tests/flow.rs` runs the state machine against an in-process stub portal
  (std only): configuration page → online probe → `get_challenge` → `login` →
  online re-check → DM sign-out, asserting the raw queries actually sent, the
  §8 retry path (`Login failed` after three checks), the serial double-stack
  authentication order, the double-stack sign-out race, and a configured
  `callback` reaching every JSONP request (the stub echoes it, so a mismatch
  would not parse). Three further cases drive
  [`reconnect::reconnect_once`](src/reconnect.rs) the same way — the login it
  sends is byte-identical to the interactive one, an online device sends no
  login at all, and a portal that never reports the session online surfaces
  `Login failed`.
* `service`'s three platform constructors are pure data, so the exact systemd
  units, the launchd plist and every `schtasks` command line are asserted
  byte-for-byte on **any** host, together with the quoting rules, the
  file/command executor (`install`/`uninstall`/`status`), and all four schedule
  modes — including the `OnActiveSec=` that an interval-only timer needs.
* Module tests cover the crypto variants (against a reference base64 encoder),
  HTML extraction, config decoding, URL validation, the translate algorithm, the
  formatters, the credentials store (the keyring-first choice, the file
  fallback, `revoke` clearing both, mode `0600`, round trip), the credential
  facilities themselves (the argv each backend builds, plus a round trip against
  the machine's real wallet when it has one — the test skips itself when
  `availability()` says there is none), and the configuration file (layering,
  first-run creation, legacy import, unknown-key preservation, the
  unwritable-portable fallback, and the key catalogue `config set`/`unset`
  validate against — including that every catalogue key parses and that
  unsetting returns a key to its documented default).

### 2. Differential against the reference binary

Same stub portal, same inputs, both clients driven over a PTY, every request
compared as a raw query string. Login flow and already-online sign-out flow:

```
path sequence  reference=['/srun_portal_pc','/cgi-bin/rad_user_info','/cgi-bin/rad_user_info',
                          '/cgi-bin/get_challenge','/cgi-bin/srun_portal','/cgi-bin/rad_user_info']
               rust     =[ identical, in the same order ]
  /cgi-bin/get_challenge     IDENTICAL
  /cgi-bin/rad_user_info     IDENTICAL   (including the parameter-less other-stack probe)
  /cgi-bin/srun_portal       IDENTICAL   (action=login&username=…&…&callback=jsonp)
  /srun_portal_pc            IDENTICAL   (ac_id=1&theme=app)

path sequence  reference=['/srun_portal_pc','/cgi-bin/rad_user_info','/cgi-bin/rad_user_info',
                          '/cgi-bin/rad_user_dm','/cgi-bin/rad_user_info']
               rust     =[ identical, in the same order ]
  /cgi-bin/rad_user_dm       IDENTICAL   (ip=…&username=…&time=…&unbind=1&sign=…&callback=jsonp)
  -> IDENTICAL request stream
```

Reproduce by backing up `~/.srun_portal.json` first — the reference client
rewrites it (this one no longer does, it only reads it as the first-run seed) —
then driving both clients against one stub portal that records every raw query
(it must serve the endpoints in the layout table above), with the same
`ac_id=1&theme=app` URL as the argument. Compare the recorded raw strings per
path.

### 3. Live read-only checks (real portal)

```
$ cargo run --example live_check -- 'https://net.szu.edu.cn/srun_portal_pc?ac_id=1' <account>
slug         : https://net.szu.edu.cn/srun_portal_pc?ac_id=1
origin       : https://net.szu.edu.cn
acid         : 12                       <- the HTML's #acid, not the URL's ac_id=1
ip (server)  : 10.9.9.9                 <- redacted; the real output carried a campus address
lang         : zh-CN
isIPv6       : false
DoubleStackPC: false
MacAuth      : true
AuthIP/AuthIP6:  /
AccountFilter: ""
get_challenge: 64 chars (expire=None)
rad_user_info: error="ok" user="testuser" online_ip="10.9.9.9" sysver=1.01.20260320 sum_bytes=57168504818 apiVersion=20260320
```

No login or sign-out was performed against the real portal.

Without a portal-URL argument the example reads `portal_url` from the
configuration file (falling back to the URL above); it never creates, asks for
or writes anything, so a read-only run leaves no file behind.

### 4. Background reconnect

Against the stub portal (recorded raw queries), `reconnect` behaves as the
interactive flow does and only logs in when needed:

```
$ SRUN_PORTAL_PASSWORD=testpass srun-portal --config …/srun-portal.toml reconnect
Config: /tmp/srun-reconnect/cfg/srun-portal.toml
Reconnected.                       # one login; the query is the captured vector, byte for byte
$ SRUN_PORTAL_PASSWORD=testpass srun-portal --config …/srun-portal.toml reconnect
Online.                            # already online: no login request at all
$ SRUN_PORTAL_PASSWORD= srun-portal --config …/srun-portal.toml reconnect
No stored password: run `srun-portal service install` or set SRUN_PORTAL_PASSWORD
$ echo $?                          # 1
```

`service install` was exercised against a real per-user systemd on Linux
(systemd 261), across all four schedule modes. With both cadences set, the two
units were written with the configured cadence, the timer listed a next run, the
service ran from the timer (`Result=success`, `ExecMainStatus=0`,
`Reconnected.`/`Online.` in the journal, one login on the stub), `service status`
reported `up to date` plus `is-enabled`/`is-active`/`list-timers`, and
`service uninstall` removed both units and restored the unit directory to its
previous state.

Two behaviours in this area were **measured rather than assumed**, and both
changed the code:

* an interval-only timer needs `OnActiveSec=`: with `OnUnitActiveSec=` alone,
  `enable --now` leaves `list-timers` showing an empty `NEXT` and the service
  never runs. With `OnActiveSec=1s` the same timer arms (observed `NEXT` ~1 s
  away). A probe of exactly that setup is what found it.
* `systemctl --user disable --now` is **not** idempotent — on an inactive unit it
  exits `5` ("Unit … not loaded"), while plain `disable` exits `0` every time and
  still removes the `timers.target.wants` symlink. Uninstall therefore uses
  `disable` alone (removing the files and reloading afterwards leaves the timer
  `failed`/`not-found` and not firing, which was verified by deleting the units
  under a running timer).

With both cadences off (the default) the timer is written with no triggers and
left `disabled`/`inactive` — a stale `timers.target.wants` symlink from a
previous install is removed — while `systemctl --user start srun-portal.service`
still runs it by hand (`Result=success`) and reports
`Linger: not changed - nothing runs automatically`. With them set,
`loginctl enable-linger` is enabled and was disabled again by hand afterwards
(the feature needs it; the test host did not).

The credential facility was exercised on that same host, whose session bus
serves `org.freedesktop.secrets` (ksecretd, KWallet 6). Four things were
observed, not assumed:

* the **store** path put the password in the wallet and left the configuration
  directory without any credentials file — `service install` printed
  `Password: stored in the keyring (testuser)`;
* the **read** path reached it from the background task: `reconnect` with no
  `SRUN_PORTAL_PASSWORD` and no file logged `Reconnected.`, and the journal of a
  unit started by the timer shows the same, with a real login on the stub;
* the **fallback** path works when the facility is unusable — with
  `DBUS_SESSION_BUS_ADDRESS` pointed at a nonexistent socket, `install` reported
  the reason, wrote `srun-portal.credentials.toml` mode `0600`, and the
  following runs read the password from that file;
* **`service forget`** deleted the entry (the wallet lookup then exited `1`) and
  `reconnect` went back to reporting `No stored password`.

The macOS and Windows paths cannot be *built* on the Linux host this was
developed on: `ring`'s build script needs the target's C toolchain, so
`cargo check --target x86_64-apple-darwin` stops at `cc -arch` and
`--target x86_64-pc-windows-msvc` at a missing `lib.exe`. Those modules were
instead type-checked in isolation against their real dependencies and the
target's `std` — `windows-sys` for the Win32 probes and the Credential Manager,
`libc` for the unix ones — which is what the `#[cfg]` split exists for. Their
*artifacts* are asserted byte-for-byte by the unit tests above; what remains
unverified for them is execution on the platform itself, not the code's shape.

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE).

The specification this crate was written against is not distributed here; the
protocol itself is the portal operator's, and nothing in this repository is
derived from another client's source.
