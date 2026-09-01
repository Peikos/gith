use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use russh::server::{Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::Config;
use crate::db::{User, compute_fingerprint, find_user_by_fingerprint, open_db};
use crate::git_protocol::{CommandContext, handle_command};

#[derive(Clone)]
pub struct GithSshServer {
    config: Config,
}

impl GithSshServer {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl Server for GithSshServer {
    type Handler = GithSession;

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
        info!("new SSH connection from {:?}", peer_addr);
        GithSession::new(self.config.clone())
    }
}

pub struct GithSession {
    config: Config,
    authenticated_user: Option<User>,
    pending_public_key: Option<russh::keys::ssh_key::PublicKey>,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl GithSession {
    fn new(config: Config) -> Self {
        Self {
            config,
            authenticated_user: None,
            pending_public_key: None,
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Handler for GithSession {
    type Error = anyhow::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let fingerprint = compute_fingerprint(public_key);
        let conn = open_db(&self.config.db_path())?;
        match find_user_by_fingerprint(&conn, &fingerprint) {
            Ok(Some(db_user)) => {
                info!(
                    "public key auth accepted for registered user {} ({})",
                    db_user.name, fingerprint
                );
                self.authenticated_user = Some(db_user);
                Ok(Auth::Accept)
            }
            Ok(None) => {
                info!(
                    "public key auth accepted for unregistered user {} ({})",
                    user, fingerprint
                );
                self.pending_public_key = Some(public_key.clone());
                Ok(Auth::Accept)
            }
            Err(e) => {
                warn!("database error during auth: {:#}", e);
                Ok(Auth::Reject {
                    proceed_with_methods: None,
                })
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let id = channel.id();
        self.channels.lock().await.insert(id, channel);
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).to_string();
        info!("exec request on channel {}: {}", channel_id, command);

        let channel = {
            let mut channels = self.channels.lock().await;
            channels.remove(&channel_id)
        };

        let Some(channel) = channel else {
            let _ = session.channel_failure(channel_id);
            return Ok(());
        };

        let pending_key = self.pending_public_key.clone();
        let ctx = CommandContext {
            config: self.config.clone(),
            authenticated_user: self.authenticated_user.clone(),
            pending_public_key: pending_key,
            channel_id,
            handle: session.handle(),
        };

        handle_command(command, channel, ctx).await
    }
}
