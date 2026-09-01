use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use russh::server::Handle;
use russh::{Channel, ChannelId, CryptoVec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn};

use crate::config::Config;
use crate::db::{Role, User, consume_registration_token, list_all_repos, open_db};

pub struct CommandContext {
    pub config: Config,
    pub authenticated_user: Option<User>,
    pub pending_public_key: Option<russh::keys::ssh_key::PublicKey>,
    pub channel_id: ChannelId,
    pub handle: Handle,
}

pub async fn handle_command(
    command: String,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) -> Result<()> {
    let parts = shlex::split(&command)
        .unwrap_or_else(|| command.split_whitespace().map(String::from).collect());

    if parts.is_empty() {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "empty command\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    match parts[0].as_str() {
        "git-upload-pack" => {
            if parts.len() < 2 {
                let _ = ctx.handle.channel_failure(ctx.channel_id).await;
                send_stderr(&ctx, "usage: git-upload-pack <repo>\n").await?;
                send_exit(&ctx, 1).await?;
                return Ok(());
            }
            spawn_git("upload-pack", parts[1].clone(), channel, ctx);
            Ok(())
        }
        "git-receive-pack" => {
            if parts.len() < 2 {
                let _ = ctx.handle.channel_failure(ctx.channel_id).await;
                send_stderr(&ctx, "usage: git-receive-pack <repo>\n").await?;
                send_exit(&ctx, 1).await?;
                return Ok(());
            }
            spawn_git("receive-pack", parts[1].clone(), channel, ctx);
            Ok(())
        }
        "register" => handle_register(parts, channel, ctx).await,
        "list" => handle_list(channel, ctx).await,
        _ => {
            let _ = ctx.handle.channel_failure(ctx.channel_id).await;
            send_stderr(&ctx, &format!("unknown command: {}\n", parts[0])).await?;
            send_exit(&ctx, 1).await?;
            Ok(())
        }
    }
}

fn spawn_git(
    subcommand: &'static str,
    _repo_arg: String,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) {
    tokio::spawn(async move {
        let repo_path = match resolve_repo_path(&ctx) {
            Ok(p) => p,
            Err(e) => {
                let _ = ctx.handle.channel_failure(ctx.channel_id).await;
                let _ = send_stderr(&ctx, &format!("{:#}\n", e)).await;
                let _ = send_exit(&ctx, 1).await;
                return;
            }
        };

        if !repo_path.exists() {
            info!("creating bare repository at {:?}", repo_path);
            if let Err(e) = create_bare_repo(&repo_path).await {
                let _ = ctx.handle.channel_failure(ctx.channel_id).await;
                let _ = send_stderr(&ctx, &format!("{:#}\n", e)).await;
                let _ = send_exit(&ctx, 1).await;
                return;
            }
        }

        if let Err(e) = run_git_command(subcommand, repo_path, channel, &ctx).await {
            warn!("git {} failed: {:#}", subcommand, e);
        }
    });
}

async fn create_bare_repo(repo_path: &std::path::Path) -> Result<()> {
    let parent = repo_path.parent().context("repo path has no parent")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating repo parent directory {:?}", parent))?;
    let output = tokio::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(repo_path)
        .output()
        .await
        .with_context(|| format!("running git init --bare {:?}", repo_path))?;
    if !output.status.success() {
        anyhow::bail!(
            "git init --bare failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

async fn run_git_command(
    subcommand: &str,
    repo_path: PathBuf,
    channel: Channel<russh::server::Msg>,
    ctx: &CommandContext,
) -> Result<()> {
    ctx.handle
        .channel_success(ctx.channel_id)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;

    let mut child = tokio::process::Command::new("git")
        .arg(subcommand)
        .arg(&repo_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning git {}", subcommand))?;

    let (mut channel_read, mut channel_write) = tokio::io::split(channel.into_stream());
    let mut child_stdin = child.stdin.take().unwrap();
    let mut child_stdout = child.stdout.take().unwrap();
    let mut child_stderr = child.stderr.take().unwrap();

    let channel_id = ctx.channel_id;
    let handle = ctx.handle.clone();

    // Copy client data to git stdin.
    let stdin_copy =
        tokio::spawn(async move { tokio::io::copy(&mut channel_read, &mut child_stdin).await });

    // Forward git stderr to the SSH channel as extended data.
    let stderr_copy = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match child_stderr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let _ = handle
                        .extended_data(channel_id, 1, CryptoVec::from_slice(&buf[..n]))
                        .await;
                }
                Err(e) => {
                    warn!("stderr read error: {}", e);
                    break;
                }
            }
        }
    });

    // Copy git stdout to the SSH channel in this task so we can shut down the
    // write half cleanly after git finishes producing output.
    let stdout_result = tokio::io::copy(&mut child_stdout, &mut channel_write).await;
    if let Err(e) = stdout_result {
        warn!("stdout copy error: {}", e);
    }
    if let Err(e) = channel_write.shutdown().await {
        warn!("channel shutdown error: {}", e);
    }

    // Wait for the remaining copy tasks and the child process.
    let (stdin_res, stderr_res) = tokio::join!(stdin_copy, stderr_copy);
    if let Err(e) = stdin_res {
        warn!("stdin copy task failed: {}", e);
    }
    if let Err(e) = stderr_res {
        warn!("stderr copy task failed: {}", e);
    }

    let status = child.wait().await.context("waiting for git process")?;
    let exit_code = status.code().unwrap_or(1) as u32;
    info!(
        "git {} exited with code {} for channel {}",
        subcommand, exit_code, ctx.channel_id
    );

    ctx.handle
        .exit_status_request(ctx.channel_id, exit_code)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send exit status"))?;

    Ok(())
}

async fn handle_register(
    parts: Vec<String>,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) -> Result<()> {
    let token = parts.get(1);
    let name = parts.get(2);

    let (Some(token), Some(name)) = (token, name) else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "usage: register <token> <name>\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    if ctx.authenticated_user.is_some() {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "already registered\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let Some(ref key) = ctx.pending_public_key else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "registration requires a recognized SSH public key\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let key_text = key.to_openssh().context("serializing public key")?;

    let mut conn = open_db(&ctx.config.db_path())?;
    match consume_registration_token(&mut conn, token, name, &key_text) {
        Ok(student) => {
            info!("registered student {} (id {})", student.name, student.id);
            ctx.handle
                .channel_success(ctx.channel_id)
                .await
                .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;
            let msg = format!("Welcome, {}! You are now registered.\n", student.name);
            channel
                .data(msg.as_bytes())
                .await
                .map_err(|_| anyhow::anyhow!("failed to send registration response"))?;
            send_exit(&ctx, 0).await?;
        }
        Err(e) => {
            warn!("registration failed: {:#}", e);
            let _ = ctx.handle.channel_failure(ctx.channel_id).await;
            send_stderr(&ctx, &format!("registration failed: {:#}\n", e)).await?;
            send_exit(&ctx, 1).await?;
        }
    }

    Ok(())
}

async fn handle_list(channel: Channel<russh::server::Msg>, ctx: CommandContext) -> Result<()> {
    ctx.handle
        .channel_success(ctx.channel_id)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;

    let conn = open_db(&ctx.config.db_path())?;
    let repos = list_all_repos(&conn)?;

    match ctx.authenticated_user.as_ref().map(|u| u.role) {
        Some(Role::Teacher) => {
            for (id, name, fingerprint) in repos {
                let line = format!("{} {} {}\n", id, name, fingerprint);
                channel
                    .data(line.as_bytes())
                    .await
                    .map_err(|_| anyhow::anyhow!("failed to send list response"))?;
            }
        }
        Some(Role::Student) => {
            let user = ctx.authenticated_user.as_ref().unwrap();
            let line = format!("{} {} repo.git\n", user.id, user.name);
            channel
                .data(line.as_bytes())
                .await
                .map_err(|_| anyhow::anyhow!("failed to send list response"))?;
        }
        None => {
            send_stderr(&ctx, "list requires registration\n").await?;
            send_exit(&ctx, 1).await?;
            return Ok(());
        }
    }

    send_exit(&ctx, 0).await?;
    Ok(())
}

fn resolve_repo_path(ctx: &CommandContext) -> Result<PathBuf> {
    let Some(ref user) = ctx.authenticated_user else {
        anyhow::bail!("not authenticated");
    };

    match user.role {
        Role::Student => Ok(ctx.config.user_repo_path(user.id)),
        Role::Teacher => {
            // For teachers, require the repo path to be specified explicitly.
            // In the MVP, teachers interact through `list` and then clone by student id.
            anyhow::bail!("teachers must clone specific student repositories via `list`");
        }
    }
}

async fn send_stderr(ctx: &CommandContext, msg: &str) -> Result<()> {
    ctx.handle
        .extended_data(ctx.channel_id, 1, CryptoVec::from_slice(msg.as_bytes()))
        .await
        .map_err(|_| anyhow::anyhow!("failed to send stderr"))?;
    Ok(())
}

async fn send_exit(ctx: &CommandContext, code: u32) -> Result<()> {
    let _ = ctx.handle.eof(ctx.channel_id).await;
    ctx.handle
        .exit_status_request(ctx.channel_id, code)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send exit status"))?;
    Ok(())
}
