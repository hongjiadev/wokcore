use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use sha2::{Digest, Sha256};
use wokcore_core::id::AccountId;

const MAX_AFFINITIES: usize = 4_096;
const MAX_AFFINITY_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_AFFINITY_SHARDS: usize = 16;

#[derive(Clone, Debug)]
struct AffinityEntry {
    account_id: Arc<AccountId>,
    expires_at_ms: u64,
    touched_sequence: u64,
}

#[derive(Debug)]
struct AffinityShard {
    capacity: usize,
    entries: BTreeMap<[u8; 32], AffinityEntry>,
}

pub struct AffinityTable {
    hash_key: [u8; 32],
    ttl_ms: u64,
    maximum_entries: usize,
    shards: Vec<Mutex<AffinityShard>>,
    touch_sequence: AtomicU64,
}

impl AffinityTable {
    pub fn new(
        hash_key: [u8; 32],
        maximum_entries: usize,
        ttl_ms: u64,
    ) -> Result<Self, AffinityError> {
        if maximum_entries == 0
            || maximum_entries > MAX_AFFINITIES
            || ttl_ms == 0
            || ttl_ms > MAX_AFFINITY_TTL_MS
        {
            return Err(AffinityError::InvalidPolicy);
        }
        let shard_count = maximum_entries.min(MAX_AFFINITY_SHARDS);
        let base_capacity = maximum_entries / shard_count;
        let extra = maximum_entries % shard_count;
        let shards = (0..shard_count)
            .map(|index| {
                Mutex::new(AffinityShard {
                    capacity: base_capacity + usize::from(index < extra),
                    entries: BTreeMap::new(),
                })
            })
            .collect();
        Ok(Self {
            hash_key,
            ttl_ms,
            maximum_entries,
            shards,
            touch_sequence: AtomicU64::new(0),
        })
    }

    pub fn bind(&self, thread_key: &[u8], account_id: &AccountId, now_ms: u64) {
        let digest = self.digest(thread_key);
        let sequence = self
            .touch_sequence
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let mut shard = self.lock_shard(&digest);
        shard
            .entries
            .retain(|_, entry| entry.expires_at_ms > now_ms);
        if let Some(entry) = shard.entries.get_mut(&digest) {
            if entry.account_id.as_ref() != account_id {
                entry.account_id = Arc::new(account_id.clone());
            }
            entry.expires_at_ms = now_ms.saturating_add(self.ttl_ms);
            entry.touched_sequence = sequence;
            return;
        }
        if shard.entries.len() >= shard.capacity {
            let evicted = shard
                .entries
                .iter()
                .min_by_key(|(digest, entry)| {
                    (entry.expires_at_ms, entry.touched_sequence, **digest)
                })
                .map(|(digest, _)| *digest);
            if let Some(evicted) = evicted {
                shard.entries.remove(&evicted);
            }
        }
        shard.entries.insert(
            digest,
            AffinityEntry {
                account_id: Arc::new(account_id.clone()),
                expires_at_ms: now_ms.saturating_add(self.ttl_ms),
                touched_sequence: sequence,
            },
        );
    }

    pub fn lookup(&self, thread_key: &[u8], now_ms: u64) -> Option<Arc<AccountId>> {
        let digest = self.digest(thread_key);
        let mut shard = self.lock_shard(&digest);
        let expired = shard
            .entries
            .get(&digest)
            .is_some_and(|entry| entry.expires_at_ms <= now_ms);
        if expired {
            shard.entries.remove(&digest);
            return None;
        }
        shard.entries.get_mut(&digest).map(|entry| {
            entry.touched_sequence = self
                .touch_sequence
                .fetch_add(1, Ordering::AcqRel)
                .saturating_add(1);
            entry.account_id.clone()
        })
    }

    pub fn len(&self, now_ms: u64) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                let mut shard = shard
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                shard
                    .entries
                    .retain(|_, entry| entry.expires_at_ms > now_ms);
                shard.entries.len()
            })
            .sum()
    }

    fn digest(&self, thread_key: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"wokcore.affinity.v1\0");
        hasher.update(self.hash_key);
        hasher.update((thread_key.len() as u64).to_le_bytes());
        hasher.update(thread_key);
        hasher.finalize().into()
    }

    fn lock_shard(&self, digest: &[u8; 32]) -> std::sync::MutexGuard<'_, AffinityShard> {
        let index = usize::from(digest[0]) % self.shards.len();
        self.shards[index]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl fmt::Debug for AffinityTable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AffinityTable")
            .field("maximum_entries", &self.maximum_entries)
            .field("ttl_ms", &self.ttl_ms)
            .field("shard_count", &self.shards.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AffinityError {
    #[error("the affinity policy is invalid")]
    InvalidPolicy,
}
