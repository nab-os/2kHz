//! Who is allowed to do what.
//!
//! One user, several devices, so per-device tokens rather than a password,
//! a password could not be revoked one phone at a time.
//!
//! `play` is every device; `pipeline` is hours of CPU, the shared rate limit
//! and a `--purge` that deletes rows. `play` still needs a token: a stream URL
//! is minted against the user's account, so handing those out is sharing it.

use anyhow::{Context, Result};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use qsuggest::api::{Device, PairingGrant, Scope};
use rand::Rng;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// 32 bytes, hex-encoded. Long enough that guessing is not a threat model.
const TOKEN_BYTES: usize = 32;

pub struct AuthStore {
    db_path: PathBuf,
}

impl AuthStore {
    pub fn new(db_path: &Path) -> Result<Self> {
        let store = Self {
            db_path: db_path.to_path_buf(),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    fn open(&self) -> Result<Connection> {
        Connection::open(&self.db_path)
            .with_context(|| format!("opening {}", self.db_path.display()))
    }

    /// Devices live in the catalogue database, so one file is still the whole
    /// backup. Created here rather than in the shared `schema.sql` because the
    /// pipeline has no business knowing which phones are paired.
    fn ensure_schema(&self) -> Result<()> {
        self.open()?.execute_batch(
            "CREATE TABLE IF NOT EXISTS devices (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 name       TEXT NOT NULL,
                 scope      TEXT NOT NULL,
                 token_hash TEXT NOT NULL UNIQUE,
                 created_at TEXT NOT NULL,
                 last_seen  TEXT
             );
             CREATE INDEX IF NOT EXISTS devices_token ON devices(token_hash);",
        )?;
        Ok(())
    }

    /// Mint a token for a new device. Returned once and never again, only
    /// the hash is kept, so a lost token means re-pairing.
    pub fn issue(&self, name: &str, scope: Scope) -> Result<PairingGrant> {
        let mut raw = [0u8; TOKEN_BYTES];
        rand::rng().fill(&mut raw);
        let token = hex(&raw);
        let now = qsuggest::db::utc_now();

        let conn = self.open()?;
        conn.execute(
            "INSERT INTO devices (name, scope, token_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![name, scope.as_str(), hash(&token), now],
        )?;

        Ok(PairingGrant {
            device: Device {
                id: conn.last_insert_rowid(),
                name: name.to_string(),
                scope,
                created_at: now,
                last_seen: None,
            },
            token,
        })
    }

    /// Resolve a presented token, and record that it was used.
    ///
    /// Looked up by hash, so the comparison is SQLite's and not constant-time.
    /// Fine for a 256-bit random token, which cannot be probed a byte at a
    /// time; it would not be for a password.
    pub fn verify(&self, token: &str) -> Option<Device> {
        let conn = self.open().ok()?;
        let digest = hash(token);

        let device = conn
            .query_row(
                "SELECT id, name, scope, created_at, last_seen
                 FROM devices WHERE token_hash = ?1",
                [&digest],
                |row| {
                    Ok(Device {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        scope: Scope::parse(&row.get::<_, String>(2)?).unwrap_or(Scope::Play),
                        created_at: row.get(3)?,
                        last_seen: row.get(4)?,
                    })
                },
            )
            .ok()?;

        let _ = conn.execute(
            "UPDATE devices SET last_seen = ?1 WHERE id = ?2",
            rusqlite::params![qsuggest::db::utc_now(), device.id],
        );

        Some(device)
    }

    pub fn list(&self) -> Result<Vec<Device>> {
        let conn = self.open()?;
        let mut statement = conn.prepare(
            "SELECT id, name, scope, created_at, last_seen FROM devices ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Device {
                id: row.get(0)?,
                name: row.get(1)?,
                scope: Scope::parse(&row.get::<_, String>(2)?).unwrap_or(Scope::Play),
                created_at: row.get(3)?,
                last_seen: row.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn revoke(&self, device_id: i64) -> Result<()> {
        let removed = self
            .open()?
            .execute("DELETE FROM devices WHERE id = ?1", [device_id])?;
        if removed == 0 {
            anyhow::bail!("no device with id {device_id}");
        }
        Ok(())
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self
            .open()?
            .query_row("SELECT COUNT(*) FROM devices", [], |row| row.get(0))?)
    }
}

fn hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ------------------------------------------------------------- extractors

/// A request that carried a valid token. Every route takes one of these or
/// `PipelineAuth`, so there is no unauthed path by construction.
pub struct PlayAuth(#[allow(dead_code)] pub Device);

/// A request from a device trusted with the expensive, destructive half.
pub struct PipelineAuth(#[allow(dead_code)] pub Device);

fn bearer(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|token| token.trim().to_string())
}

fn authorise(parts: &Parts, state: &crate::AppState, needed: Scope) -> Result<Device, crate::Failure> {
    let Some(token) = bearer(parts) else {
        return Err(crate::Failure::unauthorised(
            "no bearer token; pair this device with `qsuggest-server pair`",
        ));
    };
    let Some(device) = state.auth.verify(&token) else {
        return Err(crate::Failure::unauthorised(
            "unrecognised token; it may have been revoked",
        ));
    };
    if !device.scope.covers(needed) {
        return Err(crate::Failure::forbidden(format!(
            "device “{}” is paired for {} only; this needs {}",
            device.name,
            device.scope.as_str(),
            needed.as_str()
        )));
    }
    Ok(device)
}

impl FromRequestParts<crate::AppState> for PlayAuth {
    type Rejection = crate::Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        authorise(parts, state, Scope::Play).map(PlayAuth)
    }
}

impl FromRequestParts<crate::AppState> for PipelineAuth {
    type Rejection = crate::Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        authorise(parts, state, Scope::Pipeline).map(PipelineAuth)
    }
}
