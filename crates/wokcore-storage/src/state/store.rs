use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use fs4::fs_std::FileExt;
use rusqlite::{Connection, ErrorCode, TransactionBehavior, params};
use wokcore_core::secret::SecretRef;

use crate::StorageError;

const INITIAL_MIGRATION: &str = include_str!("../../migrations/0001_initial.sql");

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

#[derive(Debug)]
pub struct StateStore {
    connection: Connection,
    database_path: PathBuf,
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

        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
            )
            .map_err(map_database_error)?;
        let schema_version = transaction
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .map_err(map_database_error)?
            .unwrap_or_default();
        if schema_version < 1 {
            transaction
                .execute_batch(INITIAL_MIGRATION)
                .map_err(map_database_error)?;
        }
        transaction.commit().map_err(map_database_error)?;

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
