use crate::{connection::EnvironmentEndpoint, shell::ShellState, thread::ThreadState};
use std::collections::BTreeMap;
use t3_contracts::{AuthEnvironmentScope, EnvironmentId, SessionGrantInput, session_grants_scope};

#[derive(Debug, Clone, Default)]
pub struct EnvironmentCache {
    pub shell: ShellState,
    pub selected_project: Option<String>,
    pub active_thread: Option<String>,
    pub thread: ThreadState,
    pub drafts: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct EnvironmentRecord {
    pub id: EnvironmentId,
    pub label: String,
    pub address: String,
    pub session: SessionGrantInput,
    pub cache: EnvironmentCache,
}

#[derive(Debug, Clone, Default)]
pub struct EnvironmentCatalog {
    pub records: BTreeMap<EnvironmentId, EnvironmentRecord>,
    endpoint_identities: BTreeMap<String, EnvironmentId>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum EnvironmentError {
    #[error(
        "This address now belongs to a different environment. Remove the saved connection before pairing again."
    )]
    IdentityChanged,
    #[error(
        "This server uses orchestration protocol {0}. Update the server or use a compatible client."
    )]
    IncompatibleProtocol(u64),
    #[error("Unknown environment")]
    UnknownEnvironment,
}

impl EnvironmentCatalog {
    /// The same environment may have multiple LAN/relay aliases, while equal
    /// thread IDs on different environments must remain different entities.
    pub fn register(
        &mut self,
        endpoint: &EnvironmentEndpoint,
        id: EnvironmentId,
        label: String,
        protocol: u64,
    ) -> Result<(), EnvironmentError> {
        if protocol != 2 {
            return Err(EnvironmentError::IncompatibleProtocol(protocol));
        }
        let address = endpoint.http("").to_string();
        if self
            .endpoint_identities
            .get(&address)
            .is_some_and(|known| *known != id)
        {
            return Err(EnvironmentError::IdentityChanged);
        }
        self.endpoint_identities.insert(address.clone(), id.clone());
        self.records
            .entry(id.clone())
            .and_modify(|record| {
                record.label = label.clone();
                record.address = address.clone();
            })
            .or_insert(EnvironmentRecord {
                id,
                label,
                address,
                session: SessionGrantInput::default(),
                cache: EnvironmentCache::default(),
            });
        Ok(())
    }
    pub fn set_session(
        &mut self,
        destination: &EnvironmentId,
        session: SessionGrantInput,
    ) -> Result<(), EnvironmentError> {
        self.records
            .get_mut(destination)
            .ok_or(EnvironmentError::UnknownEnvironment)?
            .session = session;
        Ok(())
    }
    pub fn allows(&self, destination: &EnvironmentId, scope: AuthEnvironmentScope) -> bool {
        self.records
            .get(destination)
            .is_some_and(|record| session_grants_scope(&record.session, scope))
    }
    pub fn revoke(&mut self, destination: &EnvironmentId) {
        if let Some(record) = self.records.get_mut(destination) {
            record.session = SessionGrantInput::default();
        }
    }
}
