use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use russh::server::Handle;
use russh::{Channel, ChannelId, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn};

use crate::config::Config;
use crate::db::{
    Role, User, consume_classroom_invite_token, create_classroom, create_classroom_invite_token,
    create_classroom_template, find_classroom_by_slug, find_classroom_membership,
    find_classroom_template, find_user_by_sanitized_name, list_classroom_memberships,
    list_classroom_templates, list_user_memberships, open_db, set_membership_active,
};

#[derive(Clone)]
pub struct CommandContext {
    pub config: Config,
    pub authenticated_user: Option<User>,
    pub public_key: Option<russh::keys::ssh_key::PublicKey>,
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
            spawn_git("upload-pack", parts[1].clone(), channel, ctx).await;
            Ok(())
        }
        "git-receive-pack" => {
            if parts.len() < 2 {
                let _ = ctx.handle.channel_failure(ctx.channel_id).await;
                send_stderr(&ctx, "usage: git-receive-pack <repo>\n").await?;
                send_exit(&ctx, 1).await?;
                return Ok(());
            }
            spawn_git("receive-pack", parts[1].clone(), channel, ctx).await;
            Ok(())
        }
        "register" => handle_register(parts, channel, ctx).await,
        "list" => handle_list(channel, ctx).await,
        "classroom" => handle_classroom(parts, channel, ctx).await,
        _ => {
            let _ = ctx.handle.channel_failure(ctx.channel_id).await;
            send_stderr(&ctx, &format!("unknown command: {}\n", parts[0])).await?;
            send_exit(&ctx, 1).await?;
            Ok(())
        }
    }
}

async fn spawn_git(
    subcommand: &'static str,
    repo_arg: String,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) {
    tokio::spawn(async move {
        let repo_result = resolve_repo_path(&ctx, &repo_arg, subcommand == "receive-pack").await;
        let (repo_path, needs_upstream_sync) = match repo_result {
            Ok(p) => p,
            Err(e) => {
                let _ = ctx.handle.channel_failure(ctx.channel_id).await;
                let _ = send_stderr(&ctx, &format!("{:#}\n", e)).await;
                let _ = send_exit(&ctx, 1).await;
                return;
            }
        };

        if !repo_path.exists() {
            info!("creating repository at {:?}", repo_path);
            if let Err(e) = create_bare_repo(&repo_path).await {
                let _ = ctx.handle.channel_failure(ctx.channel_id).await;
                let _ = send_stderr(&ctx, &format!("{:#}\n", e)).await;
                let _ = send_exit(&ctx, 1).await;
                return;
            }
        }

        if needs_upstream_sync {
            if let Err(e) = sync_upstream_branch(&ctx, &repo_path).await {
                warn!("upstream sync failed: {:#}", e);
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

    let stdin_copy =
        tokio::spawn(async move { tokio::io::copy(&mut channel_read, &mut child_stdin).await });

    let stderr_copy = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match child_stderr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let _ = handle.extended_data(channel_id, 1, buf[..n].to_vec()).await;
                }
                Err(e) => {
                    warn!("stderr read error: {}", e);
                    break;
                }
            }
        }
    });

    let stdout_result = tokio::io::copy(&mut child_stdout, &mut channel_write).await;
    if let Err(e) = stdout_result {
        warn!("stdout copy error: {}", e);
    }
    if let Err(e) = channel_write.shutdown().await {
        warn!("channel shutdown error: {}", e);
    }

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
    let name = parts
        .get(2..)
        .map(|s| s.join(" "))
        .filter(|s| !s.is_empty());

    let (Some(token), Some(ref name)) = (token, name) else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "usage: register <token> <name>\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let Some(ref key) = ctx.public_key else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "registration requires a recognized SSH public key\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let key_text = key.to_openssh().context("serializing public key")?;

    let mut conn = open_db(&ctx.config.db_path())?;
    match consume_classroom_invite_token(&mut conn, token, Some(name), Some(&key_text)) {
        Ok((user, membership)) => {
            info!(
                "registered user {} (id {}) into classroom {} as {:?}",
                user.name, user.id, membership.classroom_id, membership.role
            );
            ctx.handle
                .channel_success(ctx.channel_id)
                .await
                .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;
            let msg = format!(
                "Welcome, {}! You are now registered in the classroom.\n",
                user.name
            );
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

    let Some(ref user) = ctx.authenticated_user else {
        send_stderr(&ctx, "list requires registration\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let conn = open_db(&ctx.config.db_path())?;
    let memberships =
        list_user_memberships(&conn, user.id, false).context("loading user classrooms")?;

    if memberships.is_empty() {
        channel
            .data(&b"You are not a member of any classroom yet.\n"[..])
            .await
            .map_err(|_| anyhow::anyhow!("failed to send list response"))?;
    }

    for (classroom, membership) in memberships {
        let role_line = format!(
            "{} ({}) - role: {}, active: {}\n",
            classroom.name,
            classroom.slug,
            membership.role.as_str(),
            if membership.active { "yes" } else { "no" }
        );
        channel
            .data(role_line.as_bytes())
            .await
            .map_err(|_| anyhow::anyhow!("failed to send list response"))?;

        if membership.active {
            let templates = list_classroom_templates(&conn, classroom.id)
                .context("loading classroom templates")?;
            for template in templates {
                let repo_url = format!("  /{}/{}.git\n", classroom.slug, template.repo_name);
                channel
                    .data(repo_url.as_bytes())
                    .await
                    .map_err(|_| anyhow::anyhow!("failed to send list response"))?;
            }
        }
    }

    send_exit(&ctx, 0).await?;
    Ok(())
}

async fn handle_classroom(
    parts: Vec<String>,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) -> Result<()> {
    if parts.len() < 2 {
        send_classroom_usage(&ctx).await?;
        return Ok(());
    }

    match parts[1].as_str() {
        "create" => classroom_create(parts, channel, ctx).await,
        "invite" => classroom_invite(parts, channel, ctx).await,
        "add-template" => classroom_add_template(parts, channel, ctx).await,
        "list" => classroom_list(parts, channel, ctx).await,
        "deactivate" => classroom_deactivate(parts, channel, ctx).await,
        "download" => classroom_download(parts, channel, ctx).await,
        _ => {
            send_classroom_usage(&ctx).await?;
            Ok(())
        }
    }
}

async fn send_classroom_usage(ctx: &CommandContext) -> Result<()> {
    let _ = ctx.handle.channel_failure(ctx.channel_id).await;
    let usage = "classroom usage:\n\
                 - classroom create <slug> <name>\n\
                 - classroom invite <slug> <role>\n\
                 - classroom add-template <slug> <repo-name>\n\
                 - classroom list <slug>\n\
                 - classroom deactivate <slug> <user-id>\n\
                 - classroom download <slug> <repo-name>\n";
    send_stderr(ctx, usage).await?;
    send_exit(ctx, 1).await?;
    Ok(())
}

async fn classroom_create(
    parts: Vec<String>,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) -> Result<()> {
    if parts.len() < 4 {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "usage: classroom create <slug> <name>\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let slug = &parts[2];
    let name = parts[3..].join(" ");

    if slug.contains(' ') {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "classroom slug cannot contain spaces\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let Some(ref user) = ctx.authenticated_user else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "classroom create requires registration\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let mut conn = open_db(&ctx.config.db_path())?;
    match create_classroom(&mut conn, slug, &name, user.id) {
        Ok(classroom) => {
            ctx.handle
                .channel_success(ctx.channel_id)
                .await
                .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;
            let msg = format!(
                "Created classroom {} ({}).\n",
                classroom.name, classroom.slug
            );
            channel
                .data(msg.as_bytes())
                .await
                .map_err(|_| anyhow::anyhow!("failed to send create response"))?;
            send_exit(&ctx, 0).await?;
        }
        Err(e) => {
            let _ = ctx.handle.channel_failure(ctx.channel_id).await;
            send_stderr(&ctx, &format!("failed to create classroom: {:#}\n", e)).await?;
            send_exit(&ctx, 1).await?;
        }
    }
    Ok(())
}

async fn classroom_invite(
    parts: Vec<String>,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) -> Result<()> {
    if parts.len() < 4 {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "usage: classroom invite <slug> <role>\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let slug = &parts[2];
    let role_str = &parts[3];
    let role = match role_str.parse::<Role>() {
        Ok(r) => r,
        Err(_) => {
            let _ = ctx.handle.channel_failure(ctx.channel_id).await;
            send_stderr(&ctx, "role must be one of: teacher, ta, student\n").await?;
            send_exit(&ctx, 1).await?;
            return Ok(());
        }
    };

    let Some(ref user) = ctx.authenticated_user else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "classroom invite requires registration\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let conn = open_db(&ctx.config.db_path())?;
    let classroom = find_classroom_by_slug(&conn, slug)
        .context("loading classroom")?
        .context("classroom not found")?;

    let membership =
        find_classroom_membership(&conn, user.id, classroom.id).context("loading membership")?;

    let can_invite = matches!(membership, Some(m) if m.role == Role::Teacher && m.active);

    if !can_invite {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(
            &ctx,
            "only active classroom teachers can create invite tokens\n",
        )
        .await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let token = create_classroom_invite_token(&conn, classroom.id, role)
        .context("creating invite token")?;

    ctx.handle
        .channel_success(ctx.channel_id)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;
    channel
        .data(format!("{}\n", token).as_bytes())
        .await
        .map_err(|_| anyhow::anyhow!("failed to send token"))?;
    send_exit(&ctx, 0).await?;
    Ok(())
}

async fn classroom_add_template(
    parts: Vec<String>,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) -> Result<()> {
    if parts.len() < 4 {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "usage: classroom add-template <slug> <repo-name>\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let slug = &parts[2];
    let repo_name = &parts[3];

    let Some(ref user) = ctx.authenticated_user else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "classroom add-template requires registration\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let conn = open_db(&ctx.config.db_path())?;
    let classroom = find_classroom_by_slug(&conn, slug)
        .context("loading classroom")?
        .context("classroom not found")?;

    let membership =
        find_classroom_membership(&conn, user.id, classroom.id).context("loading membership")?;

    let can_manage = matches!(membership, Some(m) if m.role == Role::Teacher && m.active);

    if !can_manage {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "only active classroom teachers can add templates\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    create_classroom_template(&conn, classroom.id, repo_name).context("recording template")?;

    let template_path = ctx.config.classroom_template_path(slug, repo_name);
    if !template_path.exists() {
        if let Err(e) = create_bare_repo(&template_path).await {
            let _ = ctx.handle.channel_failure(ctx.channel_id).await;
            send_stderr(&ctx, &format!("failed to create template repo: {:#}\n", e)).await?;
            send_exit(&ctx, 1).await?;
            return Ok(());
        }
    }

    ctx.handle
        .channel_success(ctx.channel_id)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;
    channel
        .data(format!("Added template {} to classroom {}.\n", repo_name, slug).as_bytes())
        .await
        .map_err(|_| anyhow::anyhow!("failed to send response"))?;
    send_exit(&ctx, 0).await?;
    Ok(())
}

async fn classroom_list(
    parts: Vec<String>,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) -> Result<()> {
    if parts.len() < 3 {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "usage: classroom list <slug>\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let slug = &parts[2];

    let Some(ref user) = ctx.authenticated_user else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "classroom list requires registration\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let conn = open_db(&ctx.config.db_path())?;
    let classroom = find_classroom_by_slug(&conn, slug)
        .context("loading classroom")?
        .context("classroom not found")?;

    let membership =
        find_classroom_membership(&conn, user.id, classroom.id).context("loading membership")?;

    if membership.is_none() {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "you are not a member of this classroom\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    ctx.handle
        .channel_success(ctx.channel_id)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;

    channel
        .data(format!("Classroom: {} ({}):\n", classroom.name, classroom.slug).as_bytes())
        .await
        .map_err(|_| anyhow::anyhow!("failed to send list response"))?;

    let templates = list_classroom_templates(&conn, classroom.id).context("loading templates")?;
    channel
        .data(&b"Templates:\n"[..])
        .await
        .map_err(|_| anyhow::anyhow!("failed to send list response"))?;
    for template in templates {
        channel
            .data(format!("  /{}/{}.git\n", classroom.slug, template.repo_name).as_bytes())
            .await
            .map_err(|_| anyhow::anyhow!("failed to send list response"))?;
    }

    let members =
        list_classroom_memberships(&conn, classroom.id, false).context("loading members")?;
    channel
        .data(&b"Members:\n"[..])
        .await
        .map_err(|_| anyhow::anyhow!("failed to send list response"))?;
    for (member, m) in members {
        channel
            .data(
                format!(
                    "  {} (id {}) - role: {}, active: {}\n",
                    member.name,
                    member.id,
                    m.role.as_str(),
                    if m.active { "yes" } else { "no" }
                )
                .as_bytes(),
            )
            .await
            .map_err(|_| anyhow::anyhow!("failed to send list response"))?;
    }

    send_exit(&ctx, 0).await?;
    Ok(())
}

async fn classroom_deactivate(
    parts: Vec<String>,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) -> Result<()> {
    if parts.len() < 4 {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "usage: classroom deactivate <slug> <user-id>\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let slug = &parts[2];
    let user_id: i64 = match parts[3].parse() {
        Ok(id) => id,
        Err(_) => {
            let _ = ctx.handle.channel_failure(ctx.channel_id).await;
            send_stderr(&ctx, "user-id must be an integer\n").await?;
            send_exit(&ctx, 1).await?;
            return Ok(());
        }
    };

    let Some(ref user) = ctx.authenticated_user else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "classroom deactivate requires registration\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let conn = open_db(&ctx.config.db_path())?;
    let classroom = find_classroom_by_slug(&conn, slug)
        .context("loading classroom")?
        .context("classroom not found")?;

    let membership =
        find_classroom_membership(&conn, user.id, classroom.id).context("loading membership")?;

    let can_manage = matches!(membership, Some(m) if m.role == Role::Teacher && m.active);

    if !can_manage {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(
            &ctx,
            "only active classroom teachers can deactivate members\n",
        )
        .await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    set_membership_active(&conn, classroom.id, user_id, false)
        .context("deactivating membership")?;

    ctx.handle
        .channel_success(ctx.channel_id)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;
    channel
        .data(format!("Deactivated user {} in classroom {}.\n", user_id, slug).as_bytes())
        .await
        .map_err(|_| anyhow::anyhow!("failed to send response"))?;
    send_exit(&ctx, 0).await?;
    Ok(())
}

async fn classroom_download(
    parts: Vec<String>,
    channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
) -> Result<()> {
    if parts.len() < 4 {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "usage: classroom download <slug> <repo-name>\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let slug = &parts[2];
    let repo_name = &parts[3];

    let Some(ref user) = ctx.authenticated_user else {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "classroom download requires registration\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    };

    let conn = open_db(&ctx.config.db_path())?;
    let classroom = find_classroom_by_slug(&conn, slug)
        .context("loading classroom")?
        .context("classroom not found")?;

    let membership =
        find_classroom_membership(&conn, user.id, classroom.id).context("loading membership")?;

    let can_download = match membership {
        Some(m) if m.active => m.role == Role::Teacher || m.role == Role::Ta,
        _ => false,
    };

    if !can_download {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "only active teachers and TAs can download archives\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    let _template = find_classroom_template(&conn, classroom.id, repo_name)
        .context("loading template")?
        .context("template not found")?;

    let active_students = list_classroom_memberships(&conn, classroom.id, true)
        .context("loading active students")?
        .into_iter()
        .filter(|(_, m)| m.role == Role::Student)
        .map(|(u, _)| u)
        .collect::<Vec<_>>();

    let students_dir = ctx
        .config
        .repos_dir()
        .join(slug)
        .join(repo_name)
        .join("students");

    // Only include students whose repositories actually exist.
    let mut archive_entries: Vec<(std::path::PathBuf, String)> = Vec::new();
    for student in &active_students {
        let repo_path = students_dir.join(format!("{}.git", student.id));
        if repo_path.is_dir() {
            archive_entries.push((repo_path, student.name.replace(' ', "")));
        } else {
            info!(
                "skipping student {} (id {}): repo {:?} does not exist",
                student.name, student.id, repo_path
            );
        }
    }

    if archive_entries.is_empty() {
        let _ = ctx.handle.channel_failure(ctx.channel_id).await;
        send_stderr(&ctx, "no active student repositories found to archive\n").await?;
        send_exit(&ctx, 1).await?;
        return Ok(());
    }

    // The archive build and streaming happen in `classroom_download_send`.
    // Command handling already runs off russh's session task (see
    // `exec_request`), so russh stays responsive to window updates and can
    // flush outgoing data while the archive streams.
    let slug = slug.clone();
    let repo_name = repo_name.clone();
    if let Err(e) = classroom_download_send(slug, repo_name, channel, ctx, archive_entries).await {
        warn!("classroom download send failed: {:#}", e);
    }

    Ok(())
}

async fn classroom_download_send(
    slug: String,
    repo_name: String,
    mut channel: Channel<russh::server::Msg>,
    ctx: CommandContext,
    archive_entries: Vec<(std::path::PathBuf, String)>,
) -> Result<()> {
    ctx.handle
        .channel_success(ctx.channel_id)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send channel success"))?;
    info!(
        "download for {}/{}: channel_success sent, building archive for {} repo(s)",
        slug,
        repo_name,
        archive_entries.len()
    );

    let archive = match tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        {
            let encoder = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            for (repo_path, sanitized_name) in archive_entries {
                builder
                    .append_dir_all(format!("{}.git", sanitized_name), &repo_path)
                    .with_context(|| format!("adding {:?} to archive", repo_path))?;
            }
            builder.finish().context("finalizing tar archive")?;
        }
        Ok(buf)
    })
    .await
    {
        Ok(Ok(archive)) => archive,
        Ok(Err(e)) => {
            send_stderr(&ctx, &format!("failed to build archive: {:#}\n", e)).await?;
            send_exit(&ctx, 1).await?;
            return Ok(());
        }
        Err(e) => {
            send_stderr(&ctx, &format!("archive build task panicked: {}\n", e)).await?;
            send_exit(&ctx, 1).await?;
            return Ok(());
        }
    };

    info!(
        "download for {}/{}: sending archive of {} bytes",
        slug,
        repo_name,
        archive.len()
    );

    // Stream the archive through a raw Channel writer. A separate drain task
    // consumes *all* messages from the channel receiver (including window
    // adjustments and EOF) so russh's session task never blocks trying to push
    // control messages into a full channel while we are sending data.
    let mut channel_write = channel.make_writer();
    tokio::spawn(async move {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Eof) | None => break,
                Some(_) => {}
            }
        }
    });

    let mut reader = std::io::Cursor::new(archive);
    let mut buf = [0u8; 8192];
    let mut total = 0usize;
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                warn!("archive read error: {}", e);
                send_stderr(&ctx, "failed to read archive data\n").await?;
                send_exit(&ctx, 1).await?;
                return Ok(());
            }
        };

        if let Err(e) = channel_write.write_all(&buf[..n]).await {
            warn!("archive write error after {} bytes: {}", total, e);
            send_stderr(&ctx, "failed to stream archive data\n").await?;
            send_exit(&ctx, 1).await?;
            return Ok(());
        }
        if let Err(e) = channel_write.flush().await {
            warn!("archive flush error after {} bytes: {}", total, e);
            send_stderr(&ctx, "failed to flush archive data\n").await?;
            send_exit(&ctx, 1).await?;
            return Ok(());
        }

        total += n;
        info!(
            "download for {}/{}: sent chunk of {} bytes (total {})",
            slug, repo_name, n, total
        );
    }

    if let Err(e) = channel_write.shutdown().await {
        warn!("channel shutdown error: {}", e);
    }

    info!("download for {}/{}: archive stream closed", slug, repo_name);
    send_exit(&ctx, 0).await?;
    Ok(())
}

async fn resolve_repo_path(
    ctx: &CommandContext,
    repo_arg: &str,
    is_write: bool,
) -> Result<(PathBuf, bool)> {
    let path = repo_arg.trim_start_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let (classroom_slug, repo_name, target_user_id) = match segments.len() {
        2 => (segments[0], segments[1], None),
        3 => {
            let target = segments[2];
            let user_id = if let Ok(id) = target.parse::<i64>() {
                id
            } else {
                // Treat the segment as a sanitized username.
                let name = target.replace(' ', "");
                let conn = open_db(&ctx.config.db_path())?;
                find_user_by_sanitized_name(&conn, &name)
                    .context("loading user by name")?
                    .context("user not found")?
                    .id
            };
            (segments[0], segments[1], Some(user_id))
        }
        _ => anyhow::bail!(
            "invalid repo path; expected /<classroom>/<repo>.git or /<classroom>/<repo>/<user>.git"
        ),
    };

    let conn = open_db(&ctx.config.db_path())?;
    let classroom = find_classroom_by_slug(&conn, classroom_slug)
        .context("loading classroom")?
        .context("classroom not found")?;

    let user = ctx
        .authenticated_user
        .as_ref()
        .context("git operations require registration")?;

    let membership = find_classroom_membership(&conn, user.id, classroom.id)
        .context("loading membership")?
        .context("you are not a member of this classroom")?;

    if is_write && !membership.active {
        anyhow::bail!("inactive members cannot push");
    }

    match (target_user_id, membership.role) {
        (None, Role::Teacher) => {
            // Teacher accessing /<classroom>/<repo>.git: the template repo.
            Ok((
                ctx.config
                    .classroom_template_path(classroom_slug, repo_name),
                false,
            ))
        }
        (None, Role::Ta) => {
            // TA can read the template.
            if is_write {
                anyhow::bail!("TAs cannot push to templates");
            }
            Ok((
                ctx.config
                    .classroom_template_path(classroom_slug, repo_name),
                false,
            ))
        }
        (None, Role::Student) => {
            // Student accessing their own copy.
            if is_write && !membership.active {
                anyhow::bail!("inactive students cannot push");
            }
            let student_repo = ctx
                .config
                .student_repo_path(classroom_slug, repo_name, user.id);
            if !student_repo.exists() {
                let template_path = ctx
                    .config
                    .classroom_template_path(classroom_slug, repo_name);
                if template_path.exists() {
                    init_student_repo_from_template(&template_path, &student_repo).await?;
                }
            }
            Ok((student_repo, true))
        }
        (Some(target_id), Role::Teacher) | (Some(target_id), Role::Ta) => {
            // Staff accessing a specific student's repo.
            if is_write {
                anyhow::bail!("staff cannot push to student repositories via this path");
            }
            let target_membership = find_classroom_membership(&conn, target_id, classroom.id)
                .context("loading target membership")?
                .context("target user is not a member of this classroom")?;
            if target_membership.role != Role::Student {
                anyhow::bail!("staff repo path only valid for student repositories");
            }
            Ok((
                ctx.config
                    .student_repo_path(classroom_slug, repo_name, target_id),
                false,
            ))
        }
        (Some(_), Role::Student) => {
            anyhow::bail!("students cannot access other students' repositories");
        }
    }
}

async fn init_student_repo_from_template(
    template_path: &std::path::Path,
    student_repo: &std::path::Path,
) -> Result<()> {
    let parent = student_repo
        .parent()
        .context("student repo has no parent")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating student repo parent {:?}", parent))?;

    let output = tokio::process::Command::new("git")
        .arg("clone")
        .arg("--bare")
        .arg(template_path)
        .arg(student_repo)
        .output()
        .await
        .with_context(|| format!("cloning template to {:?}", student_repo))?;
    if !output.status.success() {
        anyhow::bail!(
            "git clone template failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    sync_upstream_branch_inner(template_path, student_repo).await?;
    Ok(())
}

async fn sync_upstream_branch(ctx: &CommandContext, student_repo: &std::path::Path) -> Result<()> {
    let path_str = student_repo.to_string_lossy();
    let segments: Vec<&str> = path_str.split('/').collect();
    // Path shape: .../repos/<slug>/<repo>/students/<id>.git
    let len = segments.len();
    if len < 5 {
        anyhow::bail!("unexpected student repo path shape");
    }
    let classroom_slug = segments[len - 5];
    let repo_name = segments[len - 4];
    let template_path = ctx
        .config
        .classroom_template_path(classroom_slug, repo_name);

    sync_upstream_branch_inner(&template_path, student_repo).await
}

async fn sync_upstream_branch_inner(
    template_path: &std::path::Path,
    student_repo: &std::path::Path,
) -> Result<()> {
    // Ensure the template is a remote and fetch it.
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(student_repo)
        .arg("remote")
        .arg("add")
        .arg("template")
        .arg(template_path)
        .output()
        .await
        .context("adding template remote")?;

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(student_repo)
        .arg("fetch")
        .arg("template")
        .output()
        .await
        .context("fetching template")?;
    if !output.status.success() {
        anyhow::bail!(
            "template fetch failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Determine the default branch on the template remote.
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(student_repo)
        .arg("rev-parse")
        .arg("--symbolic-full-name")
        .arg("template/HEAD")
        .output()
        .await
        .context("resolving template HEAD")?;
    let upstream_ref = if output.status.success() {
        let head_ref = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if head_ref.starts_with("refs/remotes/template/") {
            head_ref
        } else {
            "refs/remotes/template/main".to_string()
        }
    } else {
        // Fallback: try common default branches.
        for branch in ["main", "master"] {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(student_repo)
                .arg("rev-parse")
                .arg(format!("template/{}", branch))
                .output()
                .await
                .context("resolving template branch")?;
            if output.status.success() {
                return update_upstream_ref(student_repo, &format!("template/{}", branch)).await;
            }
        }
        anyhow::bail!("could not determine template default branch");
    };

    update_upstream_ref(student_repo, &upstream_ref).await
}

async fn update_upstream_ref(student_repo: &std::path::Path, src_ref: &str) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(student_repo)
        .arg("update-ref")
        .arg("refs/heads/upstream")
        .arg(src_ref)
        .output()
        .await
        .context("updating upstream branch")?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to update upstream branch: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

async fn send_stderr(ctx: &CommandContext, msg: &str) -> Result<()> {
    ctx.handle
        .extended_data(ctx.channel_id, 1, msg.as_bytes().to_vec())
        .await
        .map_err(|_| anyhow::anyhow!("failed to send stderr"))?;
    Ok(())
}

async fn send_exit(ctx: &CommandContext, code: u32) -> Result<()> {
    ctx.handle
        .exit_status_request(ctx.channel_id, code)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send exit status"))?;
    let _ = ctx.handle.eof(ctx.channel_id).await;
    let _ = ctx.handle.close(ctx.channel_id).await;
    Ok(())
}
