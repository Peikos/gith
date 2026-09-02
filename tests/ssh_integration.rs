use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use gith::config::Config;
use gith::db::{add_user, init_db, open_db};
use gith::ssh_server::GithSshServer;
use russh::server::Server as _;
use tokio::net::TcpListener;
use tokio::process::Command;

fn test_data_dir() -> PathBuf {
    let base = std::env::temp_dir().join(format!("gith-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    base
}

fn generate_key(path: &std::path::Path) -> Result<()> {
    let output = std::process::Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(path)
        .arg("-C")
        .arg("test@example.com")
        .output()
        .context("running ssh-keygen")?;
    if !output.status.success() {
        anyhow::bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn read_public_key(path: &std::path::Path) -> Result<String> {
    let pub_path = path.with_extension("pub");
    std::fs::read_to_string(&pub_path).with_context(|| format!("reading {:?}", pub_path))
}

async fn start_server(
    config: Config,
) -> Result<(tokio::task::JoinHandle<Result<(), std::io::Error>>, u16)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let russh_config = std::sync::Arc::new(russh::server::Config {
        inactivity_timeout: Some(Duration::from_secs(30)),
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::from_secs(0)),
        keys: vec![russh::keys::PrivateKey::random(
            &mut rand::thread_rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )?],
        ..Default::default()
    });

    let handle = tokio::spawn(async move {
        let mut server = GithSshServer::new(config);
        server.run_on_socket(russh_config, &listener).await
    });

    Ok((handle, port))
}

fn ssh_cmd(port: u16, key: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg("-i")
        .arg(key)
        .arg("-p")
        .arg(port.to_string())
        .arg("git@127.0.0.1")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn git_ssh_command(key: &std::path::Path, port: u16) -> String {
    let key_str = key.to_string_lossy().replace('\\', "/");
    format!(
        "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i {} -p {}",
        key_str, port
    )
}

fn repo_url(port: u16, classroom: &str, repo: &str) -> String {
    format!("ssh://git@127.0.0.1:{}/{}/{}.git", port, classroom, repo)
}

async fn run_ssh(port: u16, key: &std::path::Path, args: &[&str]) -> Result<(String, String)> {
    let output = ssh_cmd(port, key, args).output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    anyhow::ensure!(
        output.status.success(),
        "ssh command {:?} failed: stdout={} stderr={}",
        args,
        stdout,
        stderr
    );
    Ok((stdout, stderr))
}

#[tokio::test]
async fn test_classroom_git_round_trip() -> Result<()> {
    let data_dir = test_data_dir();
    let config = Config::new(Some(data_dir.clone()))?;
    config.ensure_dirs()?;
    let conn = open_db(&config.db_path())?;
    init_db(&conn)?;

    // Teacher key.
    let teacher_key = data_dir.join("teacher_key");
    generate_key(&teacher_key)?;
    let teacher_pubkey = read_public_key(&teacher_key)?;
    add_user(&conn, "Teacher", &teacher_pubkey)?;

    // Student key.
    let student_key = data_dir.join("student_key");
    generate_key(&student_key)?;

    // Start server.
    let (_server_handle, port) = start_server(config).await?;

    // Teacher creates a classroom.
    run_ssh(
        port,
        &teacher_key,
        &["classroom", "create", "cs101", "CS 101"],
    )
    .await?;

    // Teacher adds a template repository.
    run_ssh(
        port,
        &teacher_key,
        &["classroom", "add-template", "cs101", "lab1"],
    )
    .await?;

    // Teacher clones the template and pushes initial content.
    let teacher_clone = data_dir.join("teacher-clone");
    let teacher_ssh = git_ssh_command(&teacher_key, port);
    let mut clone = Command::new("git");
    clone
        .env("GIT_SSH_COMMAND", &teacher_ssh)
        .arg("clone")
        .arg(repo_url(port, "cs101", "lab1"))
        .arg(&teacher_clone)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = clone.output().await.context("cloning teacher template")?;
    anyhow::ensure!(
        output.status.success(),
        "teacher clone failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut checkout = Command::new("git");
    checkout
        .arg("-C")
        .arg(&teacher_clone)
        .arg("checkout")
        .arg("-b")
        .arg("main");
    let output = checkout.output().await?;
    anyhow::ensure!(output.status.success(), "teacher checkout failed");

    tokio::fs::write(teacher_clone.join("README.md"), "# Lab 1\n").await?;

    let mut add = Command::new("git");
    add.arg("-C").arg(&teacher_clone).arg("add").arg(".");
    let output = add.output().await?;
    anyhow::ensure!(output.status.success(), "teacher git add failed");

    let mut commit = Command::new("git");
    commit
        .arg("-C")
        .arg(&teacher_clone)
        .arg("commit")
        .arg("-m")
        .arg("initial template");
    let output = commit.output().await?;
    anyhow::ensure!(output.status.success(), "teacher git commit failed");

    let mut push = Command::new("git");
    push.env("GIT_SSH_COMMAND", &teacher_ssh)
        .arg("-C")
        .arg(&teacher_clone)
        .arg("push")
        .arg("origin")
        .arg("main");
    let output = push.output().await.context("pushing teacher template")?;
    anyhow::ensure!(
        output.status.success(),
        "teacher push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Teacher creates a student invite token.
    let (token, _) = run_ssh(
        port,
        &teacher_key,
        &["classroom", "invite", "cs101", "student"],
    )
    .await?;
    let token = token.trim();

    // Student registers with a full name.
    let (register_out, register_err) =
        run_ssh(port, &student_key, &["register", token, "Alice", "Smith"]).await?;
    assert!(
        register_out.contains("Welcome, Alice Smith"),
        "unexpected register response: stdout={} stderr={}",
        register_out,
        register_err
    );

    // Student clones the assignment URL and gets their own repo.
    let student_clone = data_dir.join("student-clone");
    let student_ssh = git_ssh_command(&student_key, port);
    let mut clone = Command::new("git");
    clone
        .env("GIT_SSH_COMMAND", &student_ssh)
        .arg("clone")
        .arg(repo_url(port, "cs101", "lab1"))
        .arg(&student_clone)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = clone.output().await.context("cloning student repo")?;
    anyhow::ensure!(
        output.status.success(),
        "student clone failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the student repo has both main and upstream branches.
    let mut branch = Command::new("git");
    branch.arg("-C").arg(&student_clone).arg("branch").arg("-a");
    let output = branch.output().await?;
    let branches = String::from_utf8_lossy(&output.stdout);
    assert!(
        branches.contains("main"),
        "missing main branch: {}",
        branches
    );
    assert!(
        branches.contains("upstream"),
        "missing upstream branch: {}",
        branches
    );

    // Student pushes a commit.
    tokio::fs::write(student_clone.join("answer.txt"), "42\n").await?;

    let mut add = Command::new("git");
    add.arg("-C").arg(&student_clone).arg("add").arg(".");
    let output = add.output().await?;
    anyhow::ensure!(output.status.success(), "student git add failed");

    let mut commit = Command::new("git");
    commit
        .arg("-C")
        .arg(&student_clone)
        .arg("commit")
        .arg("-m")
        .arg("answer");
    let output = commit.output().await?;
    anyhow::ensure!(output.status.success(), "student git commit failed");

    let mut push = Command::new("git");
    push.env("GIT_SSH_COMMAND", &student_ssh)
        .arg("-C")
        .arg(&student_clone)
        .arg("push")
        .arg("origin")
        .arg("main");
    let output = push.output().await.context("pushing student repo")?;
    anyhow::ensure!(
        output.status.success(),
        "student push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Teacher clones the student's repository by username.
    let teacher_student_clone = data_dir.join("teacher-student-clone");
    let mut clone = Command::new("git");
    clone
        .env("GIT_SSH_COMMAND", &teacher_ssh)
        .arg("clone")
        .arg(repo_url(port, "cs101", "lab1/AliceSmith"))
        .arg(&teacher_student_clone)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = clone
        .output()
        .await
        .context("teacher cloning student repo by name")?;
    anyhow::ensure!(
        output.status.success(),
        "teacher clone by username failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut log = Command::new("git");
    log.arg("-C")
        .arg(&teacher_student_clone)
        .arg("log")
        .arg("origin/main")
        .arg("--oneline");
    let output = log.output().await?;
    let student_log = String::from_utf8_lossy(&output.stdout);
    assert!(
        student_log.contains("answer"),
        "teacher view of student repo missing answer commit: {}",
        student_log
    );

    // Teacher updates the template.
    tokio::fs::write(teacher_clone.join("NEW.md"), "new material\n").await?;

    let mut add = Command::new("git");
    add.arg("-C").arg(&teacher_clone).arg("add").arg(".");
    let output = add.output().await?;
    anyhow::ensure!(output.status.success(), "teacher git add failed");

    let mut commit = Command::new("git");
    commit
        .arg("-C")
        .arg(&teacher_clone)
        .arg("commit")
        .arg("-m")
        .arg("template update");
    let output = commit.output().await?;
    anyhow::ensure!(output.status.success(), "teacher git commit failed");

    let mut push = Command::new("git");
    push.env("GIT_SSH_COMMAND", &teacher_ssh)
        .arg("-C")
        .arg(&teacher_clone)
        .arg("push")
        .arg("origin")
        .arg("main");
    let output = push.output().await.context("pushing template update")?;
    anyhow::ensure!(
        output.status.success(),
        "teacher template update push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Student fetches and sees the new upstream commit.
    let mut fetch = Command::new("git");
    fetch
        .env("GIT_SSH_COMMAND", &student_ssh)
        .arg("-C")
        .arg(&student_clone)
        .arg("fetch")
        .arg("origin");
    let output = fetch.output().await.context("student fetch upstream")?;
    anyhow::ensure!(
        output.status.success(),
        "student fetch failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut log = Command::new("git");
    log.arg("-C")
        .arg(&student_clone)
        .arg("log")
        .arg("origin/upstream")
        .arg("--oneline");
    let output = log.output().await?;
    let upstream_log = String::from_utf8_lossy(&output.stdout);
    assert!(
        upstream_log.contains("template update"),
        "origin/upstream missing template update: {}",
        upstream_log
    );

    // Teacher downloads an archive of active student repos.
    let mut download = ssh_cmd(
        port,
        &teacher_key,
        &["classroom", "download", "cs101", "lab1"],
    );
    let output = download.output().await.context("downloading archive")?;
    anyhow::ensure!(
        output.status.success(),
        "download failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.is_empty(), "download produced empty archive");

    // Verify the archive can be listed by tar.
    let mut list = Command::new("tar");
    list.arg("tz")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = list.spawn().context("spawning tar tz")?;
    let mut stdin = child.stdin.take().unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut stdin, &output.stdout).await?;
    drop(stdin);
    let output = child.wait_with_output().await?;
    anyhow::ensure!(
        output.status.success(),
        "tar tz failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(
        listing.contains(".git/"),
        "archive listing missing repos: {}",
        listing
    );

    // Teacher deactivates the student.
    run_ssh(
        port,
        &teacher_key,
        &["classroom", "deactivate", "cs101", "2"],
    )
    .await?;

    // Listing active students should no longer include Alice.
    let (list_out, _) = run_ssh(port, &teacher_key, &["classroom", "list", "cs101"]).await?;
    assert!(
        list_out.contains("active: no"),
        "deactivated student not reflected in listing: {}",
        list_out
    );

    Ok(())
}
