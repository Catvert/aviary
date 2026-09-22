//! AI provider API keys in the OS credential store.
//!
//! Same store as the IMAP/SMTP passwords (`auth::imap`, via the `keyring`
//! crate — Keychain, Credential Manager, Secret Service), one entry per
//! provider under the `aviary-ai` service. `settings.json` only keeps a key in
//! plaintext when the keyring refused it (no Secret Service on a bare Linux
//! session, a locked store…): losing the key would be worse than keeping it
//! where it always was, and the AI preferences say so.
//!
//! The keyring may block (a D-Bus round trip, an unlock prompt), so every call
//! runs on one dedicated thread fed by a channel, never on gpui's thread. One
//! thread rather than a task per call also keeps the writes in submission
//! order: two quick saves from the preferences cannot land in the keyring
//! reversed.
//!
//! The decisions themselves — what to migrate, what stays in plaintext, which
//! reply still applies once the UI has moved on — are pure functions over a
//! [`SecretStore`], tested without touching a real keyring.

use crate::ai::{AiApiKeys, AiProvider, AiSettings};
use anyhow::{Context, Result};
use std::future::Future;
use std::sync::mpsc;
use std::sync::OnceLock;

const KEYRING_SERVICE: &str = "aviary-ai";

fn keyring_user(provider: AiProvider) -> &'static str {
    match provider {
        AiProvider::OpenAi => "openai",
        AiProvider::Anthropic => "anthropic",
        AiProvider::Gemini => "gemini",
        AiProvider::Local => "local",
    }
}

/// The three operations the key logic needs from a credential store.
pub trait SecretStore {
    /// `Ok(None)` when there is simply no entry.
    fn get(&mut self, provider: AiProvider) -> Result<Option<String>>;
    fn set(&mut self, provider: AiProvider, key: &str) -> Result<()>;
    /// Idempotent: deleting a missing entry succeeds.
    fn delete(&mut self, provider: AiProvider) -> Result<()>;
}

/// The real OS keyring.
struct OsKeyring;

impl OsKeyring {
    fn entry(provider: AiProvider) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, keyring_user(provider))
            .context(tr!("auth-error-keyring-open"))
    }
}

impl SecretStore for OsKeyring {
    fn get(&mut self, provider: AiProvider) -> Result<Option<String>> {
        match Self::entry(provider)?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("reading an AI API key from the keyring"),
        }
    }

    fn set(&mut self, provider: AiProvider, key: &str) -> Result<()> {
        Self::entry(provider)?
            .set_password(key)
            .context(tr!("auth-error-keyring-write"))
    }

    fn delete(&mut self, provider: AiProvider) -> Result<()> {
        match Self::entry(provider)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("deleting an AI API key from the keyring"),
        }
    }
}

/// Outcome of loading the keys at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyLoad {
    /// Keys to use.
    pub keys: AiApiKeys,
    /// Keys that must stay in `settings.json`: those the keyring refused.
    pub plaintext: AiApiKeys,
}

/// Loads every provider's key, moving a plaintext one into the store first.
///
/// A plaintext key wins over whatever the store holds: it is the one the user
/// last saw, and it is only still in plaintext because an earlier write
/// failed. If the store refuses it again, it stays in plaintext — never lost.
pub fn migrate(plaintext: &AiApiKeys, store: &mut impl SecretStore) -> KeyLoad {
    let mut load = KeyLoad::default();
    for provider in AiProvider::ALL {
        let clear = plaintext.get(provider);
        if !clear.is_empty() {
            load.keys.set(provider, clear.to_string());
            match store.set(provider, clear) {
                Ok(()) => log::info!("moved the {provider:?} API key into the OS keyring"),
                Err(e) => {
                    log::warn!(
                        "OS keyring unavailable ({e:#}); the {provider:?} API key stays \
                         in plaintext in settings.json"
                    );
                    load.plaintext.set(provider, clear.to_string());
                }
            }
            continue;
        }
        match store.get(provider) {
            Ok(Some(key)) => load.keys.set(provider, key),
            Ok(None) => {}
            Err(e) => log::warn!("could not read the {provider:?} API key from the keyring: {e:#}"),
        }
    }
    load
}

/// One write requested by the preferences, and whether the store took it.
/// An empty `key` means "delete the entry".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyWrite {
    pub provider: AiProvider,
    pub key: String,
    pub stored: bool,
}

pub fn write_all(writes: Vec<(AiProvider, String)>, store: &mut impl SecretStore) -> Vec<KeyWrite> {
    writes
        .into_iter()
        .map(|(provider, key)| {
            let result = if key.is_empty() {
                store.delete(provider)
            } else {
                store.set(provider, &key)
            };
            if let Err(e) = &result {
                log::warn!("could not update the {provider:?} API key in the keyring: {e:#}");
            }
            KeyWrite {
                provider,
                key,
                stored: result.is_ok(),
            }
        })
        .collect()
}

pub fn purge_with<S: SecretStore>(store: &mut S) {
    for provider in AiProvider::ALL {
        if let Err(e) = store.delete(provider) {
            log::warn!("factory reset: could not delete the {provider:?} API key: {e:#}");
        }
    }
}

/// Folds a [`KeyLoad`] into the settings. `snapshot` is the plaintext the load
/// started from (and what `api_keys` was seeded with): a provider the user
/// has saved from the preferences meanwhile no longer matches it and is left
/// alone. Returns whether `settings.json` must be rewritten.
pub fn apply_load(settings: &mut AiSettings, snapshot: &AiApiKeys, load: &KeyLoad) -> bool {
    let mut dirty = false;
    for provider in AiProvider::ALL {
        let before = snapshot.get(provider);
        if settings.api_keys.get(provider) == before {
            settings
                .api_keys
                .set(provider, load.keys.get(provider).to_string());
        }
        if settings.plaintext_api_keys.get(provider) == before
            && before != load.plaintext.get(provider)
        {
            settings
                .plaintext_api_keys
                .set(provider, load.plaintext.get(provider).to_string());
            dirty = true;
        }
    }
    dirty
}

/// Applies the keys typed in the preferences and returns the keyring writes
/// to perform.
///
/// A changed key is written; so is an unchanged one still held in plaintext,
/// which retries the keyring on every save. The plaintext copy of a changed
/// key is updated only where the keyring already refused one — a key that has
/// never been in plaintext does not reach the disk before the keyring has had
/// its say ([`apply_writes`] puts it there on refusal).
pub fn plan_save(settings: &mut AiSettings, typed: AiApiKeys) -> Vec<(AiProvider, String)> {
    let mut writes = Vec::new();
    for provider in AiProvider::ALL {
        let key = typed.get(provider);
        let changed = key != settings.api_keys.get(provider);
        let in_plaintext = !settings.plaintext_api_keys.get(provider).is_empty();
        if changed || in_plaintext {
            writes.push((provider, key.to_string()));
        }
        if changed && in_plaintext {
            settings.plaintext_api_keys.set(provider, key.to_string());
        }
    }
    settings.api_keys = typed;
    writes
}

/// Folds the keyring's answers into the settings: a refused key goes (back)
/// to plaintext, an accepted one leaves it. Answers about a key the user has
/// replaced since are ignored. Returns whether `settings.json` must be
/// rewritten.
pub fn apply_writes(settings: &mut AiSettings, writes: &[KeyWrite]) -> bool {
    let mut dirty = false;
    for write in writes {
        if settings.api_keys.get(write.provider) != write.key {
            continue;
        }
        let target = if write.stored { "" } else { write.key.as_str() };
        if settings.plaintext_api_keys.get(write.provider) != target {
            settings
                .plaintext_api_keys
                .set(write.provider, target.to_string());
            dirty = true;
        }
    }
    dirty
}

type Job = Box<dyn FnOnce(&mut OsKeyring) + Send>;

/// The keyring thread, started on first use.
fn worker() -> &'static mpsc::Sender<Job> {
    static WORKER: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("aviary-ai-keys".into())
            .spawn(move || {
                let mut store = OsKeyring;
                while let Ok(job) = rx.recv() {
                    job(&mut store);
                }
            });
        if let Err(e) = spawned {
            log::warn!("could not start the AI key thread: {e:#}");
        }
        tx
    })
}

/// Runs `job` on the keyring thread. Resolves to `None` only if that thread
/// is gone. The future is executor-agnostic: gpui and tokio both await it.
fn run<T: Send + 'static>(
    job: impl FnOnce(&mut OsKeyring) -> T + Send + 'static,
) -> impl Future<Output = Option<T>> {
    let (tx, rx) = futures::channel::oneshot::channel();
    let _ = worker().send(Box::new(move |store| {
        let _ = tx.send(job(store));
    }));
    async move { rx.await.ok() }
}

/// Startup: migrate plaintext keys and load the others.
pub async fn load(plaintext: AiApiKeys) -> KeyLoad {
    run(move |store| migrate(&plaintext, store))
        .await
        .unwrap_or_default()
}

/// Preferences: perform the writes returned by [`plan_save`].
pub async fn store(writes: Vec<(AiProvider, String)>) -> Vec<KeyWrite> {
    run(move |store| write_all(writes, store))
        .await
        .unwrap_or_default()
}

/// One key, for the runtime when a request arrives before the startup load
/// has reached the UI.
pub async fn read(provider: AiProvider) -> Option<String> {
    run(move |store| store.get(provider).ok().flatten())
        .await
        .flatten()
        .filter(|key| !key.is_empty())
}

/// Factory reset: delete every entry. Fire and forget, but queued behind any
/// write still pending, so nothing is re-created afterwards.
pub fn purge() {
    let _ = worker().send(Box::new(purge_with::<OsKeyring>));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// In-memory store; `broken` makes every call fail like a missing
    /// Secret Service.
    #[derive(Default)]
    struct FakeStore {
        entries: HashMap<AiProvider, String>,
        broken: bool,
    }

    impl SecretStore for FakeStore {
        fn get(&mut self, provider: AiProvider) -> Result<Option<String>> {
            if self.broken {
                anyhow::bail!("no secret service");
            }
            Ok(self.entries.get(&provider).cloned())
        }
        fn set(&mut self, provider: AiProvider, key: &str) -> Result<()> {
            if self.broken {
                anyhow::bail!("no secret service");
            }
            self.entries.insert(provider, key.to_string());
            Ok(())
        }
        fn delete(&mut self, provider: AiProvider) -> Result<()> {
            if self.broken {
                anyhow::bail!("no secret service");
            }
            self.entries.remove(&provider);
            Ok(())
        }
    }

    fn keys(pairs: &[(AiProvider, &str)]) -> AiApiKeys {
        let mut keys = AiApiKeys::default();
        for (provider, key) in pairs {
            keys.set(*provider, key.to_string());
        }
        keys
    }

    fn loaded_settings(plaintext: AiApiKeys) -> AiSettings {
        AiSettings {
            api_keys: plaintext.clone(),
            plaintext_api_keys: plaintext,
            ..AiSettings::default()
        }
    }

    #[test]
    fn plaintext_keys_move_into_the_keyring_and_leave_settings() {
        let plaintext = keys(&[
            (AiProvider::OpenAi, "sk-test-a"),
            (AiProvider::Local, "local-test"),
        ]);
        let mut store = FakeStore::default();
        store
            .entries
            .insert(AiProvider::Gemini, "gm-test-stored".into());

        let load = migrate(&plaintext, &mut store);

        assert_eq!(store.entries[&AiProvider::OpenAi], "sk-test-a");
        assert_eq!(store.entries[&AiProvider::Local], "local-test");
        assert_eq!(load.keys.openai, "sk-test-a");
        assert_eq!(load.keys.gemini, "gm-test-stored");
        assert!(load.plaintext.is_empty());

        let mut settings = loaded_settings(plaintext.clone());
        assert!(apply_load(&mut settings, &plaintext, &load));
        assert!(settings.plaintext_api_keys.is_empty());
        assert_eq!(settings.api_keys, load.keys);
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("sk-test-a") && !json.contains("local-test"));
    }

    #[test]
    fn a_refused_key_stays_in_plaintext() {
        let plaintext = keys(&[(AiProvider::Anthropic, "ak-test")]);
        let mut store = FakeStore {
            broken: true,
            ..FakeStore::default()
        };

        let load = migrate(&plaintext, &mut store);

        assert_eq!(load.keys.anthropic, "ak-test");
        assert_eq!(load.plaintext.anthropic, "ak-test");
        let mut settings = loaded_settings(plaintext.clone());
        assert!(!apply_load(&mut settings, &plaintext, &load));
        assert_eq!(settings.plaintext_api_keys.anthropic, "ak-test");
        assert_eq!(settings.api_keys.anthropic, "ak-test");
    }

    #[test]
    fn a_plaintext_key_wins_over_a_stale_keyring_entry() {
        let plaintext = keys(&[(AiProvider::OpenAi, "sk-test-new")]);
        let mut store = FakeStore::default();
        store
            .entries
            .insert(AiProvider::OpenAi, "sk-test-old".into());
        let load = migrate(&plaintext, &mut store);
        assert_eq!(load.keys.openai, "sk-test-new");
        assert_eq!(store.entries[&AiProvider::OpenAi], "sk-test-new");
    }

    #[test]
    fn a_load_does_not_overwrite_a_key_saved_meanwhile() {
        let plaintext = keys(&[(AiProvider::OpenAi, "sk-test-old")]);
        let mut settings = loaded_settings(plaintext.clone());
        // The user saves a new key before the startup load answers.
        let writes = plan_save(&mut settings, keys(&[(AiProvider::OpenAi, "sk-test-new")]));
        assert_eq!(
            writes,
            vec![(AiProvider::OpenAi, "sk-test-new".to_string())]
        );

        let load = KeyLoad {
            keys: plaintext.clone(),
            plaintext: AiApiKeys::default(),
        };
        apply_load(&mut settings, &plaintext, &load);
        assert_eq!(settings.api_keys.openai, "sk-test-new");
    }

    #[test]
    fn a_new_key_reaches_plaintext_only_once_refused() {
        let mut settings = AiSettings::default();
        let writes = plan_save(&mut settings, keys(&[(AiProvider::Gemini, "gm-test")]));
        assert!(settings.plaintext_api_keys.is_empty());

        let mut broken = FakeStore {
            broken: true,
            ..FakeStore::default()
        };
        let results = write_all(writes.clone(), &mut broken);
        assert!(apply_writes(&mut settings, &results));
        assert_eq!(settings.plaintext_api_keys.gemini, "gm-test");

        // Next save, keyring back: the unchanged plaintext key is retried and
        // leaves settings.json once stored.
        let retry = plan_save(&mut settings, keys(&[(AiProvider::Gemini, "gm-test")]));
        assert_eq!(retry, writes);
        let mut store = FakeStore::default();
        let results = write_all(retry, &mut store);
        assert!(apply_writes(&mut settings, &results));
        assert!(settings.plaintext_api_keys.is_empty());
        assert_eq!(store.entries[&AiProvider::Gemini], "gm-test");
    }

    #[test]
    fn clearing_a_field_deletes_the_entry() {
        let mut store = FakeStore::default();
        store.entries.insert(AiProvider::OpenAi, "sk-test".into());
        let mut settings = AiSettings::default();
        settings.api_keys.openai = "sk-test".into();

        let writes = plan_save(&mut settings, AiApiKeys::default());
        assert_eq!(writes, vec![(AiProvider::OpenAi, String::new())]);
        let results = write_all(writes, &mut store);
        assert!(!apply_writes(&mut settings, &results));
        assert!(store.entries.is_empty());
    }

    #[test]
    fn unchanged_keys_in_the_keyring_are_not_rewritten() {
        let mut settings = AiSettings::default();
        settings.api_keys.openai = "sk-test".into();
        let typed = settings.api_keys.clone();
        assert!(plan_save(&mut settings, typed).is_empty());
    }

    #[test]
    fn a_stale_write_answer_is_ignored() {
        let mut settings = AiSettings::default();
        settings.api_keys.openai = "sk-test-newer".into();
        let stale = [KeyWrite {
            provider: AiProvider::OpenAi,
            key: "sk-test-older".into(),
            stored: false,
        }];
        assert!(!apply_writes(&mut settings, &stale));
        assert!(settings.plaintext_api_keys.is_empty());
    }

    #[test]
    fn purge_deletes_every_provider() {
        let mut store = FakeStore::default();
        for provider in AiProvider::ALL {
            store.entries.insert(provider, "k".into());
        }
        purge_with(&mut store);
        assert!(store.entries.is_empty());
    }
}
