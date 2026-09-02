use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gith::config::Config;
use gith::db::{add_user, init_db, open_db};
use gith::ssh_server::GithSshServer;
use russh::server::Server as _;
use tracing::{error, info};

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
    },
}

#[derive(Subcommand)]
enum AdminCommands {
    Init,
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
    let config = Config::new(cli.data_dir.clone())?;

    match cli.command {
        Commands::Admin(admin) => match admin {
            AdminCommands::Init => {
                config.ensure_dirs()?;
                let conn = open_db(&config.db_path())?;
                init_db(&conn)?;
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
        Commands::Server { host, port } => {
            config.ensure_dirs()?;
            let conn = open_db(&config.db_path())?;
            init_db(&conn)?;

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
            &mut rand::thread_rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )
        .context("generating host key")?;
        let pem = key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?;
        std::fs::write(path, pem).with_context(|| format!("writing host key to {:?}", path))?;
        Ok(key)
    }
}
