use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rand::Rng;
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Teacher,
    Ta,
    Student,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Teacher => "teacher",
            Role::Ta => "ta",
            Role::Student => "student",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "teacher" => Ok(Role::Teacher),
            "ta" => Ok(Role::Ta),
            "student" => Ok(Role::Student),
            _ => anyhow::bail!("unknown role: {}", s),
        }
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub ssh_public_key: String,
    pub key_fingerprint: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct Classroom {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub created_by: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct ClassroomMembership {
    pub id: i64,
    pub user_id: i64,
    pub classroom_id: i64,
    pub role: Role,
    pub active: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct ClassroomTemplate {
    pub id: i64,
    pub classroom_id: i64,
    pub repo_name: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct ClassroomInviteToken {
    pub id: i64,
    pub classroom_id: i64,
    pub token: String,
    pub role: Role,
    pub created_at: i64,
}

pub fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            name_sanitized TEXT NOT NULL UNIQUE GENERATED ALWAYS AS (replace(name, ' ', '')) STORED,
            ssh_public_key TEXT NOT NULL UNIQUE,
            key_fingerprint TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS classrooms (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            slug TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            created_by INTEGER NOT NULL REFERENCES users(id),
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS classroom_memberships (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL REFERENCES users(id),
            classroom_id INTEGER NOT NULL REFERENCES classrooms(id),
            role TEXT NOT NULL CHECK(role IN ('teacher', 'ta', 'student')),
            active INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            UNIQUE(user_id, classroom_id)
        );

        CREATE TABLE IF NOT EXISTS classroom_templates (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            classroom_id INTEGER NOT NULL REFERENCES classrooms(id),
            repo_name TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE(classroom_id, repo_name)
        );

        CREATE TABLE IF NOT EXISTS classroom_invite_tokens (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            classroom_id INTEGER NOT NULL REFERENCES classrooms(id),
            token TEXT NOT NULL UNIQUE,
            role TEXT NOT NULL CHECK(role IN ('teacher', 'ta', 'student')),
            created_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_users_fingerprint ON users(key_fingerprint);
        CREATE INDEX IF NOT EXISTS idx_tokens_token ON classroom_invite_tokens(token);
        CREATE INDEX IF NOT EXISTS idx_memberships_user ON classroom_memberships(user_id);
        CREATE INDEX IF NOT EXISTS idx_memberships_classroom ON classroom_memberships(classroom_id);
        ",
    )
    .context("creating database schema")?;
    Ok(())
}

pub fn open_db(path: &std::path::Path) -> Result<Connection> {
    Connection::open(path).with_context(|| format!("opening database at {:?}", path))
}

pub fn add_user(conn: &Connection, name: &str, ssh_public_key: &str) -> Result<i64> {
    let key = parse_public_key(ssh_public_key)?;
    let fingerprint = compute_fingerprint(&key);
    let now = now_secs();

    conn.execute(
        "INSERT INTO users (name, ssh_public_key, key_fingerprint, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        (name, ssh_public_key, &fingerprint, now),
    )
    .context("inserting user")?;

    Ok(conn.last_insert_rowid())
}

pub fn find_user_by_fingerprint(conn: &Connection, fingerprint: &str) -> Result<Option<User>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, ssh_public_key, key_fingerprint, created_at
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
                created_at: row.get(4)?,
            })
        })
        .optional()
        .context("querying user by fingerprint")?;

    Ok(user)
}

pub fn find_user_by_id(conn: &Connection, id: i64) -> Result<Option<User>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, ssh_public_key, key_fingerprint, created_at
             FROM users WHERE id = ?1",
        )
        .context("preparing user lookup by id")?;

    let user = stmt
        .query_row([id], |row| {
            Ok(User {
                id: row.get(0)?,
                name: row.get(1)?,
                ssh_public_key: row.get(2)?,
                key_fingerprint: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .optional()
        .context("querying user by id")?;

    Ok(user)
}

pub fn find_user_by_name(conn: &Connection, name: &str) -> Result<Option<User>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, ssh_public_key, key_fingerprint, created_at
             FROM users WHERE name = ?1",
        )
        .context("preparing user lookup by name")?;

    let user = stmt
        .query_row([name], |row| {
            Ok(User {
                id: row.get(0)?,
                name: row.get(1)?,
                ssh_public_key: row.get(2)?,
                key_fingerprint: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .optional()
        .context("querying user by name")?;

    Ok(user)
}

pub fn find_user_by_sanitized_name(conn: &Connection, name: &str) -> Result<Option<User>> {
    let sanitized = name.replace(' ', "");
    let mut stmt = conn
        .prepare(
            "SELECT id, name, ssh_public_key, key_fingerprint, created_at
             FROM users WHERE name_sanitized = ?1",
        )
        .context("preparing user lookup by sanitized name")?;

    let user = stmt
        .query_row([sanitized], |row| {
            Ok(User {
                id: row.get(0)?,
                name: row.get(1)?,
                ssh_public_key: row.get(2)?,
                key_fingerprint: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .optional()
        .context("querying user by sanitized name")?;

    Ok(user)
}

pub fn create_classroom(
    conn: &mut Connection,
    slug: &str,
    name: &str,
    created_by: i64,
) -> Result<Classroom> {
    let now = now_secs();
    let tx = conn.transaction().context("starting transaction")?;

    tx.execute(
        "INSERT INTO classrooms (slug, name, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        (slug, name, created_by, now),
    )
    .context("inserting classroom")?;

    let classroom_id = tx.last_insert_rowid();

    tx.execute(
        "INSERT INTO classroom_memberships (user_id, classroom_id, role, active, created_at)
         VALUES (?1, ?2, 'teacher', 1, ?3)",
        (created_by, classroom_id, now),
    )
    .context("adding creator as teacher")?;

    tx.commit().context("committing classroom creation")?;

    Ok(Classroom {
        id: classroom_id,
        slug: slug.to_string(),
        name: name.to_string(),
        created_by,
        created_at: now,
    })
}

pub fn find_classroom_by_slug(conn: &Connection, slug: &str) -> Result<Option<Classroom>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, slug, name, created_by, created_at
             FROM classrooms WHERE slug = ?1",
        )
        .context("preparing classroom lookup")?;

    let classroom = stmt
        .query_row([slug], |row| {
            Ok(Classroom {
                id: row.get(0)?,
                slug: row.get(1)?,
                name: row.get(2)?,
                created_by: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .optional()
        .context("querying classroom by slug")?;

    Ok(classroom)
}

pub fn find_classroom_by_id(conn: &Connection, id: i64) -> Result<Option<Classroom>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, slug, name, created_by, created_at
             FROM classrooms WHERE id = ?1",
        )
        .context("preparing classroom lookup by id")?;

    let classroom = stmt
        .query_row([id], |row| {
            Ok(Classroom {
                id: row.get(0)?,
                slug: row.get(1)?,
                name: row.get(2)?,
                created_by: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .optional()
        .context("querying classroom by id")?;

    Ok(classroom)
}

pub fn add_classroom_membership(
    conn: &Connection,
    user_id: i64,
    classroom_id: i64,
    role: Role,
) -> Result<i64> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO classroom_memberships (user_id, classroom_id, role, active, created_at)
         VALUES (?1, ?2, ?3, 1, ?4)",
        (user_id, classroom_id, role.as_str(), now),
    )
    .context("inserting classroom membership")?;
    Ok(conn.last_insert_rowid())
}

pub fn find_classroom_membership(
    conn: &Connection,
    user_id: i64,
    classroom_id: i64,
) -> Result<Option<ClassroomMembership>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, user_id, classroom_id, role, active, created_at
             FROM classroom_memberships
             WHERE user_id = ?1 AND classroom_id = ?2",
        )
        .context("preparing membership lookup")?;

    let membership = stmt
        .query_row([user_id, classroom_id], |row| {
            Ok(ClassroomMembership {
                id: row.get(0)?,
                user_id: row.get(1)?,
                classroom_id: row.get(2)?,
                role: row
                    .get::<_, String>(3)?
                    .parse::<Role>()
                    .unwrap_or(Role::Student),
                active: row.get::<_, i64>(4)? != 0,
                created_at: row.get(5)?,
            })
        })
        .optional()
        .context("querying classroom membership")?;

    Ok(membership)
}

pub fn set_membership_active(
    conn: &Connection,
    classroom_id: i64,
    user_id: i64,
    active: bool,
) -> Result<usize> {
    let rows = conn
        .execute(
            "UPDATE classroom_memberships SET active = ?1
             WHERE classroom_id = ?2 AND user_id = ?3",
            (if active { 1 } else { 0 }, classroom_id, user_id),
        )
        .context("updating membership active flag")?;
    Ok(rows)
}

pub fn list_classroom_memberships(
    conn: &Connection,
    classroom_id: i64,
    active_only: bool,
) -> Result<Vec<(User, ClassroomMembership)>> {
    let sql = if active_only {
        "SELECT u.id, u.name, u.ssh_public_key, u.key_fingerprint, u.created_at,
                m.id, m.user_id, m.classroom_id, m.role, m.active, m.created_at
         FROM users u
         JOIN classroom_memberships m ON u.id = m.user_id
         WHERE m.classroom_id = ?1 AND m.active = 1
         ORDER BY u.name"
    } else {
        "SELECT u.id, u.name, u.ssh_public_key, u.key_fingerprint, u.created_at,
                m.id, m.user_id, m.classroom_id, m.role, m.active, m.created_at
         FROM users u
         JOIN classroom_memberships m ON u.id = m.user_id
         WHERE m.classroom_id = ?1
         ORDER BY u.name"
    };

    let mut stmt = conn.prepare(sql).context("preparing membership list")?;
    let rows = stmt
        .query_map([classroom_id], |row| {
            Ok((
                User {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    ssh_public_key: row.get(2)?,
                    key_fingerprint: row.get(3)?,
                    created_at: row.get(4)?,
                },
                ClassroomMembership {
                    id: row.get(5)?,
                    user_id: row.get(6)?,
                    classroom_id: row.get(7)?,
                    role: row
                        .get::<_, String>(8)?
                        .parse::<Role>()
                        .unwrap_or(Role::Student),
                    active: row.get::<_, i64>(9)? != 0,
                    created_at: row.get(10)?,
                },
            ))
        })
        .context("querying classroom memberships")?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.context("reading membership row")?);
    }
    Ok(result)
}

pub fn list_user_memberships(
    conn: &Connection,
    user_id: i64,
    active_only: bool,
) -> Result<Vec<(Classroom, ClassroomMembership)>> {
    let sql = if active_only {
        "SELECT c.id, c.slug, c.name, c.created_by, c.created_at,
                m.id, m.user_id, m.classroom_id, m.role, m.active, m.created_at
         FROM classrooms c
         JOIN classroom_memberships m ON c.id = m.classroom_id
         WHERE m.user_id = ?1 AND m.active = 1
         ORDER BY c.name"
    } else {
        "SELECT c.id, c.slug, c.name, c.created_by, c.created_at,
                m.id, m.user_id, m.classroom_id, m.role, m.active, m.created_at
         FROM classrooms c
         JOIN classroom_memberships m ON c.id = m.classroom_id
         WHERE m.user_id = ?1
         ORDER BY c.name"
    };

    let mut stmt = conn
        .prepare(sql)
        .context("preparing user membership list")?;
    let rows = stmt
        .query_map([user_id], |row| {
            Ok((
                Classroom {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    name: row.get(2)?,
                    created_by: row.get(3)?,
                    created_at: row.get(4)?,
                },
                ClassroomMembership {
                    id: row.get(5)?,
                    user_id: row.get(6)?,
                    classroom_id: row.get(7)?,
                    role: row
                        .get::<_, String>(8)?
                        .parse::<Role>()
                        .unwrap_or(Role::Student),
                    active: row.get::<_, i64>(9)? != 0,
                    created_at: row.get(10)?,
                },
            ))
        })
        .context("querying user memberships")?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.context("reading user membership row")?);
    }
    Ok(result)
}

pub fn create_classroom_template(
    conn: &Connection,
    classroom_id: i64,
    repo_name: &str,
) -> Result<ClassroomTemplate> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO classroom_templates (classroom_id, repo_name, created_at)
         VALUES (?1, ?2, ?3)",
        (classroom_id, repo_name, now),
    )
    .context("inserting classroom template")?;

    Ok(ClassroomTemplate {
        id: conn.last_insert_rowid(),
        classroom_id,
        repo_name: repo_name.to_string(),
        created_at: now,
    })
}

pub fn find_classroom_template(
    conn: &Connection,
    classroom_id: i64,
    repo_name: &str,
) -> Result<Option<ClassroomTemplate>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, classroom_id, repo_name, created_at
             FROM classroom_templates
             WHERE classroom_id = ?1 AND repo_name = ?2",
        )
        .context("preparing template lookup")?;

    let template = stmt
        .query_row(rusqlite::params![classroom_id, repo_name], |row| {
            Ok(ClassroomTemplate {
                id: row.get(0)?,
                classroom_id: row.get(1)?,
                repo_name: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .optional()
        .context("querying classroom template")?;

    Ok(template)
}

pub fn list_classroom_templates(
    conn: &Connection,
    classroom_id: i64,
) -> Result<Vec<ClassroomTemplate>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, classroom_id, repo_name, created_at
             FROM classroom_templates
             WHERE classroom_id = ?1
             ORDER BY repo_name",
        )
        .context("preparing template list")?;

    let rows = stmt
        .query_map([classroom_id], |row| {
            Ok(ClassroomTemplate {
                id: row.get(0)?,
                classroom_id: row.get(1)?,
                repo_name: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .context("querying templates")?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.context("reading template row")?);
    }
    Ok(result)
}

pub fn create_classroom_invite_token(
    conn: &Connection,
    classroom_id: i64,
    role: Role,
) -> Result<String> {
    let token = generate_token();
    let now = now_secs();
    conn.execute(
        "INSERT INTO classroom_invite_tokens (classroom_id, token, role, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        (classroom_id, &token, role.as_str(), now),
    )
    .context("creating invite token")?;
    Ok(token)
}

pub fn consume_classroom_invite_token(
    conn: &mut Connection,
    token: &str,
    name: Option<&str>,
    ssh_public_key: Option<&str>,
) -> Result<(User, ClassroomMembership)> {
    let now = now_secs();
    let tx = conn.transaction().context("starting transaction")?;

    let token_row: Option<(i64, String)> = tx
        .query_row(
            "SELECT classroom_id, role
             FROM classroom_invite_tokens
             WHERE token = ?1",
            [token],
            |row| Ok((row.get(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .context("looking up invite token")?;

    let (classroom_id, role_str) = token_row.context("invalid invite token")?;
    let role = role_str.parse::<Role>().context("invalid role in token")?;

    let key_text = ssh_public_key.context("registration requires an SSH public key")?;
    let key = parse_public_key(key_text)?;
    let fingerprint = compute_fingerprint(&key);

    let user: User = match tx
        .query_row(
            "SELECT id, name, ssh_public_key, key_fingerprint, created_at
             FROM users WHERE key_fingerprint = ?1",
            [&fingerprint],
            |row| {
                Ok(User {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    ssh_public_key: row.get(2)?,
                    key_fingerprint: row.get(3)?,
                    created_at: row.get(4)?,
                })
            },
        )
        .optional()
        .context("looking up existing user")?
    {
        Some(existing) => {
            // Update name if provided and different.
            if let Some(new_name) = name {
                if new_name != existing.name {
                    tx.execute(
                        "UPDATE users SET name = ?1 WHERE id = ?2",
                        (new_name, existing.id),
                    )
                    .context("updating user name")?;
                }
            }
            existing
        }
        None => {
            let user_name = name.unwrap_or("Unnamed");
            tx.execute(
                "INSERT INTO users (name, ssh_public_key, key_fingerprint, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                (user_name, key_text, &fingerprint, now),
            )
            .context("inserting user")?;
            let user_id = tx.last_insert_rowid();
            User {
                id: user_id,
                name: user_name.to_string(),
                ssh_public_key: key_text.to_string(),
                key_fingerprint: fingerprint,
                created_at: now,
            }
        }
    };

    // Create or replace membership.
    tx.execute(
        "INSERT INTO classroom_memberships (user_id, classroom_id, role, active, created_at)
         VALUES (?1, ?2, ?3, 1, ?4)
         ON CONFLICT(user_id, classroom_id)
         DO UPDATE SET role = excluded.role, active = 1, created_at = excluded.created_at",
        (user.id, classroom_id, role.as_str(), now),
    )
    .context("inserting classroom membership")?;

    let membership_id = tx.last_insert_rowid();

    tx.commit().context("committing registration")?;

    let membership = ClassroomMembership {
        id: membership_id,
        user_id: user.id,
        classroom_id,
        role,
        active: true,
        created_at: now,
    };

    Ok((user, membership))
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
    let mut rng = rand::rng();
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

    fn sample_key_2() -> &'static str {
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGtQUDZStQx0U/jZ2U+5rQfizSjGv0LtG+huuh1vU1pP student@example.com"
    }

    fn sample_key_3() -> &'static str {
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFWbeU37/ZtwplnwqRgglIoQl9D5JH5+ecIUBV3DAv4g test3@example.com"
    }

    #[test]
    fn test_init_and_add_user() {
        let conn = in_memory_conn();
        init_db(&conn).unwrap();
        let id = add_user(&conn, "Alice", sample_key()).unwrap();
        assert!(id > 0);

        let key = parse_public_key(sample_key()).unwrap();
        let fp = compute_fingerprint(&key);
        let user = find_user_by_fingerprint(&conn, &fp).unwrap();
        assert!(user.is_some());
        let user = user.unwrap();
        assert_eq!(user.name, "Alice");
    }

    #[test]
    fn test_classroom_lifecycle() {
        let mut conn = in_memory_conn();
        init_db(&conn).unwrap();

        let teacher_id = add_user(&conn, "Alice", sample_key()).unwrap();
        let classroom = create_classroom(&mut conn, "cs101", "CS 101", teacher_id).unwrap();
        assert_eq!(classroom.slug, "cs101");

        let found = find_classroom_by_slug(&conn, "cs101").unwrap();
        assert!(found.is_some());

        let membership = find_classroom_membership(&conn, teacher_id, classroom.id).unwrap();
        assert!(membership.is_some());
        assert_eq!(membership.unwrap().role, Role::Teacher);

        let token = create_classroom_invite_token(&conn, classroom.id, Role::Student).unwrap();
        assert_eq!(token.len(), 32);

        let (student, membership) =
            consume_classroom_invite_token(&mut conn, &token, Some("Bob"), Some(sample_key_2()))
                .unwrap();
        assert_eq!(student.name, "Bob");
        assert_eq!(membership.role, Role::Student);
        assert!(membership.active);

        // The same token can be used by another student.
        let (reused_student, reused_membership) = consume_classroom_invite_token(
            &mut conn,
            &token,
            Some("Charlie"),
            Some(sample_key_3()),
        )
        .unwrap();
        assert_eq!(reused_student.name, "Charlie");
        assert_eq!(reused_membership.role, Role::Student);
        assert!(reused_membership.active);

        let members = list_classroom_memberships(&conn, classroom.id, true).unwrap();
        assert_eq!(members.len(), 3);

        set_membership_active(&conn, classroom.id, student.id, false).unwrap();
        let members = list_classroom_memberships(&conn, classroom.id, true).unwrap();
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn test_template_management() {
        let mut conn = in_memory_conn();
        init_db(&conn).unwrap();

        let teacher_id = add_user(&conn, "Alice", sample_key()).unwrap();
        let classroom = create_classroom(&mut conn, "cs101", "CS 101", teacher_id).unwrap();

        let template = create_classroom_template(&conn, classroom.id, "assignment1").unwrap();
        assert_eq!(template.repo_name, "assignment1");

        let found = find_classroom_template(&conn, classroom.id, "assignment1").unwrap();
        assert!(found.is_some());

        let templates = list_classroom_templates(&conn, classroom.id).unwrap();
        assert_eq!(templates.len(), 1);
    }

    #[test]
    fn test_existing_user_joins_second_classroom() {
        let mut conn = in_memory_conn();
        init_db(&conn).unwrap();

        let teacher_id = add_user(&conn, "Alice", sample_key()).unwrap();
        let cs101 = create_classroom(&mut conn, "cs101", "CS 101", teacher_id).unwrap();
        let cs102 = create_classroom(&mut conn, "cs102", "CS 102", teacher_id).unwrap();

        let token101 = create_classroom_invite_token(&conn, cs101.id, Role::Student).unwrap();
        let token102 = create_classroom_invite_token(&conn, cs102.id, Role::Student).unwrap();

        // Student joins cs101 with a full name.
        let (student, membership) = consume_classroom_invite_token(
            &mut conn,
            &token101,
            Some("Bob Smith"),
            Some(sample_key_2()),
        )
        .unwrap();
        assert_eq!(student.name, "Bob Smith");
        assert_eq!(membership.classroom_id, cs101.id);

        // Same student joins cs102 using the same key with a different token.
        let (student2, membership2) = consume_classroom_invite_token(
            &mut conn,
            &token102,
            Some("Bob Smith"),
            Some(sample_key_2()),
        )
        .unwrap();
        assert_eq!(student2.id, student.id);
        assert_eq!(membership2.classroom_id, cs102.id);

        let user_memberships = list_user_memberships(&conn, student.id, false).unwrap();
        assert_eq!(user_memberships.len(), 2);
    }
}
