use std::{
    collections::BTreeMap,
    sync::{Arc, Barrier},
    thread,
};

use wokcore_core::id::AccountId;
use wokcore_engine::{
    accounts::{
        AccountAuthentication, AccountCandidate, AccountChoiceOrigin, AccountHealthPolicy,
        AccountHealthSnapshot, AccountHealthTable, AccountObservation, AccountStateError,
        AccountStatusKind, SelectionError,
    },
    affinity::AffinityTable,
};

#[test]
fn weighted_lru_honors_weights_without_a_request_semaphore() {
    let light = account("light");
    let heavy = account("heavy");
    let table =
        AccountHealthTable::new(policy(), &[light.clone(), heavy.clone()]).expect("health table");
    let candidates = [
        candidate(&light, AccountAuthentication::ApiKey, 1),
        candidate(&heavy, AccountAuthentication::ApiKey, 3),
    ];
    let mut counts = BTreeMap::new();

    for now_ms in 1..=40 {
        let choice = table
            .select(&candidates, AccountAuthentication::ApiKey, None, now_ms)
            .expect("choice");
        *counts.entry(choice.account_id().as_str()).or_insert(0_u32) += 1;
        assert_eq!(
            choice.origin(),
            AccountChoiceOrigin::WeightedLeastRecentlyUsed
        );
    }

    assert_eq!(counts.get("light"), Some(&10));
    assert_eq!(counts.get("heavy"), Some(&30));
    assert_eq!(table.shard_count(), 64);
}

#[test]
fn affinity_uses_only_a_bounded_expiring_digest() {
    let table = AffinityTable::new([7; 32], 16, 1_000).expect("affinity table");
    let first = account("first");
    table.bind(b"raw-thread-key", &first, 100);

    assert_eq!(
        table.lookup(b"raw-thread-key", 1_099).as_deref(),
        Some(&first)
    );
    let before_rebind = table.lookup(b"raw-thread-key", 1_099).expect("affinity");
    table.bind(b"raw-thread-key", &first, 1_099);
    let after_rebind = table.lookup(b"raw-thread-key", 1_099).expect("affinity");
    assert!(Arc::ptr_eq(&before_rebind, &after_rebind));
    assert_eq!(table.lookup(b"raw-thread-key", 2_099), None);
    assert!(!format!("{table:?}").contains("raw-thread-key"));

    for index in 0..100_u32 {
        let key = format!("thread-{index}");
        table.bind(key.as_bytes(), &account(&format!("account-{index}")), 2_000);
    }
    assert!(table.len(2_000) <= 16);
}

#[test]
fn affinity_wins_only_when_the_bound_account_is_still_eligible() {
    let first = account("first");
    let second = account("second");
    let table =
        AccountHealthTable::new(policy(), &[first.clone(), second.clone()]).expect("health table");
    let candidates = [
        candidate(&first, AccountAuthentication::ApiKey, 1),
        candidate(&second, AccountAuthentication::ApiKey, 1),
    ];

    let choice = table
        .select(
            &candidates,
            AccountAuthentication::ApiKey,
            Some(&second),
            10,
        )
        .expect("affinity choice");
    assert_eq!(choice.account_id(), &second);
    assert_eq!(choice.origin(), AccountChoiceOrigin::Affinity);

    table
        .observe(&second, AccountObservation::InvalidCredentials, 11)
        .expect("observe");
    let fallback = table
        .select(
            &candidates,
            AccountAuthentication::ApiKey,
            Some(&second),
            12,
        )
        .expect("fallback");
    assert_eq!(fallback.account_id(), &first);
}

#[test]
fn cooldown_is_exponential_bounded_and_honors_a_bounded_server_hint() {
    let id = account("limited");
    let table = AccountHealthTable::new(policy(), std::slice::from_ref(&id)).expect("health table");
    let candidates = [candidate(&id, AccountAuthentication::ApiKey, 1)];

    table
        .observe(
            &id,
            AccountObservation::RateLimited {
                retry_after_ms: None,
            },
            10,
        )
        .expect("first rate limit");
    assert_eq!(
        table.status(&id, 10).expect("status").cooldown_until_ms(),
        Some(110)
    );
    assert_eq!(
        table.select(&candidates, AccountAuthentication::ApiKey, None, 109),
        Err(SelectionError::NoEligibleAccount)
    );

    table
        .observe(
            &id,
            AccountObservation::TemporaryFailure {
                retry_after_ms: Some(900),
            },
            110,
        )
        .expect("second failure");
    assert_eq!(
        table.status(&id, 110).expect("status").cooldown_until_ms(),
        Some(1_010)
    );
    assert_eq!(
        table
            .select(&candidates, AccountAuthentication::ApiKey, None, 1_010)
            .expect("recovered")
            .account_id(),
        &id
    );
}

#[test]
fn invalid_credentials_quarantine_without_crossing_authentication_types() {
    let oauth = account("oauth");
    let api_key = account("api-key");
    let table =
        AccountHealthTable::new(policy(), &[oauth.clone(), api_key.clone()]).expect("health table");
    let candidates = [
        candidate(&oauth, AccountAuthentication::Oauth, 1),
        candidate(&api_key, AccountAuthentication::ApiKey, 1),
    ];
    table
        .observe(&oauth, AccountObservation::InvalidCredentials, 10)
        .expect("quarantine");

    assert_eq!(
        table.status(&oauth, 10).expect("status").kind(),
        AccountStatusKind::Quarantined
    );
    assert_eq!(
        table.select(&candidates, AccountAuthentication::Oauth, None, 11),
        Err(SelectionError::NoEligibleAccount)
    );
    assert_eq!(
        table
            .select(&candidates, AccountAuthentication::ApiKey, None, 11)
            .expect("key choice")
            .account_id(),
        &api_key
    );
}

#[test]
fn quota_window_and_success_observation_recover_health() {
    let id = account("quota");
    let table = AccountHealthTable::new(policy(), std::slice::from_ref(&id)).expect("health table");
    let candidates = [candidate(&id, AccountAuthentication::ApiKey, 1)];

    table
        .observe(
            &id,
            AccountObservation::Quota {
                remaining: 0,
                resets_at_ms: 500,
            },
            10,
        )
        .expect("quota");
    assert_eq!(
        table.select(&candidates, AccountAuthentication::ApiKey, None, 499),
        Err(SelectionError::NoEligibleAccount)
    );
    assert!(
        table
            .select(&candidates, AccountAuthentication::ApiKey, None, 500)
            .is_ok()
    );

    table
        .observe(&id, AccountObservation::InvalidCredentials, 600)
        .expect("quarantine");
    table
        .observe(&id, AccountObservation::Success, 601)
        .expect("success");
    let status = table.status(&id, 601).expect("status");
    assert_eq!(status.kind(), AccountStatusKind::Healthy);
    assert_eq!(status.consecutive_failures(), 0);
}

#[test]
fn observations_are_sharded_and_concurrent_without_a_global_request_lock() {
    let ids = (0..64)
        .map(|index| account(&format!("account-{index}")))
        .collect::<Vec<_>>();
    let table = Arc::new(AccountHealthTable::new(policy(), &ids).expect("health table"));
    let barrier = Arc::new(Barrier::new(8));
    let handles = ids
        .chunks_exact(8)
        .take(8)
        .map(|chunk| {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            let owned = chunk.to_vec();
            thread::spawn(move || {
                barrier.wait();
                for round in 0..100_u64 {
                    for id in &owned {
                        table
                            .observe(id, AccountObservation::Success, round)
                            .expect("observation");
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().expect("thread");
    }

    for id in &ids {
        assert_eq!(
            table.status(id, 1_000).expect("status").kind(),
            AccountStatusKind::Healthy
        );
    }
    assert_eq!(
        table.status(&account("unknown"), 1_000),
        Err(AccountStateError::UnknownAccount)
    );
}

#[test]
fn restart_restore_preserves_live_state_and_recovers_expired_windows() {
    let cooling = account("cooling");
    let quota = account("quota-restore");
    let accounts = [cooling.clone(), quota.clone()];
    let table = AccountHealthTable::new(policy(), &accounts).expect("health table");
    table
        .observe(
            &cooling,
            AccountObservation::TemporaryFailure {
                retry_after_ms: None,
            },
            100,
        )
        .expect("cooldown");
    table
        .observe(
            &quota,
            AccountObservation::Quota {
                remaining: 0,
                resets_at_ms: 180,
            },
            100,
        )
        .expect("quota");
    let snapshots = table.snapshots(120);

    let restored =
        AccountHealthTable::restore(policy(), &accounts, &snapshots, 150).expect("restore");
    assert_eq!(
        restored
            .status(&cooling, 150)
            .expect("cooling status")
            .kind(),
        AccountStatusKind::CoolingDown
    );
    assert_eq!(
        restored.status(&quota, 180).expect("quota status").kind(),
        AccountStatusKind::Healthy
    );
    assert_eq!(
        restored
            .status(&cooling, 200)
            .expect("recovered status")
            .kind(),
        AccountStatusKind::Healthy
    );

    let unknown = AccountHealthSnapshot {
        account_id: account("unknown"),
        consecutive_failures: 0,
        cooldown_until_ms: None,
        quarantined: false,
        quota_remaining: None,
        quota_resets_at_ms: None,
        selection_count: 0,
        last_selected_sequence: 0,
        updated_at_ms: 100,
    };
    assert!(matches!(
        AccountHealthTable::restore(policy(), &accounts, &[unknown], 150),
        Err(AccountStateError::UnknownAccount)
    ));
}

fn policy() -> AccountHealthPolicy {
    AccountHealthPolicy::new(100, 1_000).expect("policy")
}

fn account(value: &str) -> AccountId {
    AccountId::new(value).expect("account")
}

fn candidate(
    account_id: &AccountId,
    authentication: AccountAuthentication,
    weight: u16,
) -> AccountCandidate<'_> {
    AccountCandidate::new(account_id, authentication, weight).expect("candidate")
}
