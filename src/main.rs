use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gith::config::{Config, normalize_base_url};
use gith::db::{add_user, get_setting, init_db, open_db, set_setting};
use gith::ssh_server::GithSshServer;
use russh::server::Server as _;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "gith")]
#[command(about = "A self-hosted, SSH-only Git server for classrooms")]
struct Cli {
    #[arg(long, global = true, help = "Path to the data directory")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(subcommand)]
    Admin(AdminCommands),

    Server {
        #[arg(short = 'H', long, default_value = "127.0.0.1")]
        host: String,
        #[arg(short, long, default_value_t = 2222)]
        port: u16,
        /// Canonical SSH base URL (`host`, `host:port`, or `ssh://user@host:port`)
        /// for repository URLs in bulk downloads. Overrides $GITH_BASE_URL and the
        /// value stored by `admin init`; when unset, the server derives one from
        /// the bind address, falling back to a public-IP probe at startup.
        #[arg(long)]
        base_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum AdminCommands {
    Init {
        /// Persist the canonical SSH base URL (same forms as `server --base-url`).
        /// May be overridden per invocation via the flag or $GITH_BASE_URL.
        #[arg(long)]
        base_url: Option<String>,
    },
    AddTeacher {
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        key: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let mut config = Config::new(cli.data_dir.clone())?;

    match cli.command {
        Commands::Admin(admin) => match admin {
            AdminCommands::Init { base_url } => {
                config.ensure_dirs()?;
                let conn = open_db(&config.db_path())?;
                init_db(&conn)?;
                if let Some(url) = base_url {
                    set_setting(&conn, "base_url", &normalize_base_url(&url))?;
                    info!("stored base URL {}", normalize_base_url(&url));
                }
                info!("initialized data directory at {:?}", config.data_dir);
                info!("database: {:?}", config.db_path());
                info!("repos: {:?}", config.repos_dir());
            }
            AdminCommands::AddTeacher { name, key } => {
                config.ensure_dirs()?;
                let conn = open_db(&config.db_path())?;
                init_db(&conn)?;
                let id = add_user(&conn, &name, &key)?;
                info!("added user '{}' with id {}", name, id);
            }
        },
        Commands::Server {
            host,
            port,
            base_url,
        } => {
            config.ensure_dirs()?;
            let conn = open_db(&config.db_path())?;
            init_db(&conn)?;

            // Canonical base URL for repository links in bulk downloads:
            // CLI flag > $GITH_BASE_URL > value persisted by `admin init` >
            // derivation from the bind address > public-IP probe. When even
            // that fails, exports carry no origin remote rather than a
            // misleading one.
            let configured = base_url
                .map(|u| normalize_base_url(&u))
                .or_else(|| {
                    std::env::var("GITH_BASE_URL")
                        .ok()
                        .map(|u| normalize_base_url(&u))
                })
                .or_else(|| get_setting(&conn, "base_url").ok().flatten());
            let resolved = match configured {
                Some(url) => Some(url),
                None => detect_base_url(&host, port),
            };
            match &resolved {
                Some(url) => info!("canonical base URL: {}", url),
                None => warn!(
                    "no base URL configured or detectable; bulk downloads will carry no origin remote"
                ),
            }
            config.set_base_url(resolved);

            let server_key_path = config.data_dir.join("host_key");
            let server_key = load_or_generate_host_key(&server_key_path)
                .context("loading or generating SSH host key")?;

            let russh_config = Arc::new(russh::server::Config {
                inactivity_timeout: Some(Duration::from_secs(600)),
                auth_rejection_time: Duration::from_secs(1),
                auth_rejection_time_initial: Some(Duration::from_secs(0)),
                keys: vec![server_key],
                ..Default::default()
            });

            info!("starting SSH server on {}:{}", host, port);

            let mut server = GithSshServer::new(config);
            if let Err(e) = server.run_on_address(russh_config, (&*host, port)).await {
                error!("server error: {:#}", e);
                return Err(e.into());
            }
        }
    }

    Ok(())
}

fn load_or_generate_host_key(path: &std::path::Path) -> Result<russh::keys::PrivateKey> {
    if path.exists() {
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("reading host key from {:?}", path))?;
        pem.trim()
            .parse::<russh::keys::PrivateKey>()
            .with_context(|| format!("parsing host key from {:?}", path))
    } else {
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )
        .context("generating host key")?;
        let pem = key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?;
        std::fs::write(path, pem).with_context(|| format!("writing host key to {:?}", path))?;
        Ok(key)
    }
}

/// Derive a base URL when none is configured: a concrete bind address is
/// used as-is; an unspecified one (0.0.0.0 / ::) triggers a best-effort
/// public-IP probe. The SSH port is included unless it is the default.
fn detect_base_url(host: &str, port: u16) -> Option<String> {
    let host = if host.is_empty()
        || host
            .parse::<std::net::IpAddr>()
            .map(|a| a.is_unspecified())
            .unwrap_or(false)
    {
        detect_public_ip()?
    } else {
        host.to_string()
    };
    Some(if port == 22 {
        format!("ssh://git@{host}")
    } else {
        format!("ssh://git@{host}:{port}")
    })
}

/// Best-effort public-IP lookup via api.ipify.org (plain HTTP). Returns
/// `None` on any failure; this only runs when the server binds to an
/// unspecified address and no base URL is configured.
fn detect_public_ip() -> Option<String> {
    use std::io::{Read, Write};
    use std::net::ToSocketAddrs;

    let addr = "api.ipify.org:80".to_socket_addrs().ok()?.next()?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: api.ipify.org\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let body = response.split("\r\n\r\n").nth(1)?.trim();
    if body.parse::<std::net::IpAddr>().is_ok() {
        Some(body.to_string())
    } else {
        None
    }
}
