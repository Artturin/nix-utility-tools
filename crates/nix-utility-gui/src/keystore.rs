//! Where the analyzer's API key lives. Never in the settings file: the
//! desktop's Secret Service when there is one, memory for the session when
//! there is not.
//!
//! The calls block (D-Bus round trips, maybe an unlock prompt), so call them
//! from a worker thread, not from the main loop.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

const APPLICATION: &str = "nix-utility-gui";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError(pub String);

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for KeyError {}

/// What `set` did with the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// Kept by the system and still there after a restart.
    Saved,
    /// Kept in memory until the app closes; why it was not saved.
    SessionOnly(String),
}

/// `account` names what the key is for, e.g. the endpoint and host.
pub trait KeyStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, KeyError>;
    fn set(&self, account: &str, key: &str) -> Result<Stored, KeyError>;
    fn delete(&self, account: &str) -> Result<(), KeyError>;
}

#[derive(Default)]
pub struct MemoryKeyStore(Mutex<HashMap<String, String>>);

impl KeyStore for MemoryKeyStore {
    fn get(&self, account: &str) -> Result<Option<String>, KeyError> {
        Ok(self.0.lock().unwrap().get(account).cloned())
    }

    fn set(&self, account: &str, key: &str) -> Result<Stored, KeyError> {
        self.0
            .lock()
            .unwrap()
            .insert(account.to_owned(), key.to_owned());
        Ok(Stored::SessionOnly("kept in memory".into()))
    }

    fn delete(&self, account: &str) -> Result<(), KeyError> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}

/// The Secret Service through `oo7`, which uses the secret portal inside a
/// sandbox and the host's service otherwise.
pub struct SecretService;

impl SecretService {
    fn run<T>(
        work: impl std::future::Future<Output = Result<T, oo7::Error>>,
    ) -> Result<T, KeyError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| KeyError(err.to_string()))?
            .block_on(work)
            .map_err(|err| KeyError(err.to_string()))
    }

    fn attributes(account: &str) -> HashMap<&'static str, String> {
        HashMap::from([
            ("application", APPLICATION.to_owned()),
            ("account", account.to_owned()),
        ])
    }
}

impl KeyStore for SecretService {
    fn get(&self, account: &str) -> Result<Option<String>, KeyError> {
        Self::run(async {
            let keyring = oo7::Keyring::new().await?;
            keyring.unlock().await?;
            let items = keyring.search_items(&Self::attributes(account)).await?;
            match items.first() {
                Some(item) => Ok(item.secret().await?.as_str().map(str::to_owned)),
                None => Ok(None),
            }
        })
    }

    fn set(&self, account: &str, key: &str) -> Result<Stored, KeyError> {
        Self::run(async {
            let keyring = oo7::Keyring::new().await?;
            keyring.unlock().await?;
            keyring
                .create_item(
                    "nix-utility-gui analyzer API key",
                    &Self::attributes(account),
                    key,
                    true,
                )
                .await
        })
        .map(|()| Stored::Saved)
    }

    fn delete(&self, account: &str) -> Result<(), KeyError> {
        Self::run(async {
            let keyring = oo7::Keyring::new().await?;
            keyring.unlock().await?;
            keyring.delete(&Self::attributes(account)).await
        })
    }
}

/// Uses `primary`; when it fails, keeps the key in memory for the session and
/// says so, rather than losing it or failing the whole analysis.
pub struct FallbackKeyStore {
    primary: Box<dyn KeyStore>,
    session: MemoryKeyStore,
}

impl FallbackKeyStore {
    pub fn new(primary: Box<dyn KeyStore>) -> Self {
        Self {
            primary,
            session: MemoryKeyStore::default(),
        }
    }

    /// The Secret Service with the session fallback.
    pub fn system() -> Self {
        Self::new(Box::new(SecretService))
    }
}

impl KeyStore for FallbackKeyStore {
    // A key set this session wins: it is the newer one if the primary failed
    // when it was set.
    fn get(&self, account: &str) -> Result<Option<String>, KeyError> {
        if let Some(key) = self.session.get(account)? {
            return Ok(Some(key));
        }
        match self.primary.get(account) {
            Ok(key) => Ok(key),
            Err(err) => {
                log::warn!("no Secret Service for the API key: {err}");
                Ok(None)
            }
        }
    }

    fn set(&self, account: &str, key: &str) -> Result<Stored, KeyError> {
        match self.primary.set(account, key) {
            Ok(stored) => {
                self.session.delete(account)?;
                Ok(stored)
            }
            Err(err) => {
                self.session.set(account, key)?;
                Ok(Stored::SessionOnly(format!(
                    "the Secret Service is not available ({err})"
                )))
            }
        }
    }

    fn delete(&self, account: &str) -> Result<(), KeyError> {
        self.session.delete(account)?;
        if let Err(err) = self.primary.delete(account) {
            log::warn!("could not delete the API key from the Secret Service: {err}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Broken;

    impl KeyStore for Broken {
        fn get(&self, _: &str) -> Result<Option<String>, KeyError> {
            Err(KeyError("no bus".into()))
        }
        fn set(&self, _: &str, _: &str) -> Result<Stored, KeyError> {
            Err(KeyError("no bus".into()))
        }
        fn delete(&self, _: &str) -> Result<(), KeyError> {
            Err(KeyError("no bus".into()))
        }
    }

    #[test]
    fn memory_store_round_trips() {
        let store = MemoryKeyStore::default();
        assert_eq!(store.get("a").unwrap(), None);
        store.set("a", "k1").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("k1"));
        store.set("a", "k2").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("k2"));
        store.delete("a").unwrap();
        assert_eq!(store.get("a").unwrap(), None);
    }

    #[test]
    fn a_broken_primary_falls_back_to_the_session_and_says_so() {
        let store = FallbackKeyStore::new(Box::new(Broken));
        assert_eq!(store.get("a").unwrap(), None);
        let Stored::SessionOnly(why) = store.set("a", "k").unwrap() else {
            panic!("expected a session-only key");
        };
        assert!(why.contains("no bus"), "{why}");
        assert_eq!(store.get("a").unwrap().as_deref(), Some("k"));
        store.delete("a").unwrap();
        assert_eq!(store.get("a").unwrap(), None);
    }

    #[test]
    fn a_working_primary_is_used_and_the_session_copy_dropped() {
        let store = FallbackKeyStore::new(Box::new(Shared::default()));
        store.session.set("a", "old").unwrap();
        assert_eq!(store.set("a", "new").unwrap(), Stored::Saved);
        assert_eq!(store.get("a").unwrap().as_deref(), Some("new"));
    }

    #[derive(Default)]
    struct Shared(MemoryKeyStore);

    impl KeyStore for Shared {
        fn get(&self, account: &str) -> Result<Option<String>, KeyError> {
            self.0.get(account)
        }
        fn set(&self, account: &str, key: &str) -> Result<Stored, KeyError> {
            self.0.set(account, key).map(|_| Stored::Saved)
        }
        fn delete(&self, account: &str) -> Result<(), KeyError> {
            self.0.delete(account)
        }
    }
}
