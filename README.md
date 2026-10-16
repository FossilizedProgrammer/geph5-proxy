# geph5-proxy

**English** | [فارسی](README.fa.md) | [Русский](README.ru.md)

A standalone Geph5 VPN client that, instead of taking over your whole system
like a VPN, serves a plain **local SOCKS5 and/or HTTP proxy**. Create an
account, pick a country, point your apps at the proxy — done.

Works on Linux and Windows. Binaries are fully static on Linux.

> **About this repository:** it contains the full Geph5 source tree (broker,
> bridges, exits, libraries — see [README.geph5.md](README.geph5.md) for the
> upstream project description) plus the `binaries/geph5-proxy` tool that this
> README documents. Download prebuilt binaries from the
> [Releases](../../releases) page; build your own with
> `./build-static.sh all`.

## Provenance and disclaimer

**This is an unofficial client.** It is a modified derivative of the open-source
[Geph5](https://github.com/geph-official/geph5) client (originally written in
Rust by the Geph project), with additional features such as a standalone
SOCKS5/HTTP proxy mode, upstream-proxy chaining, and CLI account management.

- This project is **not affiliated with, endorsed by, or connected to** the
  official Geph project or its operators. Support requests, outages, or policy
  questions about the network itself belong to the official Geph channels.
- The name **"Geph"** is a trademark of its respective owners. Nothing here
  grants any right to use that trademark; this build must not be presented as
  an official Geph product.
- The original Geph5 code is licensed under the **Mozilla Public License 2.0
  (MPL-2.0)**, with some low-level libraries under the **ISC License**. This
  modified version remains subject to those licenses: if you distribute
  binaries or modified versions containing MPL-2.0 code, you must make the
  corresponding modified MPL-2.0 source files available to recipients, and you
  must retain all copyright and license notices. See the
  [upstream repository](https://github.com/geph-official/geph5) for the full
  license texts.
- **No warranty:** the software is provided "as is", without warranty of any
  kind, as disclaimed by the MPL-2.0 and ISC licenses. Use at your own risk.
- Connectivity is provided through the official Geph brokers and exits, which
  this software does not own or control and whose availability, limits
  (including free-tier bandwidth caps), and policies may change at any time.
  Your account code is a bearer secret for that network; treat it accordingly.

## Quick start

```bash
# 1. Create an account (code is saved automatically)
./geph5-proxy signup

# 2. See which countries you can connect to
./geph5-proxy countries            # everything
./geph5-proxy countries --level free   # free-tier exits only

# 3. Connect (SOCKS5 on 127.0.0.1:9909, HTTP on 127.0.0.1:9910)
./geph5-proxy run --country NL

# 4. Use it
curl --proxy socks5h://127.0.0.1:9909 https://api.ipify.org
curl --proxy http://127.0.0.1:9910   https://api.ipify.org
```

## Commands

### `signup`

Creates a new Geph account. Solves the proof-of-work puzzle, registers, and
prints the account code. The code is saved to the account file (see
`--account-file`) so later commands pick it up automatically.

| Option | Description |
|---|---|
| `--account-file <PATH>` | Where to save the account code |
| `--upstream <URL>` | Reach the broker through an upstream proxy |

### `login`

Validates an account code against the broker and re-saves it.

| Option | Description |
|---|---|
| `--secret <CODE>` | Account code to validate (default: the saved one) |
| `--account-file <PATH>` | File to read the code from / save it back to |
| `--upstream <URL>` | Reach the broker through an upstream proxy |

### `countries`

Lists exit countries, number of exits, whether they are free-tier, and cities.

| Option | Description |
|---|---|
| `--level <all\|free\|plus>` | Only list exits for that account level (default: `all`) |
| `--upstream <URL>` | Reach the broker through an upstream proxy |

### `run`

Connects to a Geph exit and serves a local proxy until interrupted (Ctrl-C).

| Option | Description |
|---|---|
| `--country <CC>` | Two-letter country code (`NL`, `DE`, …) or `auto` (default) |
| `--city <NAME>` | Restrict to a city shown by `countries` (requires `--country`) |
| `--exit <HOST/PUBKEY>` | Connect to one specific exit instead of country selection |
| `--secret <CODE>` | Account code (default: the saved one, then prompts) |
| `--socks <SPEC>` | SOCKS5 listener: `PORT`, `HOST:PORT`, or `none` (default: `9909`) |
| `--http <SPEC>` | HTTP proxy listener: `PORT`, `HOST:PORT`, or `none` (default: `9910`) |
| `--share-lan` | Listen on `0.0.0.0` so other LAN devices can use the proxy |
| `--cache <PATH>` | SQLite cache path (default: `<cache-dir>/geph5-proxy/db`) |
| `--account-file <PATH>` | File to read the account code from |
| `--upstream <URL>` | Chain all outbound traffic through an upstream proxy first |

## Global options

These work before or after any command:

| Option | Description |
|---|---|
| `--account-file <PATH>` | Where the account code is stored/read (default: `<config-dir>/geph5-proxy/account.txt`, mode 600) |
| `--upstream <URL>` | Proxy that all of *Geph's own* traffic (broker + tunnels) goes through first |

### Upstream proxy URL formats

```
socks5://host:port
socks5://user:pass@host:port
http://host:port
http://user:pass@host:port
```

Use this to chain Geph through a proxy you already have (another VPN, Tor,
a corporate proxy, …). Notes:

- The upstream proxy must allow CONNECT (HTTP) or arbitrary destinations
  (SOCKS5) on high ports — Geph bridges do not only use port 443. Some
  restrictive local proxies (e.g. certain Psiphon builds) only allow CONNECT
  to port 443 and will not work for the tunnel itself.
- IPv6 bridge addresses are only usable if the upstream proxy supports IPv6.

## Listening address examples

```bash
# Only this machine (default)
./geph5-proxy run --socks 9909 --http 9910

# Whole LAN (no authentication — anyone on the network can use it!)
./geph5-proxy run --share-lan --socks 1080 --http 8080

# SOCKS5 only, specific address
./geph5-proxy run --socks 192.168.1.9:1080 --http none
```

## Account file

By default the account code lives in `<config-dir>/geph5-proxy/account.txt`
with permissions `0600`. Override the location everywhere with
`--account-file`:

```bash
./geph5-proxy signup --account-file ~/my-geph-account
./geph5-proxy --account-file ~/my-geph-account run --country auto
```

Reading order for `login`/`run`: `--secret` → `--account-file` → default
path → interactive prompt.

## Building from source

```bash
./build-static.sh all      # Linux static + Windows .exe into dist/
PKG=geph5-proxy ./build-static.sh linux
```

Requirements: Rust toolchain, gcc/make/perl; for Windows cross-builds also
mingw-w64 (the script bootstraps the Rust std source itself).

## Security notes

- `--share-lan` exposes an **unauthenticated** proxy to your whole LAN.
- The account code is a bearer secret: anyone with it controls the account.
- This is an unofficial build; it talks to Geph's official brokers and exits.

## License

Code derived from Geph5: **MPL-2.0** (some low-level libraries: **ISC**).
This modified distribution complies with those licenses; recipients of
binaries are entitled to the corresponding modified MPL-2.0 source files.
See "Provenance and disclaimer" above.
