use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use gith::config::Config;
use gith::db::{add_teacher, create_registration_token, init_db, open_db};
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

#[tokio::test]
async fn test_register_and_git_round_trip() -> Result<()> {
    let data_dir = test_data_dir();
    let config = Config::new(Some(data_dir.clone()))?;
    config.ensure_dirs()?;
    let conn = open_db(&config.db_path())?;
    init_db(&conn)?;

    // Teacher key.
    let teacher_key = data_dir.join("teacher_key");
    generate_key(&teacher_key)?;
    let teacher_pubkey = read_public_key(&teacher_key)?;
    let teacher_id = add_teacher(&conn, "Teacher", &teacher_pubkey)?;

    // Student key.
    let student_key = data_dir.join("student_key");
    generate_key(&student_key)?;

    // Registration token.
    let token = create_registration_token(&conn, teacher_id)?;

    // Start server.
    let (_server_handle, port) = start_server(config).await?;

    // Register the student.
    let mut register = ssh_cmd(port, &student_key, &["register", &token, "Alice"]);
    let output = register
        .output()
        .await
        .context("running register command")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "register failed: stdout={} stderr={}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("Welcome, Alice"),
        "unexpected register response: {}",
        stdout
    );

    // Clone the student's repo.
    let clone_dir = data_dir.join("clone");
    let student_key_str = student_key.to_string_lossy().replace('\\', "/");
    let ssh_command = format!(
        "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i {} -p {}",
        student_key_str, port
    );
    let mut clone = Command::new("git");
    clone
        .env("GIT_SSH_COMMAND", &ssh_command)
        .arg("clone")
        .arg("ssh://git@127.0.0.1/repo.git")
        .arg(&clone_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = clone.output().await.context("running git clone")?;
    assert!(
        output.status.success(),
        "git clone failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Create an explicit branch so push refspec is unambiguous.
    let mut checkout = Command::new("git");
    checkout
        .arg("-C")
        .arg(&clone_dir)
        .arg("checkout")
        .arg("-b")
        .arg("master");
    let output = checkout.output().await?;
    assert!(output.status.success(), "git checkout failed");

    // Make a commit and push.
    let file_path = clone_dir.join("hello.txt");
    tokio::fs::write(&file_path, "hello world\n").await?;

    let mut add = Command::new("git");
    add.arg("-C").arg(&clone_dir).arg("add").arg(".");
    let output = add.output().await?;
    assert!(output.status.success(), "git add failed");

    let mut commit = Command::new("git");
    commit
        .arg("-C")
        .arg(&clone_dir)
        .arg("commit")
        .arg("-m")
        .arg("initial commit");
    let output = commit.output().await?;
    assert!(output.status.success(), "git commit failed");

    let mut push = Command::new("git");
    push.env("GIT_SSH_COMMAND", &ssh_command)
        .arg("-C")
        .arg(&clone_dir)
        .arg("push")
        .arg("origin")
        .arg("master");
    let output = push.output().await.context("running git push")?;
    assert!(
        output.status.success(),
        "git push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(())
}
