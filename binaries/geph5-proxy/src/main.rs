use std::{io::Write, net::{IpAddr, Ipv4Addr, SocketAddr}, path::PathBuf, time::Duration};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use geph5_broker_protocol::{
    Credential, DOMAIN_NET_STATUS, ExitConstraint, NetStatus, puzzle::solve_puzzle,
};
use geph5_client::{broker_client, Client, Config};
use geph5_misc_rpc::{
    client_control::ConnInfo,
    client_config::{UpstreamAuth, UpstreamProxy},
};
use isocountry::CountryCode;

const CONFIG_TEMPLATE: &str = include_str!("../default-config.yaml");

#[derive(Parser)]
#[command(
    name = "geph5-proxy",
    about = "Geph5 proxy client: create an account, pick a country, and serve a local SOCKS5/HTTP proxy (no system VPN)"
)]
struct Cli {
    /// Chain all outbound traffic (broker + tunnels) through an upstream
    /// proxy first: socks5://[user:pass@]host:port or http://[user:pass@]host:port
    #[arg(long, global = true)]
    upstream: Option<String>,
    /// Where to store/read the account code
    /// (default: <config-dir>/geph5-proxy/account.txt)
    #[arg(long, global = true)]
    account_file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new Geph account; prints and saves the account code
    Signup,
    /// Check a saved (or given) account code and show account info
    Login {
        /// Account code (defaults to the saved code)
        #[arg(long)]
        secret: Option<String>,
    },
    /// List available exit countries and cities
    Countries {
        /// Only list exits usable at this account level
        #[arg(long, value_enum, default_value = "all")]
        level: LevelFilter,
    },
    /// Connect and serve a local proxy
    Run {
        /// Account code (defaults to the saved code, then prompts)
        #[arg(long)]
        secret: Option<String>,
        /// Two-letter country code (e.g. DE), or "auto"
        #[arg(long, default_value = "auto")]
        country: String,
        /// City name as shown by `countries` (requires --country)
        #[arg(long)]
        city: Option<String>,
        /// Connect to a specific exit hostname instead of country selection
        #[arg(long)]
        exit: Option<String>,
        /// SOCKS5 listen port/address ("PORT", "HOST:PORT", or "none")
        #[arg(long, default_value = "9909")]
        socks: String,
        /// HTTP proxy listen port/address ("PORT", "HOST:PORT", or "none")
        #[arg(long, default_value = "9910")]
        http: String,
        /// Listen on 0.0.0.0 so other LAN devices can use this proxy
        /// (no authentication; anyone on the LAN can use it)
        #[arg(long)]
        share_lan: bool,
        /// SQLite cache path (default: <cache-dir>/geph5-proxy/db)
        #[arg(long)]
        cache: Option<PathBuf>,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
enum LevelFilter {
    /// Every exit regardless of account level
    All,
    /// Only exits usable with a free account
    Free,
    /// Only exits that require a Plus subscription
    Plus,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let upstream = match &cli.upstream {
        Some(s) => Some(parse_upstream(s)?),
        None => None,
    };
    let account_file = cli.account_file;
    match cli.cmd {
        Cmd::Signup => geph5_rt::block_on(cmd_signup(upstream, account_file)),
        Cmd::Login { secret } => {
            geph5_rt::block_on(cmd_login(secret, upstream, account_file))
        }
        Cmd::Countries { level } => geph5_rt::block_on(cmd_countries(upstream, level)),
        Cmd::Run {
            secret,
            country,
            city,
            exit,
            socks,
            http,
            share_lan,
            cache,
        } => cmd_run(RunArgs {
            secret,
            country,
            city,
            exit,
            socks,
            http,
            share_lan,
            cache,
            upstream,
            account_file,
        }),
    }
}

/// Parse an upstream proxy URL: socks5://, socks5h://, socks://, http://, or
/// https:// (treated as HTTP CONNECT), with optional user:pass. Hostnames are
/// resolved here so the engine only ever sees a SocketAddr.
fn parse_upstream(s: &str) -> anyhow::Result<UpstreamProxy> {
    let (scheme, rest) = s
        .split_once("://")
        .context("upstream must look like socks5://host:port or http://user:pass@host:port")?;
    let (creds, hostport) = match rest.rsplit_once('@') {
        Some((creds, hostport)) => (Some(creds), hostport),
        None => (None, rest),
    };
    let auth = creds
        .map(|c| -> anyhow::Result<UpstreamAuth> {
            let (user, pass) = c.split_once(':').unwrap_or((c, ""));
            anyhow::ensure!(!user.is_empty(), "upstream username is empty");
            Ok(UpstreamAuth {
                username: user.to_string(),
                password: pass.to_string(),
            })
        })
        .transpose()?;
    let addr = resolve_hostport(hostport).context("could not resolve upstream address")?;
    match scheme.to_ascii_lowercase().as_str() {
        "socks5" | "socks5h" | "socks" => Ok(UpstreamProxy::Socks5 { addr, auth }),
        "http" | "https" => Ok(UpstreamProxy::Http { addr, auth }),
        other => bail!("unknown upstream scheme {other:?} (use socks5:// or http://)"),
    }
}

/// Resolve "host:port" (or just "port" → loopback) to a SocketAddr.
fn resolve_hostport(s: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let (host, port) = s
        .rsplit_once(':')
        .with_context(|| format!("expected host:port, got {s:?}"))?;
    let port: u16 = port.parse().context("bad port")?;
    let ip: IpAddr = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))?
        .next()
        .with_context(|| format!("could not resolve {host}"))?
        .ip();
    Ok(SocketAddr::new(ip, port))
}

/// Parse a listen spec: "PORT" (loopback, or 0.0.0.0 with share_lan),
/// "HOST:PORT", or "none".
fn parse_listen(spec: &str, share_lan: bool) -> anyhow::Result<Option<SocketAddr>> {
    if spec.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let default_ip: IpAddr = if share_lan {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv4Addr::LOCALHOST.into()
    };
    let addr = if let Ok(port) = spec.parse::<u16>() {
        SocketAddr::new(default_ip, port)
    } else {
        let mut addr: SocketAddr = spec
            .parse()
            .with_context(|| format!("invalid listen address {spec:?} (use PORT, HOST:PORT, or none)"))?;
        if share_lan && addr.ip().is_loopback() {
            addr.set_ip(default_ip);
        }
        addr
    };
    Ok(Some(addr))
}

/// Base config from the embedded official template.
fn base_config(upstream: Option<UpstreamProxy>) -> anyhow::Result<Config> {
    let val: serde_json::Value =
        serde_yaml::from_str(CONFIG_TEMPLATE).context("bad embedded config template")?;
    let mut cfg: Config =
        serde_json::from_value(val).context("bad embedded config template")?;
    // Inert until a subcommand turns on what it needs.
    cfg.dry_run = true;
    cfg.credentials = Credential::Secret(String::new());
    cfg.control_listen = None;
    cfg.socks5_listen = None;
    cfg.http_proxy_listen = None;
    cfg.pac_listen = None;
    cfg.upstream = upstream;
    Ok(cfg)
}

fn secret_path(account_file: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match account_file {
        Some(path) => Ok(path),
        None => {
            let dir =
                dirs::config_dir().context("no config directory")?.join("geph5-proxy");
            Ok(dir.join("account.txt"))
        }
    }
}

fn save_secret(account_file: Option<PathBuf>, secret: &str) -> anyhow::Result<()> {
    let path = secret_path(account_file)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path)?;
        f.write_all(secret.trim().as_bytes())?;
    }
    eprintln!("account code saved to {}", path.display());
    Ok(())
}

fn load_saved_secret(account_file: Option<PathBuf>) -> Option<String> {
    let path = secret_path(account_file).ok()?;
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn resolve_secret(explicit: Option<String>, account_file: Option<PathBuf>) -> anyhow::Result<String> {
    if let Some(s) = explicit {
        return Ok(s.trim().to_string());
    }
    if let Some(s) = load_saved_secret(account_file) {
        return Ok(s);
    }
    eprint!("Enter your account code: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("could not read account code from stdin")?;
    let s = line.trim().to_string();
    if s.is_empty() {
        bail!("no account code given");
    }
    Ok(s)
}

async fn cmd_signup(
    upstream: Option<UpstreamProxy>,
    account_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let cfg = base_config(upstream)?;
    let ctx = anyctx::AnyCtx::new(cfg);
    let broker = broker_client(&ctx)?;
    eprintln!("requesting proof-of-work puzzle from broker...");
    let (puzzle, difficulty) = broker.get_puzzle().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    eprintln!("solving puzzle (difficulty {difficulty})...");
    let solution = {
        let puzzle = puzzle.clone();
        geph5_rt::spawn_blocking(move || {
            // Throttle progress output: the callback fires once per PoW step
            // (2^difficulty times), so printing every time would dominate runtime.
            let last = std::cell::Cell::new(u32::MAX);
            solve_puzzle(&puzzle, difficulty, |progress| {
                let pct = (progress * 100.0) as u32;
                if pct != last.get() {
                    last.set(pct);
                    eprint!("\rsolving... {pct:3}%");
                    let _ = std::io::stderr().flush();
                }
            })
        })
        .await
    };
    eprintln!("\rregistering account...              ");
    let secret = broker
        .register_user_secret(puzzle, solution)
        .await
        .map_err(|e| anyhow::anyhow!("broker unreachable: {e}"))?
        .map_err(|e| anyhow::anyhow!("registration refused: {e}"))?;
    save_secret(account_file, &secret)?;
    println!();
    println!("================ ================");
    println!(" Your Geph account code:");
    println!(" {secret}");
    println!("================ ================");
    println!("Use it with: geph5-proxy run --country <CC>");
    Ok(())
}

async fn cmd_login(
    secret: Option<String>,
    upstream: Option<UpstreamProxy>,
    account_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let secret = resolve_secret(secret, account_file.clone())?;
    let cfg = base_config(upstream)?;
    let ctx = anyctx::AnyCtx::new(cfg);
    let broker = broker_client(&ctx)?;
    let info = broker
        .get_user_info_by_cred(Credential::Secret(secret.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("broker unreachable: {e}"))?
        .map_err(|e| anyhow::anyhow!("login failed: {e}"))?
        .context("incorrect account code")?;
    save_secret(account_file, &secret)?;
    println!("logged in: user_id = {}", info.user_id);
    match info.plus_expires_unix {
        Some(unix) => println!(
            "plus: active until {}",
            chrono::DateTime::from_timestamp(unix as i64, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| unix.to_string())
        ),
        None => println!("level: free"),
    }
    if let Some(bw) = info.bw_consumption {
        println!("bandwidth: {} MB / {} MB (renews {})", bw.mb_used, bw.mb_limit, bw.renew_unix);
    }
    Ok(())
}

async fn fetch_net_status(ctx: &anyctx::AnyCtx<Config>) -> anyhow::Result<NetStatus> {
    let broker = broker_client(ctx)?;
    let resp = broker
        .get_net_status()
        .await
        .map_err(|e| anyhow::anyhow!("broker unreachable: {e}"))?
        .map_err(|e| anyhow::anyhow!("broker refused: {e}"))?;
    let master = ctx.init().broker_keys.as_ref().map(|k| k.master.clone());
    resp.verify(DOMAIN_NET_STATUS, |pk| match &master {
        Some(m) => hex::encode(pk.as_bytes()) == *m,
        None => true,
    })
    .context("could not verify net status signature")
}

async fn cmd_countries(
    upstream: Option<UpstreamProxy>,
    level: LevelFilter,
) -> anyhow::Result<()> {
    let cfg = base_config(upstream)?;
    let ctx = anyctx::AnyCtx::new(cfg);
    let net_status = fetch_net_status(&ctx).await?;

    let wants_free = matches!(level, LevelFilter::All | LevelFilter::Free);
    let wants_plus = matches!(level, LevelFilter::All | LevelFilter::Plus);

    // country -> (matching exits, free?, cities->count), counting only exits
    // whose allowed levels pass the filter.
    let mut countries: std::collections::BTreeMap<
        CountryCode,
        (usize, bool, std::collections::BTreeMap<String, usize>),
    > = Default::default();
    for (_key, (_pk, exit, meta)) in &net_status.exits {
        let is_free = meta
            .allowed_levels
            .contains(&geph5_broker_protocol::AccountLevel::Free);
        if !(is_free && wants_free || !is_free && wants_plus) {
            continue;
        }
        let entry = countries.entry(exit.country).or_default();
        entry.0 += 1;
        if is_free {
            entry.1 = true;
        }
        *entry.2.entry(exit.city.clone()).or_default() += 1;
    }

    if countries.is_empty() {
        match level {
            LevelFilter::All => bail!("broker returned an empty exit list"),
            _ => bail!(
                "broker returned no {} exits",
                format!("{level:?}").to_lowercase()
            ),
        }
    }
    println!("{:<4} {:<22} {:>6} {:<6} cities", "CC", "country", "exits", "level");
    for (cc, (count, free, cities)) in countries {
        let cities = cities
            .iter()
            .map(|(city, n)| format!("{city}({n})"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{:<4} {:<22} {:>6} {:<6} {cities}",
            cc.alpha2(),
            cc.name(),
            count,
            if free { "free" } else { "plus" },
        );
    }
    println!();
    println!("Connect with: geph5-proxy run --country <CC>   (or --country auto)");
    Ok(())
}

struct RunArgs {
    secret: Option<String>,
    country: String,
    city: Option<String>,
    exit: Option<String>,
    socks: String,
    http: String,
    share_lan: bool,
    cache: Option<PathBuf>,
    upstream: Option<UpstreamProxy>,
    account_file: Option<PathBuf>,
}

fn cmd_run(args: RunArgs) -> anyhow::Result<()> {
    let constraint_desc = describe_constraint(&client_constraint_hint(&args));
    let secret = resolve_secret(args.secret, args.account_file.clone())?;

    let socks = parse_listen(&args.socks, args.share_lan)?;
    let http = parse_listen(&args.http, args.share_lan)?;
    if socks.is_none() && http.is_none() {
        bail!("nothing to serve: both --socks and --http are none");
    }

    // Validate the account code up front for a friendly error.
    geph5_rt::block_on(async {
        let cfg = base_config(args.upstream.clone())?;
        let ctx = anyctx::AnyCtx::new(cfg);
        let broker = broker_client(&ctx)?;
        broker
            .get_user_info_by_cred(Credential::Secret(secret.clone()))
            .await
            .map_err(|e| anyhow::anyhow!("broker unreachable: {e}"))?
            .map_err(|e| anyhow::anyhow!("incorrect account code: {e}"))?
            .context("incorrect account code")?;
        anyhow::Ok(())
    })?;

    let exit_constraint = if let Some(host) = &args.exit {
        ExitConstraint::Direct(host.clone())
    } else if args.country.eq_ignore_ascii_case("auto") {
        if args.city.is_some() {
            bail!("--city requires --country");
        }
        ExitConstraint::Auto
    } else {
        let cc = CountryCode::for_alpha2_caseless(&args.country)
            .map_err(|_| anyhow::anyhow!("unknown country code: {}", args.country))?;
        match &args.city {
            Some(city) => ExitConstraint::CountryCity(cc, city.clone()),
            None => ExitConstraint::Country(cc),
        }
    };

    let mut cfg = base_config(args.upstream.clone())?;
    cfg.credentials = Credential::Secret(secret);
    cfg.exit_constraint = exit_constraint;
    cfg.dry_run = false;
    cfg.socks5_listen = socks;
    cfg.http_proxy_listen = http;
    cfg.pac_listen = None;
    cfg.control_listen = None;
    cfg.cache = Some(match args.cache {
        Some(c) => c,
        None => dirs::cache_dir()
            .context("no cache directory")?
            .join("geph5-proxy")
            .join("db"),
    });
    if let Some(parent) = cfg.cache.as_ref().and_then(|c| c.parent()) {
        std::fs::create_dir_all(parent)?;
    }

    let http_desc = cfg.http_proxy_listen;
    let client = Client::start(cfg);
    if let Some(socks) = socks {
        eprintln!("SOCKS5 proxy : socks5h://{socks}");
    }
    if let Some(http) = http_desc {
        eprintln!("HTTP proxy   : http://{http}");
    }
    eprintln!("exit         : {constraint_desc}");
    if let Some(up) = &args.upstream {
        eprintln!("upstream     : {up:?}");
    }

    let _ = ctrlc::set_handler(move || {
        eprintln!("\ninterrupted, exiting");
        std::process::exit(0);
    });

    geph5_rt::block_on(async move {
        let ctrl = client.control_client();
        let mut announced = false;
        loop {
            if let Err(err) = client.check_dead() {
                return Err(err);
            }
            if let Ok(ConnInfo::Connected { sessions }) = ctrl.conn_info().await {
                if !announced {
                    for s in &sessions {
                        eprintln!(
                            "connected    : {} via {} ({} / {})",
                            s.exit.country.alpha2(),
                            s.exit.city,
                            s.exit.country.name(),
                            s.protocol,
                        );
                    }
                    announced = true;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

fn client_constraint_hint(args: &RunArgs) -> ExitConstraint {
    if let Some(host) = &args.exit {
        return ExitConstraint::Direct(host.clone());
    }
    if args.country.eq_ignore_ascii_case("auto") {
        return ExitConstraint::Auto;
    }
    match CountryCode::for_alpha2_caseless(&args.country) {
        Ok(cc) => match &args.city {
            Some(city) => ExitConstraint::CountryCity(cc, city.clone()),
            None => ExitConstraint::Country(cc),
        },
        Err(_) => ExitConstraint::Auto,
    }
}

fn describe_constraint(c: &ExitConstraint) -> String {
    match c {
        ExitConstraint::Auto => "auto (any country)".into(),
        ExitConstraint::Direct(s) => format!("direct {s}"),
        ExitConstraint::Hostname(h) => format!("hostname {h}"),
        ExitConstraint::Country(cc) => format!("{} ({})", cc.alpha2(), cc.name()),
        ExitConstraint::CountryCity(cc, city) => {
            format!("{} / {city} ({})", cc.alpha2(), cc.name())
        }
    }
}
