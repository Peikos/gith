use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use gith::config::Config;
use gith::db::{add_user, init_db, open_db};
use gith::ssh_server::GithSshServer;
use rand::RngCore;
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

async fn register_and_push_student(
    port: u16,
    token: &str,
    key: &std::path::Path,
    data_dir: &std::path::Path,
    first_name: &str,
    last_name: &str,
    clone_dir_name: &str,
) -> Result<PathBuf> {
    let name = format!("{} {}", first_name, last_name);
    let (register_out, register_err) =
        run_ssh(port, key, &["register", token, first_name, last_name]).await?;
    assert!(
        register_out.contains(&format!("Welcome, {}", name)),
        "unexpected register response for {}: stdout={} stderr={}",
        name,
        register_out,
        register_err
    );

    let clone_dir = data_dir.join(clone_dir_name);
    let ssh = git_ssh_command(key, port);
    let mut clone = Command::new("git");
    clone
        .env("GIT_SSH_COMMAND", &ssh)
        .arg("clone")
        .arg(repo_url(port, "cs101", "lab1"))
        .arg(&clone_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = clone
        .output()
        .await
        .with_context(|| format!("cloning {}'s repo", name))?;
    anyhow::ensure!(
        output.status.success(),
        "{} clone failed: stdout={} stderr={}",
        name,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut branch = Command::new("git");
    branch.arg("-C").arg(&clone_dir).arg("branch").arg("-a");
    let output = branch.output().await?;
    let branches = String::from_utf8_lossy(&output.stdout);
    assert!(
        branches.contains("main"),
        "{} missing main branch: {}",
        name,
        branches
    );
    assert!(
        branches.contains("upstream"),
        "{} missing upstream branch: {}",
        name,
        branches
    );

    tokio::fs::write(clone_dir.join("answer.txt"), "42\n").await?;

    // Use incompressible data so the resulting archive is large enough to
    // exercise the bulk-transfer code path (> 73 KB).
    let mut noise = vec![0u8; 100_000];
    rand::thread_rng().fill_bytes(&mut noise);
    tokio::fs::write(clone_dir.join("large.bin"), noise).await?;

    let mut add = Command::new("git");
    add.arg("-C").arg(&clone_dir).arg("add").arg(".");
    let output = add.output().await?;
    anyhow::ensure!(output.status.success(), "{} git add failed", name);

    let mut commit = Command::new("git");
    commit
        .arg("-C")
        .arg(&clone_dir)
        .arg("commit")
        .arg("-m")
        .arg("answer");
    let output = commit.output().await?;
    anyhow::ensure!(output.status.success(), "{} git commit failed", name);

    let mut push = Command::new("git");
    push.env("GIT_SSH_COMMAND", &ssh)
        .arg("-C")
        .arg(&clone_dir)
        .arg("push")
        .arg("origin")
        .arg("main");
    let output = push
        .output()
        .await
        .with_context(|| format!("pushing {}'s repo", name))?;
    anyhow::ensure!(
        output.status.success(),
        "{} push failed: stdout={} stderr={}",
        name,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(clone_dir)
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

    // Student keys.
    let student_key = data_dir.join("student_key");
    generate_key(&student_key)?;
    let student2_key = data_dir.join("student2_key");
    generate_key(&student2_key)?;

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

    // Teacher creates a reusable student invite token.
    let (token, _) = run_ssh(
        port,
        &teacher_key,
        &["classroom", "invite", "cs101", "student"],
    )
    .await?;
    let token = token.trim();

    // Register and push work for two students in parallel, using the same invite token.
    let (student_clone, _student2_clone) = tokio::try_join!(
        register_and_push_student(
            port,
            token,
            &student_key,
            &data_dir,
            "Alice",
            "Smith",
            "student-clone"
        ),
        register_and_push_student(
            port,
            token,
            &student2_key,
            &data_dir,
            "Bob",
            "Smith",
            "student2-clone"
        )
    )?;

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
        .env("GIT_SSH_COMMAND", git_ssh_command(&student_key, port))
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
    assert!(
        output.stdout.len() > 73_728,
        "download archive is too small ({} bytes); should exercise the bulk-transfer path",
        output.stdout.len()
    );

    // Verify the archive can be listed by tar and contains both students.
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
        listing.contains("AliceSmith.git/"),
        "archive listing missing Alice's repo: {}",
        listing
    );
    assert!(
        listing.contains("BobSmith.git/"),
        "archive listing missing Bob's repo: {}",
        listing
    );

    // Teacher deactivates both students.
    run_ssh(
        port,
        &teacher_key,
        &["classroom", "deactivate", "cs101", "2"],
    )
    .await?;
    run_ssh(
        port,
        &teacher_key,
        &["classroom", "deactivate", "cs101", "3"],
    )
    .await?;

    // Listing should show both students as inactive now.
    let (list_out, _) = run_ssh(port, &teacher_key, &["classroom", "list", "cs101"]).await?;
    assert_eq!(
        list_out.matches("active: no").count(),
        2,
        "deactivated students not reflected in listing: {}",
        list_out
    );

    Ok(())
}
