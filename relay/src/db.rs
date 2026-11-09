//! Accounts, sessions, computers and pairing codes in SQLite. Tokens and codes are stored
//! as SHA-256 hashes; passwords as Argon2id.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};

pub struct Db(Mutex<Connection>);

pub struct Device {
    pub id: String,
    pub name: String,
    pub last_seen: Option<i64>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn hash_password(pw: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .expect("argon2")
        .to_string()
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

impl Db {
    pub fn open(path: &str) -> Self {
        let c = Connection::open(path).expect("open database");
        c.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS users(
               id INTEGER PRIMARY KEY, email TEXT UNIQUE NOT NULL, pw_hash TEXT NOT NULL,
               created INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions(
               token_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               expires INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS devices(
               id TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               name TEXT NOT NULL, token_hash TEXT UNIQUE NOT NULL, created INTEGER NOT NULL,
               last_seen INTEGER);
             CREATE TABLE IF NOT EXISTS pairings(
               code_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               expires INTEGER NOT NULL);",
        )
        .expect("schema");
        Db(Mutex::new(c))
    }

    pub fn add_user(&self, email: &str, pw_hash: &str) -> Option<i64> {
        let c = self.0.lock();
        c.execute(
            "INSERT INTO users(email, pw_hash, created) VALUES(?,?,?)",
            params![email, pw_hash, now()],
        )
        .ok()
        .map(|_| c.last_insert_rowid())
    }

    pub fn user_by_email(&self, email: &str) -> Option<(i64, String)> {
        self.0
            .lock()
            .query_row("SELECT id, pw_hash FROM users WHERE email=?", [email], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .expect("query")
    }

    pub fn add_session(&self, token_hash: &str, user: i64, days: i64) {
        let c = self.0.lock();
        c.execute("DELETE FROM sessions WHERE expires < ?", [now()]).expect("prune");
        c.execute(
            "INSERT INTO sessions(token_hash, user_id, expires) VALUES(?,?,?)",
            params![token_hash, user, now() + days * 86400],
        )
        .expect("insert");
    }

    pub fn session_user(&self, token_hash: &str) -> Option<i64> {
        self.0
            .lock()
            .query_row(
                "SELECT user_id FROM sessions WHERE token_hash=? AND expires > ?",
                params![token_hash, now()],
                |r| r.get(0),
            )
            .optional()
            .expect("query")
    }

    pub fn drop_session(&self, token_hash: &str) {
        self.0
            .lock()
            .execute("DELETE FROM sessions WHERE token_hash=?", [token_hash])
            .expect("delete");
    }

    pub fn add_pairing(&self, code_hash: &str, user: i64, secs: i64) {
        let c = self.0.lock();
        c.execute("DELETE FROM pairings WHERE expires < ?", [now()]).expect("prune");
        c.execute(
            "INSERT INTO pairings(code_hash, user_id, expires) VALUES(?,?,?)",
            params![code_hash, user, now() + secs],
        )
        .expect("insert");
    }

    /// The account a code belongs to; a code works once.
    pub fn take_pairing(&self, code_hash: &str) -> Option<i64> {
        let c = self.0.lock();
        let user = c
            .query_row(
                "SELECT user_id FROM pairings WHERE code_hash=? AND expires > ?",
                params![code_hash, now()],
                |r| r.get(0),
            )
            .optional()
            .expect("query")?;
        c.execute("DELETE FROM pairings WHERE code_hash=?", [code_hash]).expect("delete");
        Some(user)
    }

    pub fn add_device(&self, id: &str, user: i64, name: &str, token_hash: &str) {
        self.0
            .lock()
            .execute(
                "INSERT INTO devices(id, user_id, name, token_hash, created) VALUES(?,?,?,?,?)",
                params![id, user, name, token_hash, now()],
            )
            .expect("insert");
    }

    pub fn devices(&self, user: i64) -> Vec<Device> {
        let c = self.0.lock();
        let mut q = c
            .prepare("SELECT id, name, last_seen FROM devices WHERE user_id=? ORDER BY name")
            .expect("prepare");
        q.query_map([user], |r| {
            Ok(Device { id: r.get(0)?, name: r.get(1)?, last_seen: r.get(2)? })
        })
        .expect("query")
        .filter_map(Result::ok)
        .collect()
    }

    pub fn owns(&self, user: i64, device: &str) -> bool {
        self.0
            .lock()
            .query_row(
                "SELECT 1 FROM devices WHERE id=? AND user_id=?",
                params![device, user],
                |_| Ok(()),
            )
            .optional()
            .expect("query")
            .is_some()
    }

    pub fn remove_device(&self, user: i64, device: &str) -> bool {
        self.0
            .lock()
            .execute("DELETE FROM devices WHERE id=? AND user_id=?", params![device, user])
            .expect("delete")
            > 0
    }

    pub fn device_by_token(&self, token_hash: &str) -> Option<String> {
        self.0
            .lock()
            .query_row("SELECT id FROM devices WHERE token_hash=?", [token_hash], |r| r.get(0))
            .optional()
            .expect("query")
    }

    pub fn touch_device(&self, device: &str) {
        self.0
            .lock()
            .execute("UPDATE devices SET last_seen=? WHERE id=?", params![now(), device])
            .expect("update");
    }
}
