//! The environment's signing key pair: one JSON secret, migrated from the two secrets an earlier
//! version wrote, generated on first use (`cloud/environmentKeys.ts`).

use crate::jwt::KeyPair;
use crate::secrets::{
    CLOUD_LINK_KEY_PAIR, CLOUD_LINK_PRIVATE_KEY, CLOUD_LINK_PUBLIC_KEY, SecretError, SecretStore,
};

#[derive(Debug, Clone, thiserror::Error)]
pub enum KeyPairError {
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error("Failed to decode environment signing key pair.")]
    Decode,
    /// `create` reported the pair already exists, but it could not be read back.
    #[error("Failed to read environment signing key pair concurrently.")]
    ConcurrentRead,
}

fn read(store: &dyn SecretStore) -> Result<Option<KeyPair>, KeyPairError> {
    let Some(bytes) = store.get(CLOUD_LINK_KEY_PAIR)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| KeyPairError::Decode)
}

fn persist(store: &dyn SecretStore, pair: KeyPair) -> Result<KeyPair, KeyPairError> {
    let encoded = serde_json::to_vec(&pair).expect("key pair json");
    match store.create(CLOUD_LINK_KEY_PAIR, &encoded) {
        Ok(()) => Ok(pair),
        // A concurrent creator won: use what it stored.
        Err(error) if error.already_exists => read(store)?.ok_or(KeyPairError::ConcurrentRead),
        Err(error) => Err(error.into()),
    }
}

pub fn get_or_create_environment_key_pair(
    store: &dyn SecretStore,
) -> Result<KeyPair, KeyPairError> {
    if let Some(existing) = read(store)? {
        return Ok(existing);
    }
    let private = store.get(CLOUD_LINK_PRIVATE_KEY)?;
    let public = store.get(CLOUD_LINK_PUBLIC_KEY)?;
    if let (Some(private), Some(public)) = (private, public) {
        return persist(
            store,
            KeyPair {
                private_key: String::from_utf8_lossy(&private).into_owned(),
                public_key: String::from_utf8_lossy(&public).into_owned(),
            },
        );
    }
    persist(store, KeyPair::generate())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecretStore;
    use std::sync::Mutex;

    #[test]
    fn persists_one_atomic_secret_and_reuses_it() {
        let store = MemorySecretStore::new();
        let first = get_or_create_environment_key_pair(&store).unwrap();
        let second = get_or_create_environment_key_pair(&store).unwrap();
        assert_eq!(first, second);
        assert!(store.get(CLOUD_LINK_KEY_PAIR).unwrap().is_some());
        assert!(store.get(CLOUD_LINK_PRIVATE_KEY).unwrap().is_none());
        assert!(store.get(CLOUD_LINK_PUBLIC_KEY).unwrap().is_none());
    }

    #[test]
    fn migrates_a_legacy_pair_into_the_atomic_secret() {
        let store = MemorySecretStore::with(&[
            (CLOUD_LINK_PRIVATE_KEY, "private"),
            (CLOUD_LINK_PUBLIC_KEY, "public"),
        ]);
        let pair = get_or_create_environment_key_pair(&store).unwrap();
        assert_eq!(pair.private_key, "private");
        assert_eq!(pair.public_key, "public");
        assert!(store.get(CLOUD_LINK_KEY_PAIR).unwrap().is_some());
    }

    /// A store where `create` always loses to a concurrent creator whose value `get` then returns.
    struct LosingStore {
        create_attempted: Mutex<bool>,
    }

    impl SecretStore for LosingStore {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError> {
            let attempted = *self.create_attempted.lock().unwrap();
            Ok((name == CLOUD_LINK_KEY_PAIR && attempted).then(|| {
                br#"{"privateKey":"winner-private","publicKey":"winner-public"}"#.to_vec()
            }))
        }
        fn set(&self, _: &str, _: &[u8]) -> Result<(), SecretError> {
            unreachable!("unused secret-store operation")
        }
        fn create(&self, name: &str, _: &[u8]) -> Result<(), SecretError> {
            *self.create_attempted.lock().unwrap() = true;
            Err(SecretError::already_exists(name))
        }
        fn remove(&self, _: &str) -> Result<(), SecretError> {
            unreachable!("unused secret-store operation")
        }
    }

    #[test]
    fn uses_the_persisted_pair_when_a_concurrent_creator_wins() {
        let store = LosingStore {
            create_attempted: Mutex::new(false),
        };
        let pair = get_or_create_environment_key_pair(&store).unwrap();
        assert_eq!(pair.private_key, "winner-private");
        assert_eq!(pair.public_key, "winner-public");
    }
}
