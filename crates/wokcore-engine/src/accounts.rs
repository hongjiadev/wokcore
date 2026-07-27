use std::{
    array,
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
};

use wokcore_core::id::AccountId;

const ACCOUNT_SHARDS: usize = 64;
const MAX_ACCOUNTS: usize = 256;
const MAX_WEIGHT: u16 = 100;
const MAX_COOLDOWN_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AccountAuthentication {
    Forward,
    Oauth,
    ApiKey,
    Local,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountHealthPolicy {
    base_cooldown_ms: u64,
    max_cooldown_ms: u64,
}

impl AccountHealthPolicy {
    pub fn new(base_cooldown_ms: u64, max_cooldown_ms: u64) -> Result<Self, AccountStateError> {
        if base_cooldown_ms == 0
            || max_cooldown_ms < base_cooldown_ms
            || max_cooldown_ms > MAX_COOLDOWN_MS
        {
            return Err(AccountStateError::InvalidPolicy);
        }
        Ok(Self {
            base_cooldown_ms,
            max_cooldown_ms,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountCandidate<'a> {
    account_id: &'a AccountId,
    authentication: AccountAuthentication,
    weight: u16,
}

impl<'a> AccountCandidate<'a> {
    pub fn new(
        account_id: &'a AccountId,
        authentication: AccountAuthentication,
        weight: u16,
    ) -> Result<Self, AccountStateError> {
        if weight == 0 || weight > MAX_WEIGHT {
            return Err(AccountStateError::InvalidWeight);
        }
        Ok(Self {
            account_id,
            authentication,
            weight,
        })
    }

    pub fn account_id(self) -> &'a AccountId {
        self.account_id
    }

    pub const fn authentication(self) -> AccountAuthentication {
        self.authentication
    }

    pub const fn weight(self) -> u16 {
        self.weight
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountObservation {
    Success,
    RateLimited { retry_after_ms: Option<u64> },
    TemporaryFailure { retry_after_ms: Option<u64> },
    InvalidCredentials,
    InvalidRequest,
    PolicyRejected,
    Quota { remaining: u64, resets_at_ms: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountStatusKind {
    Healthy,
    CoolingDown,
    QuotaExhausted,
    Quarantined,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountStatus {
    kind: AccountStatusKind,
    consecutive_failures: u8,
    cooldown_until_ms: Option<u64>,
    quota_remaining: Option<u64>,
    quota_resets_at_ms: Option<u64>,
    selection_count: u64,
    last_selected_sequence: u64,
    updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountHealthSnapshot {
    pub account_id: AccountId,
    pub consecutive_failures: u8,
    pub cooldown_until_ms: Option<u64>,
    pub quarantined: bool,
    pub quota_remaining: Option<u64>,
    pub quota_resets_at_ms: Option<u64>,
    pub selection_count: u64,
    pub last_selected_sequence: u64,
    pub updated_at_ms: u64,
}

impl AccountStatus {
    pub const fn kind(self) -> AccountStatusKind {
        self.kind
    }

    pub const fn consecutive_failures(self) -> u8 {
        self.consecutive_failures
    }

    pub const fn cooldown_until_ms(self) -> Option<u64> {
        self.cooldown_until_ms
    }

    pub const fn quota_remaining(self) -> Option<u64> {
        self.quota_remaining
    }

    pub const fn quota_resets_at_ms(self) -> Option<u64> {
        self.quota_resets_at_ms
    }

    pub const fn selection_count(self) -> u64 {
        self.selection_count
    }

    pub const fn last_selected_sequence(self) -> u64 {
        self.last_selected_sequence
    }

    pub const fn updated_at_ms(self) -> u64 {
        self.updated_at_ms
    }
}

#[derive(Clone, Debug, Default)]
struct AccountState {
    consecutive_failures: u8,
    cooldown_until_ms: Option<u64>,
    quarantined: bool,
    quota_remaining: Option<u64>,
    quota_resets_at_ms: Option<u64>,
    selection_count: u64,
    last_selected_sequence: u64,
    updated_at_ms: u64,
}

impl AccountState {
    fn normalize(&mut self, now_ms: u64) {
        if self
            .cooldown_until_ms
            .is_some_and(|until_ms| until_ms <= now_ms)
        {
            self.cooldown_until_ms = None;
        }
        if self
            .quota_resets_at_ms
            .is_some_and(|resets_at_ms| resets_at_ms <= now_ms)
        {
            self.quota_remaining = None;
            self.quota_resets_at_ms = None;
        }
    }

    fn status(&self) -> AccountStatus {
        let kind = if self.quarantined {
            AccountStatusKind::Quarantined
        } else if self.cooldown_until_ms.is_some() {
            AccountStatusKind::CoolingDown
        } else if self.quota_remaining == Some(0) {
            AccountStatusKind::QuotaExhausted
        } else {
            AccountStatusKind::Healthy
        };
        AccountStatus {
            kind,
            consecutive_failures: self.consecutive_failures,
            cooldown_until_ms: self.cooldown_until_ms,
            quota_remaining: self.quota_remaining,
            quota_resets_at_ms: self.quota_resets_at_ms,
            selection_count: self.selection_count,
            last_selected_sequence: self.last_selected_sequence,
            updated_at_ms: self.updated_at_ms,
        }
    }

    fn is_eligible(&self) -> bool {
        !self.quarantined && self.cooldown_until_ms.is_none() && self.quota_remaining != Some(0)
    }
}

pub struct AccountHealthTable {
    policy: AccountHealthPolicy,
    shards: [Mutex<BTreeMap<AccountId, AccountState>>; ACCOUNT_SHARDS],
    selection_sequence: AtomicU64,
}

impl AccountHealthTable {
    pub fn new(
        policy: AccountHealthPolicy,
        accounts: &[AccountId],
    ) -> Result<Self, AccountStateError> {
        let unique = accounts.iter().collect::<BTreeSet<_>>();
        if unique.len() != accounts.len() {
            return Err(AccountStateError::DuplicateAccount);
        }
        if accounts.len() > MAX_ACCOUNTS {
            return Err(AccountStateError::TooManyAccounts);
        }
        let table = Self {
            policy,
            shards: array::from_fn(|_| Mutex::new(BTreeMap::new())),
            selection_sequence: AtomicU64::new(0),
        };
        for account in accounts {
            table
                .lock_shard(account)
                .insert(account.clone(), AccountState::default());
        }
        Ok(table)
    }

    pub fn restore(
        policy: AccountHealthPolicy,
        accounts: &[AccountId],
        snapshots: &[AccountHealthSnapshot],
        now_ms: u64,
    ) -> Result<Self, AccountStateError> {
        let table = Self::new(policy, accounts)?;
        let mut restored = BTreeSet::new();
        let mut maximum_sequence = 0_u64;
        for snapshot in snapshots {
            if !restored.insert(&snapshot.account_id) {
                return Err(AccountStateError::InvalidSnapshot);
            }
            if snapshot.consecutive_failures > 64
                || snapshot.selection_count > snapshot.last_selected_sequence
                || (snapshot.selection_count == 0 && snapshot.last_selected_sequence != 0)
                || snapshot.cooldown_until_ms.is_some() && snapshot.quarantined
                || snapshot.quota_remaining.is_some() != snapshot.quota_resets_at_ms.is_some()
            {
                return Err(AccountStateError::InvalidSnapshot);
            }
            let mut shard = table.lock_shard(&snapshot.account_id);
            let state = shard
                .get_mut(&snapshot.account_id)
                .ok_or(AccountStateError::UnknownAccount)?;
            *state = AccountState {
                consecutive_failures: snapshot.consecutive_failures,
                cooldown_until_ms: snapshot.cooldown_until_ms,
                quarantined: snapshot.quarantined,
                quota_remaining: snapshot.quota_remaining,
                quota_resets_at_ms: snapshot.quota_resets_at_ms,
                selection_count: snapshot.selection_count,
                last_selected_sequence: snapshot.last_selected_sequence,
                updated_at_ms: snapshot.updated_at_ms,
            };
            state.normalize(now_ms);
            maximum_sequence = maximum_sequence.max(snapshot.last_selected_sequence);
        }
        table
            .selection_sequence
            .store(maximum_sequence, AtomicOrdering::Release);
        Ok(table)
    }

    pub const fn shard_count(&self) -> usize {
        ACCOUNT_SHARDS
    }

    pub fn observe(
        &self,
        account_id: &AccountId,
        observation: AccountObservation,
        now_ms: u64,
    ) -> Result<(), AccountStateError> {
        let mut shard = self.lock_shard(account_id);
        let state = shard
            .get_mut(account_id)
            .ok_or(AccountStateError::UnknownAccount)?;
        state.normalize(now_ms);
        match observation {
            AccountObservation::Success => {
                state.consecutive_failures = 0;
                state.cooldown_until_ms = None;
                state.quarantined = false;
                state.quota_remaining = None;
                state.quota_resets_at_ms = None;
            }
            AccountObservation::RateLimited { retry_after_ms }
            | AccountObservation::TemporaryFailure { retry_after_ms } => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                let exponent = u32::from(state.consecutive_failures.saturating_sub(1)).min(63);
                let exponential = self
                    .policy
                    .base_cooldown_ms
                    .saturating_mul(1_u64.checked_shl(exponent).unwrap_or(u64::MAX))
                    .min(self.policy.max_cooldown_ms);
                let hinted = retry_after_ms
                    .unwrap_or_default()
                    .min(self.policy.max_cooldown_ms);
                let duration = exponential.max(hinted);
                state.cooldown_until_ms = Some(now_ms.saturating_add(duration));
            }
            AccountObservation::InvalidCredentials => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.cooldown_until_ms = None;
                state.quarantined = true;
            }
            AccountObservation::InvalidRequest | AccountObservation::PolicyRejected => {}
            AccountObservation::Quota {
                remaining,
                resets_at_ms,
            } => {
                if resets_at_ms <= now_ms {
                    state.quota_remaining = None;
                    state.quota_resets_at_ms = None;
                } else {
                    state.quota_remaining = Some(remaining);
                    state.quota_resets_at_ms = Some(resets_at_ms);
                }
            }
        }
        state.updated_at_ms = state.updated_at_ms.max(now_ms);
        Ok(())
    }

    pub fn status(
        &self,
        account_id: &AccountId,
        now_ms: u64,
    ) -> Result<AccountStatus, AccountStateError> {
        let mut shard = self.lock_shard(account_id);
        let state = shard
            .get_mut(account_id)
            .ok_or(AccountStateError::UnknownAccount)?;
        state.normalize(now_ms);
        Ok(state.status())
    }

    pub fn snapshots(&self, now_ms: u64) -> Vec<AccountHealthSnapshot> {
        let mut snapshots = Vec::new();
        for shard in &self.shards {
            let mut shard = shard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (account_id, state) in shard.iter_mut() {
                state.normalize(now_ms);
                snapshots.push(AccountHealthSnapshot {
                    account_id: account_id.clone(),
                    consecutive_failures: state.consecutive_failures,
                    cooldown_until_ms: state.cooldown_until_ms,
                    quarantined: state.quarantined,
                    quota_remaining: state.quota_remaining,
                    quota_resets_at_ms: state.quota_resets_at_ms,
                    selection_count: state.selection_count,
                    last_selected_sequence: state.last_selected_sequence,
                    updated_at_ms: state.updated_at_ms,
                });
            }
        }
        snapshots.sort_by(|left, right| left.account_id.cmp(&right.account_id));
        snapshots
    }

    pub fn select<'a>(
        &self,
        candidates: &'a [AccountCandidate<'a>],
        authentication: AccountAuthentication,
        affinity_account: Option<&AccountId>,
        now_ms: u64,
    ) -> Result<AccountChoice<'a>, SelectionError> {
        if let Some(account_id) = affinity_account
            && let Some(candidate) = candidates.iter().copied().find(|candidate| {
                candidate.authentication == authentication && candidate.account_id == account_id
            })
            && self.eligible(candidate.account_id, now_ms)?
        {
            self.mark_selected(candidate.account_id, now_ms)?;
            return Ok(AccountChoice {
                account_id: candidate.account_id,
                origin: AccountChoiceOrigin::Affinity,
            });
        }

        let mut best: Option<(AccountCandidate<'a>, AccountStatus)> = None;
        for candidate in candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.authentication == authentication)
        {
            let status = self.status(candidate.account_id, now_ms)?;
            if status.kind != AccountStatusKind::Healthy {
                continue;
            }
            let replace = best.as_ref().is_none_or(|(current, current_status)| {
                compare_weighted_usage(candidate, status, *current, *current_status)
                    == Ordering::Less
            });
            if replace {
                best = Some((candidate, status));
            }
        }
        let (candidate, _) = best.ok_or(SelectionError::NoEligibleAccount)?;
        self.mark_selected(candidate.account_id, now_ms)?;
        Ok(AccountChoice {
            account_id: candidate.account_id,
            origin: AccountChoiceOrigin::WeightedLeastRecentlyUsed,
        })
    }

    fn eligible(&self, account_id: &AccountId, now_ms: u64) -> Result<bool, SelectionError> {
        let mut shard = self.lock_shard(account_id);
        let state = shard
            .get_mut(account_id)
            .ok_or(AccountStateError::UnknownAccount)?;
        state.normalize(now_ms);
        Ok(state.is_eligible())
    }

    fn mark_selected(&self, account_id: &AccountId, now_ms: u64) -> Result<(), SelectionError> {
        let sequence = self
            .selection_sequence
            .fetch_add(1, AtomicOrdering::AcqRel)
            .saturating_add(1);
        let mut shard = self.lock_shard(account_id);
        let state = shard
            .get_mut(account_id)
            .ok_or(AccountStateError::UnknownAccount)?;
        state.selection_count = state.selection_count.saturating_add(1);
        state.last_selected_sequence = sequence;
        state.updated_at_ms = state.updated_at_ms.max(now_ms);
        Ok(())
    }

    fn lock_shard(
        &self,
        account_id: &AccountId,
    ) -> std::sync::MutexGuard<'_, BTreeMap<AccountId, AccountState>> {
        self.shards[account_shard(account_id)]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn compare_weighted_usage(
    left: AccountCandidate<'_>,
    left_status: AccountStatus,
    right: AccountCandidate<'_>,
    right_status: AccountStatus,
) -> Ordering {
    let left_score = u128::from(left_status.selection_count) * u128::from(right.weight);
    let right_score = u128::from(right_status.selection_count) * u128::from(left.weight);
    left_score.cmp(&right_score).then_with(|| {
        left_status
            .last_selected_sequence
            .cmp(&right_status.last_selected_sequence)
    })
}

fn account_shard(account_id: &AccountId) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in account_id.as_str().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash as usize) % ACCOUNT_SHARDS
}

impl std::fmt::Debug for AccountHealthTable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountHealthTable")
            .field("shard_count", &ACCOUNT_SHARDS)
            .field("maximum_accounts", &MAX_ACCOUNTS)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountChoiceOrigin {
    Affinity,
    WeightedLeastRecentlyUsed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountChoice<'a> {
    account_id: &'a AccountId,
    origin: AccountChoiceOrigin,
}

impl<'a> AccountChoice<'a> {
    pub fn account_id(self) -> &'a AccountId {
        self.account_id
    }

    pub const fn origin(self) -> AccountChoiceOrigin {
        self.origin
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AccountStateError {
    #[error("the account health policy is invalid")]
    InvalidPolicy,
    #[error("the account weight is invalid")]
    InvalidWeight,
    #[error("the account set contains a duplicate")]
    DuplicateAccount,
    #[error("the account set exceeds its bound")]
    TooManyAccounts,
    #[error("the account is not registered")]
    UnknownAccount,
    #[error("the restored account snapshot is invalid")]
    InvalidSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SelectionError {
    #[error(transparent)]
    AccountState(#[from] AccountStateError),
    #[error("no eligible account is available")]
    NoEligibleAccount,
}
