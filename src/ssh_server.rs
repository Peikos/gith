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
    public_key: Option<russh::keys::ssh_key::PublicKey>,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl GithSession {
    fn new(config: Config) -> Self {
        Self {
            config,
            authenticated_user: None,
            public_key: None,
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
                self.public_key = Some(public_key.clone());
                Ok(Auth::Accept)
            }
            Ok(None) => {
                info!(
                    "public key auth accepted for unregistered user {} ({})",
                    user, fingerprint
                );
                self.public_key = Some(public_key.clone());
                Ok(Auth::Accept)
            }
            Err(e) => {
                warn!("database error during auth: {:#}", e);
                Ok(Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                })
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::ChannelOpenHandleInner<Msg>,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        let id = channel.id();
        self.channels.lock().await.insert(id, channel);
        Ok(())
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

        let pending_key = self.public_key.clone();
        let ctx = CommandContext {
            config: self.config.clone(),
            authenticated_user: self.authenticated_user.clone(),
            public_key: pending_key,
            channel_id,
            handle: session.handle(),
        };

        // Run the command off russh's session task. The session task must stay
        // responsive to drain its internal event queue (capacity
        // `event_buffer_size`, 10 by default), process window updates, and
        // flush outgoing data. A handler that queues more than a few messages
        // while running on the session task fills that queue and deadlocks the
        // connection, since nothing can drain it until the handler returns.
        tokio::spawn(async move {
            if let Err(e) = handle_command(command, channel, ctx.clone()).await {
                warn!("command handling failed: {:#}", e);
                let _ = ctx.handle.channel_failure(ctx.channel_id).await;
                let _ = ctx.handle.exit_status_request(ctx.channel_id, 1).await;
                let _ = ctx.handle.eof(ctx.channel_id).await;
                let _ = ctx.handle.close(ctx.channel_id).await;
            }
        });

        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel_id: ChannelId,
        _term: &str,
        _width_px: u32,
        _height_px: u32,
        _width_rows: u32,
        _height_rows: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        info!("rejecting pty request on channel {}", channel_id);
        let _ = session.channel_failure(channel_id);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        info!(
            "handling shell request on channel {} as usage query",
            channel_id
        );

        let channel = {
            let mut channels = self.channels.lock().await;
            channels.remove(&channel_id)
        };

        let Some(channel) = channel else {
            let _ = session.channel_failure(channel_id);
            return Ok(());
        };

        let handle = session.handle();

        let usage = match self.authenticated_user {
            Some(ref user) => format!(
                "gith SSH server\n\nHello, {}. Available commands:\n\
                 - register <token> <name>   (only before registration)\n\
                 - list                     (list repositories)\n\
                 - git-upload-pack <repo>   (git fetch/clone)\n\
                 - git-receive-pack <repo>  (git push)\n",
                user.name
            ),
            None => "gith SSH server\n\nYou are not registered yet.\n\
                     Available command:\n\
                     - register <token> <name>\n"
                .to_string(),
        };

        let _ = handle.channel_success(channel_id).await;
        let _ = channel.data(usage.as_bytes()).await;
        let _ = handle.eof(channel_id).await;
        let _ = handle.exit_status_request(channel_id, 0).await;
        let _ = handle.close(channel_id).await;

        Ok(())
    }
}
