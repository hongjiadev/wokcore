use std::{
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use fs4::fs_std::FileExt;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params};
use wokcore_core::{id::ClientId, secret::SecretRef};

use crate::StorageError;

use super::wal;

const INITIAL_MIGRATION: &str = include_str!("../../migrations/0001_initial.sql");
const RUNTIME_AUTH_MIGRATION: &str = include_str!("../../migrations/0002_runtime_auth.sql");
const LATEST_SCHEMA_VERSION: i64 = 2;

pub const WAL_CHECKPOINT_THRESHOLD_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestMetric {
    pub request_id: String,
    pub provider_id: String,
    pub model: String,
    pub started_at: String,
    pub latency_ms: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub status_code: i64,
    pub error_code: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateHealth {
    pub schema_version: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointResult {
    pub busy: bool,
    pub log_frames: i64,
    pub checkpointed_frames: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSecretBinding {
    pub name: String,
    pub secret_ref: SecretRef,
    pub revision: u64,
    pub created_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientTokenMetadata {
    pub token_id: String,
    pub client_id: ClientId,
    pub digest: [u8; 32],
    pub issued_at: String,
}

#[derive(Debug)]
pub struct StateStore {
    connection: Connection,
    database_path: PathBuf,
}

pub struct ReadOnlyStateStore {
    connection: Connection,
}

#[derive(Clone, Copy)]
enum ReadOnlyAccess {
    Offline,
    Live,
}

impl ReadOnlyStateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_path(path.as_ref(), ReadOnlyAccess::Offline)
    }

    pub fn open_live(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_path(path.as_ref(), ReadOnlyAccess::Live)
    }

    fn open_path(path: &Path, access: ReadOnlyAccess) -> Result<Self, StorageError> {
        let absolute = path
            .canonicalize()
            .map_err(|source| StorageError::Io { source })?;
        let wal_path = sqlite_sidecar_path(&absolute, "-wal");
        let has_wal = match fs::metadata(&wal_path) {
            Ok(metadata) => metadata.len() > 0,
            Err(source) if source.kind() == io::ErrorKind::NotFound => false,
            Err(source) => return Err(StorageError::Io { source }),
        };
        if has_wal && matches!(access, ReadOnlyAccess::Offline) {
            return Self::from_connection(wal::open_replayed(&absolute, &wal_path)?);
        }
        Self::open_uri(read_only_database_uri(&absolute, has_wal)?)
    }

    fn open_uri(uri: String) -> Result<Self, StorageError> {
        let connection = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_database_error)?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, StorageError> {
        connection
            .execute_batch("PRAGMA query_only = ON;")
            .map_err(map_database_error)?;
        let store = Self { connection };
        store.validate()?;
        Ok(store)
    }

    pub fn health(&self) -> Result<StateHealth, StorageError> {
        let versions = schema_versions(&self.connection)?;
        if versions != [1, LATEST_SCHEMA_VERSION] {
            return Err(StorageError::StateDatabaseCorrupt {
                message: "state database has an incompatible migration history".to_owned(),
            });
        }
        Ok(StateHealth {
            schema_version: LATEST_SCHEMA_VERSION,
        })
    }

    pub fn runtime_secret_binding(
        &self,
        name: &str,
    ) -> Result<Option<RuntimeSecretBinding>, StorageError> {
        query_runtime_secret_binding(&self.connection, name)
    }

    fn validate(&self) -> Result<(), StorageError> {
        let quick_check = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .map_err(map_database_error)?;
        if quick_check != "ok" {
            return Err(StorageError::StateDatabaseCorrupt {
                message: "state database failed read-only integrity inspection".to_owned(),
            });
        }
        self.health().map(|_| ())
    }
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let setup_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(Self::setup_lock_path(path))
            .map_err(|source| StorageError::Io { source })?;
        setup_lock
            .lock_exclusive()
            .map_err(|source| StorageError::Io { source })?;
        let result = Self::open_locked(path);
        let unlock_result =
            FileExt::unlock(&setup_lock).map_err(|source| StorageError::Io { source });

        match (result, unlock_result) {
            (Ok(store), Ok(())) => Ok(store),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn open_locked(path: &Path) -> Result<Self, StorageError> {
        let mut connection = Connection::open(path).map_err(map_database_error)?;
        connection
            .execute_batch(
                "PRAGMA busy_timeout = 5000;\
                 PRAGMA foreign_keys = ON;\
                 PRAGMA journal_mode = WAL;\
                 PRAGMA wal_autocheckpoint = 0;",
            )
            .map_err(map_database_error)?;

        apply_ordered_migrations(&mut connection)?;

        Ok(Self {
            connection,
            database_path: path.to_path_buf(),
        })
    }

    fn setup_lock_path(path: &Path) -> PathBuf {
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        lock_path.into()
    }

    fn wal_path(&self) -> PathBuf {
        let mut wal_path = self.database_path.as_os_str().to_os_string();
        wal_path.push("-wal");
        wal_path.into()
    }

    pub fn health(&self) -> Result<StateHealth, StorageError> {
        let schema_version = self
            .connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .map_err(map_database_error)?
            .unwrap_or_default();
        Ok(StateHealth { schema_version })
    }

    pub fn record_request_metrics(
        &mut self,
        metrics: &[RequestMetric],
    ) -> Result<(), StorageError> {
        if metrics.is_empty() {
            return Ok(());
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO request_metrics (request_id, provider_id, model, started_at, latency_ms, input_tokens, output_tokens, status_code, error_code) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(map_database_error)?;
            for metric in metrics {
                statement
                    .execute(params![
                        metric.request_id,
                        metric.provider_id,
                        metric.model,
                        metric.started_at,
                        metric.latency_ms,
                        metric.input_tokens,
                        metric.output_tokens,
                        metric.status_code,
                        metric.error_code,
                    ])
                    .map_err(map_database_error)?;
            }
        }
        transaction.commit().map_err(map_database_error)?;
        Ok(())
    }

    pub fn record_orphan_secret(
        &self,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<(), StorageError> {
        self.connection
            .execute(
                "INSERT INTO orphan_secrets (secret_ref, created_at) VALUES (?1, ?2) ON CONFLICT(secret_ref) DO UPDATE SET created_at = excluded.created_at",
                params![secret_ref.as_str(), created_at],
            )
            .map_err(map_database_error)?;
        Ok(())
    }

    pub fn runtime_secret_binding(
        &self,
        name: &str,
    ) -> Result<Option<RuntimeSecretBinding>, StorageError> {
        query_runtime_secret_binding(&self.connection, name)
    }

    pub fn bind_runtime_secret_if_absent(
        &mut self,
        name: &str,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<RuntimeSecretBinding, StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let existing_revision = transaction
            .query_row(
                "SELECT revision FROM runtime_secret_bindings WHERE binding_name = ?1",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_database_error)?;
        if let Some(existing_revision) = existing_revision {
            let actual = u64::try_from(existing_revision).map_err(|_| {
                StorageError::StateDatabaseCorrupt {
                    message: "runtime secret binding contains an invalid revision".to_owned(),
                }
            })?;
            return Err(StorageError::RuntimeSecretBindingConflict { actual });
        }

        transaction
            .execute(
                "INSERT INTO runtime_secret_bindings(binding_name, secret_ref, revision, created_at)
                 VALUES (?1, ?2, 1, ?3)",
                params![name, secret_ref.as_str(), created_at],
            )
            .map_err(map_database_error)?;
        transaction.commit().map_err(map_database_error)?;

        Ok(RuntimeSecretBinding {
            name: name.to_owned(),
            secret_ref: secret_ref.clone(),
            revision: 1,
            created_at: created_at.to_owned(),
        })
    }

    pub fn issue_client_token(&mut self, token: &ClientTokenMetadata) -> Result<(), StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        transaction
            .execute(
                "INSERT INTO client_tokens(token_id, client_id, token_digest, issued_at, revoked_at)
                 VALUES (?1, ?2, ?3, ?4, NULL)",
                params![
                    token.token_id,
                    token.client_id.as_str(),
                    token.digest.as_slice(),
                    token.issued_at,
                ],
            )
            .map_err(map_database_error)?;
        transaction.commit().map_err(map_database_error)?;
        Ok(())
    }

    pub fn revoke_client_token(
        &mut self,
        client_id: &ClientId,
        token_id: &str,
        revoked_at: &str,
    ) -> Result<bool, StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let changed = transaction
            .execute(
                "UPDATE client_tokens
                 SET revoked_at = ?3
                 WHERE token_id = ?1 AND client_id = ?2 AND revoked_at IS NULL",
                params![token_id, client_id.as_str(), revoked_at],
            )
            .map_err(map_database_error)?
            != 0;
        transaction.commit().map_err(map_database_error)?;
        Ok(changed)
    }

    pub fn load_active_client_tokens(&self) -> Result<Vec<ClientTokenMetadata>, StorageError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT token_id, client_id, token_digest, issued_at
                 FROM client_tokens
                 WHERE revoked_at IS NULL
                 ORDER BY token_id",
            )
            .map_err(map_database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;

        rows.into_iter()
            .map(|(token_id, client_id, digest, issued_at)| {
                let client_id =
                    ClientId::new(client_id).map_err(|_| StorageError::StateDatabaseCorrupt {
                        message: "client token metadata contains an invalid client identifier"
                            .to_owned(),
                    })?;
                let digest = digest
                    .try_into()
                    .map_err(|_| StorageError::StateDatabaseCorrupt {
                        message: "client token metadata contains an invalid digest".to_owned(),
                    })?;
                Ok(ClientTokenMetadata {
                    token_id,
                    client_id,
                    digest,
                    issued_at,
                })
            })
            .collect()
    }

    pub fn orphan_secret_refs(&self) -> Result<Vec<SecretRef>, StorageError> {
        let mut statement = self
            .connection
            .prepare("SELECT secret_ref FROM orphan_secrets ORDER BY secret_ref")
            .map_err(map_database_error)?;
        let references = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;

        references
            .into_iter()
            .map(|secret_ref| {
                SecretRef::parse(secret_ref).map_err(|_| StorageError::StateDatabaseCorrupt {
                    message: "orphan secret metadata contains an invalid reference".to_owned(),
                })
            })
            .collect()
    }

    pub fn wal_size_bytes(&self) -> Result<u64, StorageError> {
        match fs::metadata(self.wal_path()) {
            Ok(metadata) => Ok(metadata.len()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(source) => Err(StorageError::Io { source }),
        }
    }

    pub fn checkpoint_passive_if_at_least(
        &self,
        threshold_bytes: u64,
    ) -> Result<Option<CheckpointResult>, StorageError> {
        if self.wal_size_bytes()? < threshold_bytes {
            return Ok(None);
        }

        self.checkpoint("PRAGMA wal_checkpoint(PASSIVE)").map(Some)
    }

    pub fn checkpoint_truncate(&self) -> Result<CheckpointResult, StorageError> {
        self.checkpoint("PRAGMA wal_checkpoint(TRUNCATE)")
    }

    fn checkpoint(&self, pragma: &str) -> Result<CheckpointResult, StorageError> {
        self.connection
            .query_row(pragma, [], |row| {
                Ok(CheckpointResult {
                    busy: row.get::<_, i64>(0)? != 0,
                    log_frames: row.get(1)?,
                    checkpointed_frames: row.get(2)?,
                })
            })
            .map_err(map_database_error)
    }

    pub fn pragma_journal_mode(&self) -> Result<String, StorageError> {
        self.pragma_value("journal_mode")
    }

    pub fn pragma_foreign_keys(&self) -> Result<i64, StorageError> {
        self.pragma_value("foreign_keys")
    }

    pub fn pragma_busy_timeout(&self) -> Result<i64, StorageError> {
        self.pragma_value("busy_timeout")
    }

    pub fn pragma_wal_autocheckpoint(&self) -> Result<i64, StorageError> {
        self.pragma_value("wal_autocheckpoint")
    }

    fn pragma_value<T>(&self, name: &str) -> Result<T, StorageError>
    where
        T: rusqlite::types::FromSql,
    {
        self.connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .map_err(map_database_error)
    }
}

fn query_runtime_secret_binding(
    connection: &Connection,
    name: &str,
) -> Result<Option<RuntimeSecretBinding>, StorageError> {
    let result = connection.query_row(
        "SELECT binding_name, secret_ref, revision, created_at
             FROM runtime_secret_bindings
             WHERE binding_name = ?1",
        [name],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    );
    let (name, secret_ref, revision, created_at) = match result {
        Ok(binding) => binding,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(map_database_error(error)),
    };
    let secret_ref =
        SecretRef::parse(secret_ref).map_err(|_| StorageError::StateDatabaseCorrupt {
            message: "runtime secret binding contains an invalid reference".to_owned(),
        })?;
    let revision = u64::try_from(revision).map_err(|_| StorageError::StateDatabaseCorrupt {
        message: "runtime secret binding contains an invalid revision".to_owned(),
    })?;

    Ok(Some(RuntimeSecretBinding {
        name,
        secret_ref,
        revision,
        created_at,
    }))
}

fn read_only_database_uri(path: &Path, has_wal: bool) -> Result<String, StorageError> {
    let value = path
        .to_str()
        .ok_or_else(|| StorageError::Io {
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "state database path is not valid UTF-8",
            ),
        })?
        .replace('\\', "/");
    #[cfg(windows)]
    let value = value.strip_prefix("//?/").unwrap_or(&value);
    #[cfg(not(windows))]
    let value = value.as_str();
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    #[cfg(windows)]
    if !encoded.starts_with('/') {
        encoded.insert(0, '/');
    }
    let options = match has_wal {
        false => "mode=ro&immutable=1",
        true => "mode=ro&readonly_shm=1",
    };
    Ok(format!("file:{encoded}?{options}"))
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    sidecar.into()
}

fn apply_ordered_migrations(connection: &mut Connection) -> Result<(), StorageError> {
    let versions = schema_versions(connection)?;
    if versions
        .iter()
        .enumerate()
        .any(|(index, version)| *version != (index as i64) + 1)
        || versions
            .last()
            .is_some_and(|version| *version > LATEST_SCHEMA_VERSION)
    {
        return Err(StorageError::StateDatabaseCorrupt {
            message: "state database has an incompatible migration history".to_owned(),
        });
    }

    for (version, migration) in [(1, INITIAL_MIGRATION), (2, RUNTIME_AUTH_MIGRATION)] {
        if versions.contains(&version) {
            continue;
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        transaction
            .execute_batch(migration)
            .map_err(map_database_error)?;
        transaction.commit().map_err(map_database_error)?;
    }
    Ok(())
}

fn schema_versions(connection: &Connection) -> Result<Vec<i64>, StorageError> {
    let has_migration_table = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'schema_migrations'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(map_database_error)?;
    if !has_migration_table {
        let existing_tables = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_database_error)?;
        if existing_tables != 0 {
            return Err(StorageError::StateDatabaseCorrupt {
                message: "state database has tables without migration metadata".to_owned(),
            });
        }
        return Ok(Vec::new());
    }

    let mut statement = connection
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .map_err(map_database_error)?;
    statement
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(map_database_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_database_error)
}

fn map_database_error(error: rusqlite::Error) -> StorageError {
    match &error {
        rusqlite::Error::SqliteFailure(sqlite_error, _)
            if matches!(
                sqlite_error.code,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
            ) =>
        {
            StorageError::StateDatabaseCorrupt {
                message: error.to_string(),
            }
        }
        _ => StorageError::StateDatabase { source: error },
    }
}
