use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use wokcore_core::{
    config::AccountAuthConfig,
    id::{AccountId, ClientId, ProviderId},
};

use crate::{
    accounts::AccountAuthentication,
    catalog::{AdapterFamily, AuthKind, ProviderCapabilities},
};

const STANDARD_REASONING_EFFORTS: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteRequest {
    pub provider: Option<ProviderId>,
    pub model: String,
    pub client_id: Option<ClientId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteOrigin {
    Explicit,
    Alias,
    Rule,
    Default,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteAccount {
    id: AccountId,
    auth: AccountAuthConfig,
}

impl RouteAccount {
    pub(crate) fn new(id: AccountId, auth: AccountAuthConfig) -> Self {
        Self { id, auth }
    }

    pub fn id(&self) -> &AccountId {
        &self.id
    }

    pub fn auth(&self) -> &AccountAuthConfig {
        &self.auth
    }

    pub const fn authentication(&self) -> AccountAuthentication {
        match self.auth {
            AccountAuthConfig::Forward { .. } => AccountAuthentication::Forward,
            AccountAuthConfig::Oauth { .. } => AccountAuthentication::Oauth,
            AccountAuthConfig::ApiKey { .. } => AccountAuthentication::ApiKey,
            AccountAuthConfig::Local => AccountAuthentication::Local,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RouteProvider {
    id: ProviderId,
    catalog_id: ProviderId,
    endpoint: String,
    adapter: AdapterFamily,
    auth_kind: AuthKind,
    capabilities: ProviderCapabilities,
    accounts: Arc<[RouteAccount]>,
    reasoning_efforts: Arc<[String]>,
    reasoning_effort_map: BTreeMap<String, String>,
}

impl RouteProvider {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: ProviderId,
        catalog_id: ProviderId,
        endpoint: String,
        adapter: AdapterFamily,
        auth_kind: AuthKind,
        capabilities: ProviderCapabilities,
        accounts: Arc<[RouteAccount]>,
        reasoning_efforts: Arc<[String]>,
        reasoning_effort_map: BTreeMap<String, String>,
    ) -> Self {
        Self {
            id,
            catalog_id,
            endpoint,
            adapter,
            auth_kind,
            capabilities,
            accounts,
            reasoning_efforts,
            reasoning_effort_map,
        }
    }

    pub fn id(&self) -> &ProviderId {
        &self.id
    }

    pub fn catalog_id(&self) -> &ProviderId {
        &self.catalog_id
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub const fn adapter(&self) -> AdapterFamily {
        self.adapter
    }

    pub const fn auth_kind(&self) -> AuthKind {
        self.auth_kind
    }

    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    pub fn accounts(&self) -> &[RouteAccount] {
        &self.accounts
    }
}

impl fmt::Debug for RouteProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteProvider")
            .field("id", &self.id)
            .field("catalog_id", &self.catalog_id)
            .field("adapter", &self.adapter)
            .field("auth_kind", &self.auth_kind)
            .field("account_count", &self.accounts.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteDecision {
    provider: Arc<RouteProvider>,
    model: String,
    origin: RouteOrigin,
}

impl RouteDecision {
    pub(crate) fn new(provider: Arc<RouteProvider>, model: String, origin: RouteOrigin) -> Self {
        Self {
            provider,
            model,
            origin,
        }
    }

    pub fn provider_id(&self) -> &ProviderId {
        self.provider.id()
    }

    pub fn provider(&self) -> &RouteProvider {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub const fn origin(&self) -> RouteOrigin {
        self.origin
    }

    pub fn candidates(&self) -> RouteCandidates<'_> {
        RouteCandidates {
            decision: self,
            next_account: 0,
            yielded_accountless: false,
        }
    }

    pub fn map_reasoning_effort(&self, effort: &str) -> Result<String, RouteError> {
        if !self.provider.capabilities.reasoning {
            return Err(RouteError::UnsupportedReasoningEffort);
        }
        if let Some(mapped) = self.provider.reasoning_effort_map.get(effort) {
            return Ok(mapped.clone());
        }
        let supported = if self.provider.reasoning_efforts.is_empty() {
            STANDARD_REASONING_EFFORTS.contains(&effort)
        } else {
            self.provider
                .reasoning_efforts
                .iter()
                .any(|supported| supported == effort)
        };
        if supported {
            Ok(effort.to_owned())
        } else {
            Err(RouteError::UnsupportedReasoningEffort)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteCandidate<'a> {
    provider: &'a RouteProvider,
    account: Option<&'a RouteAccount>,
    model: &'a str,
    origin: RouteOrigin,
}

impl<'a> RouteCandidate<'a> {
    pub fn provider(self) -> &'a RouteProvider {
        self.provider
    }

    pub fn account(self) -> Option<&'a RouteAccount> {
        self.account
    }

    pub fn model(self) -> &'a str {
        self.model
    }

    pub const fn origin(self) -> RouteOrigin {
        self.origin
    }
}

#[derive(Clone, Debug)]
pub struct RouteCandidates<'a> {
    decision: &'a RouteDecision,
    next_account: usize,
    yielded_accountless: bool,
}

impl<'a> Iterator for RouteCandidates<'a> {
    type Item = RouteCandidate<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let accounts = self.decision.provider.accounts();
        let account = if accounts.is_empty() {
            if self.yielded_accountless {
                return None;
            }
            self.yielded_accountless = true;
            None
        } else {
            let account = accounts.get(self.next_account)?;
            self.next_account += 1;
            Some(account)
        };
        Some(RouteCandidate {
            provider: self.decision.provider(),
            account,
            model: self.decision.model(),
            origin: self.decision.origin(),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let accounts = self.decision.provider.accounts();
        let remaining = if accounts.is_empty() {
            usize::from(!self.yielded_accountless)
        } else {
            accounts.len().saturating_sub(self.next_account)
        };
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for RouteCandidates<'_> {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RouteError {
    #[error("no route is available")]
    NoRoute,
    #[error("the selected Provider is unavailable")]
    ProviderUnavailable,
    #[error("the Provider does not support the requested reasoning effort")]
    UnsupportedReasoningEffort,
}
