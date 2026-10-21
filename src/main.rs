//! Orqen — loopback gateway for coding agents.
//!
//! Native HTTP/SSE forwarding to model providers (OpenCode Go today) with a
//! scoped credential pool.
//! The operating contract lives in `docs/operations.md`.

mod config;
mod error;
#[cfg(test)]
mod integration_tests;
mod pool;
mod proxy;
mod retry_after;
mod session;

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use axum::Router;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::pool::Pool;
use crate::proxy::AppState;

const USAGE: &str = "\
Usage: orqen [--config PATH] [--check]
       orqen --version | --help

  -c, --config PATH  TOML configuration (default: $ORQEN_CONFIG, then
                     $XDG_CONFIG_HOME/orqen/orqen.toml, then
                     ~/.config/orqen/orqen.toml)
      --check        validate the configuration and key files, then exit
  -V, --version      print the version
  -h, --help         print this help

Logging: ORQEN_LOG (tracing env-filter syntax, default \"info\").
";

#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("{0}\n\n{USAGE}")]
    Usage(String),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error("build upstream HTTP client: {0}")]
    Client(reqwest::Error),
    #[error("bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("serve: {0}")]
    Serve(std::io::Error),
}

enum Command {
    Run(PathBuf),
    Check(PathBuf),
    Version,
    Help,
}

fn parse_args(args: &[OsString]) -> Result<Command, StartupError> {
    let mut config: Option<PathBuf> = None;
    let mut check = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(text) = arg.to_str() else {
            return Err(StartupError::Usage(format!("unknown argument {arg:?}")));
        };
        match text {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--check" => check = true,
            "-c" | "--config" => {
                let value = iter
                    .next()
                    .ok_or_else(|| StartupError::Usage(format!("{text} requires a path")))?;
                config = Some(PathBuf::from(value));
            }
            other => match other.strip_prefix("--config=") {
                Some(value) => config = Some(PathBuf::from(value)),
                None => return Err(StartupError::Usage(format!("unknown argument {other:?}"))),
            },
        }
    }
    let path = match config {
        Some(path) => path,
        None => default_config_path()?,
    };
    Ok(if check {
        Command::Check(path)
    } else {
        Command::Run(path)
    })
}

fn default_config_path() -> Result<PathBuf, StartupError> {
    if let Some(path) = std::env::var_os("ORQEN_CONFIG").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir).join("orqen").join("orqen.toml"));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config/orqen/orqen.toml"))
        .ok_or_else(|| {
            StartupError::Usage(
                "no --config given and none of ORQEN_CONFIG, XDG_CONFIG_HOME, HOME is set"
                    .to_owned(),
            )
        })
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("ORQEN_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// Resolve on SIGINT or SIGTERM. A handler that cannot be installed never
/// resolves, so a signal-setup failure cannot trigger a spurious shutdown.
async fn wait_for_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "SIGINT handler unavailable");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    () = interrupt => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "SIGTERM handler unavailable");
                interrupt.await;
            }
        }
    }
    #[cfg(not(unix))]
    interrupt.await;
}

/// Assembled service, shared by `run` and the loopback tests.
struct Service {
    listen: SocketAddr,
    drain: Duration,
    state: Arc<AppState>,
    router: Router,
}

/// Build shared state and the router from a loaded config.
fn build_service(config: Config, shutdown: CancellationToken) -> Result<Service, StartupError> {
    let Config {
        listen,
        upstream,
        limits,
        cooldown,
        session,
        keys,
    } = config;
    let client = proxy::build_client(&upstream).map_err(StartupError::Client)?;
    let drain = limits.shutdown_drain;
    let state = Arc::new(AppState {
        client,
        pool: Pool::new(keys, cooldown),
        upstream,
        limits,
        session,
        shutdown,
        started: Instant::now(),
        ephemeral_sessions: AtomicU64::new(0),
    });
    let router = proxy::router(Arc::clone(&state));
    Ok(Service {
        listen,
        drain,
        state,
        router,
    })
}

async fn run(args: &[OsString]) -> Result<(), StartupError> {
    let path = match parse_args(args)? {
        Command::Help => {
            print!("{USAGE}");
            return Ok(());
        }
        Command::Version => {
            println!("orqen {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Command::Check(path) => {
            init_tracing();
            let config = config::load(&path)?;
            println!("configuration OK: {} key(s)", config.keys.len());
            return Ok(());
        }
        Command::Run(path) => path,
    };
    init_tracing();

    let config = config::load(&path)?;
    let shutdown = CancellationToken::new();
    let Service {
        listen,
        drain,
        state,
        router,
    } = build_service(config, shutdown.clone())?;
    tracing::info!(
        %listen,
        upstream = %state.upstream.base_url,
        keys = state.pool.len(),
        scopes = state.pool.scope_count(),
        "starting orqen"
    );

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|source| StartupError::Bind {
            addr: listen,
            source,
        })?;
    tracing::info!(%listen, "orqen listening");

    let server = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .into_future();
    let mut server = std::pin::pin!(server);
    tokio::select! {
        result = &mut server => result.map_err(StartupError::Serve)?,
        () = wait_for_signal() => {
            tracing::info!(drain_secs = drain.as_secs(), "shutdown requested; draining in-flight requests");
            // New attempts observe the token and are refused; in-flight
            // responses may finish until the drain deadline.
            shutdown.cancel();
            match tokio::time::timeout(drain, &mut server).await {
                Ok(result) => result.map_err(StartupError::Serve)?,
                Err(_) => tracing::warn!("drain deadline reached; closing remaining connections"),
            }
        }
    }
    // Returning drops the runtime: every connection task and its upstream
    // request is dropped with it. No retry runs detached from a request.
    tracing::info!("orqen stopped");
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("orqen: {error}");
            ExitCode::FAILURE
        }
    }
}
