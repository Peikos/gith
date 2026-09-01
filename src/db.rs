use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Teacher,
    Student,
}

impl Role {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "teacher" => Some(Role::Teacher),
            "student" => Some(Role::Student),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub ssh_public_key: String,
    pub key_fingerprint: String,
    pub role: Role,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct RegistrationToken {
    pub id: i64,
    pub token: String,
    pub created_by: i64,
    pub used_by: Option<i64>,
    pub created_at: i64,
}

pub fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            ssh_public_key TEXT NOT NULL UNIQUE,
            key_fingerprint TEXT NOT NULL UNIQUE,
            role TEXT NOT NULL CHECK(role IN ('teacher', 'student')),
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS registration_tokens (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            token TEXT NOT NULL UNIQUE,
            created_by INTEGER NOT NULL REFERENCES users(id),
            used_by INTEGER REFERENCES users(id),
            created_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_users_fingerprint ON users(key_fingerprint);
        CREATE INDEX IF NOT EXISTS idx_tokens_token ON registration_tokens(token);
        ",
    )
    .context("creating database schema")?;
    Ok(())
}

pub fn open_db(path: &std::path::Path) -> Result<Connection> {
    Connection::open(path).with_context(|| format!("opening database at {:?}", path))
}

pub fn add_teacher(conn: &Connection, name: &str, ssh_public_key: &str) -> Result<i64> {
    let key = parse_public_key(ssh_public_key)?;
    let fingerprint = compute_fingerprint(&key);
    let now = now_secs();

    conn.execute(
        "INSERT INTO users (name, ssh_public_key, key_fingerprint, role, created_at)
         VALUES (?1, ?2, ?3, 'teacher', ?4)",
        (name, ssh_public_key, &fingerprint, now),
    )
    .context("inserting teacher")?;

    Ok(conn.last_insert_rowid())
}

pub fn create_registration_token(conn: &Connection, created_by: i64) -> Result<String> {
    let token = generate_token();
    let now = now_secs();
    conn.execute(
        "INSERT INTO registration_tokens (token, created_by, created_at) VALUES (?1, ?2, ?3)",
        (&token, created_by, now),
    )
    .context("creating registration token")?;
    Ok(token)
}

pub fn consume_registration_token(
    conn: &mut Connection,
    token: &str,
    name: &str,
    ssh_public_key: &str,
) -> Result<User> {
    let key = parse_public_key(ssh_public_key)?;
    let fingerprint = compute_fingerprint(&key);
    let now = now_secs();

    let tx = conn.transaction().context("starting transaction")?;

    let token_row: Option<(i64, i64)> = tx
        .query_row(
            "SELECT id, created_by FROM registration_tokens WHERE token = ?1 AND used_by IS NULL",
            [token],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .context("looking up token")?;

    let (token_id, _created_by) =
        token_row.context("invalid or already used registration token")?;

    tx.execute(
        "INSERT INTO users (name, ssh_public_key, key_fingerprint, role, created_at)
         VALUES (?1, ?2, ?3, 'student', ?4)",
        (name, ssh_public_key, &fingerprint, now),
    )
    .context("inserting student")?;

    let user_id = tx.last_insert_rowid();

    tx.execute(
        "UPDATE registration_tokens SET used_by = ?1 WHERE id = ?2",
        (user_id, token_id),
    )
    .context("marking token used")?;

    tx.commit().context("committing registration")?;

    Ok(User {
        id: user_id,
        name: name.to_string(),
        ssh_public_key: ssh_public_key.to_string(),
        key_fingerprint: fingerprint,
        role: Role::Student,
        created_at: now,
    })
}

pub fn find_user_by_fingerprint(conn: &Connection, fingerprint: &str) -> Result<Option<User>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, ssh_public_key, key_fingerprint, role, created_at
             FROM users WHERE key_fingerprint = ?1",
        )
        .context("preparing user lookup")?;

    let user = stmt
        .query_row([fingerprint], |row| {
            Ok(User {
                id: row.get(0)?,
                name: row.get(1)?,
                ssh_public_key: row.get(2)?,
                key_fingerprint: row.get(3)?,
                role: Role::from_str(&row.get::<_, String>(4)?).unwrap_or(Role::Student),
                created_at: row.get(5)?,
            })
        })
        .optional()
        .context("querying user by fingerprint")?;

    Ok(user)
}

pub fn list_all_repos(conn: &Connection) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn
        .prepare("SELECT id, name, key_fingerprint FROM users ORDER BY id")
        .context("preparing repo list")?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .context("querying repo list")?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.context("reading repo row")?);
    }
    Ok(result)
}

pub fn parse_public_key(key_text: &str) -> Result<russh::keys::ssh_key::PublicKey> {
    key_text
        .trim()
        .parse::<russh::keys::ssh_key::PublicKey>()
        .with_context(|| "parsing SSH public key")
}

pub fn compute_fingerprint(key: &russh::keys::ssh_key::PublicKey) -> String {
    key.fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
        .to_string()
}

fn generate_token() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 32];
    rng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_conn() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    fn sample_key() -> &'static str {
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDIhz2GK/XCUj4i6Q5yQJNL1MicJsIqHP2BkpHqH6/1o test@example.com"
    }

    #[test]
    fn test_init_and_add_teacher() {
        let conn = in_memory_conn();
        init_db(&conn).unwrap();
        let id = add_teacher(&conn, "Alice", sample_key()).unwrap();
        assert!(id > 0);

        let key = parse_public_key(sample_key()).unwrap();
        let fp = compute_fingerprint(&key);
        let user = find_user_by_fingerprint(&conn, &fp).unwrap();
        assert!(user.is_some());
        let user = user.unwrap();
        assert_eq!(user.name, "Alice");
        assert_eq!(user.role, Role::Teacher);
    }

    #[test]
    fn test_registration_token() {
        let mut conn = in_memory_conn();
        init_db(&conn).unwrap();
        let teacher_id = add_teacher(&conn, "Alice", sample_key()).unwrap();
        let token = create_registration_token(&conn, teacher_id).unwrap();
        assert_eq!(token.len(), 32);

        let student_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGtQUDZStQx0U/jZ2U+5rQfizSjGv0LtG+huuh1vU1pP student@example.com";
        let student = consume_registration_token(&mut conn, &token, "Bob", student_key).unwrap();
        assert_eq!(student.name, "Bob");
        assert_eq!(student.role, Role::Student);

        let reused = consume_registration_token(&mut conn, &token, "Charlie", student_key);
        assert!(reused.is_err());
    }
}
