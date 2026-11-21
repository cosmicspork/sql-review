//! Passwords from the OS keyring, for the standalone modes only.
//!
//! The CLI and `mcp` read a password here when neither the environment nor
//! `.env` gives one: service `sql-review`, account `<profile>` for the
//! read-only login and `<profile>-write` for the write login, the profile
//! being `default` when none is chosen. `serve` never builds a [`Keyring`]:
//! under Tracon every credential arrives with the call.
//!
//! The store behind it is the platform's own: the Secret Service on Linux
//! and the BSDs (over D-Bus, in pure Rust), the login Keychain on macOS, and
//! the Credential Manager on Windows. It is opened on the first lookup, so a
//! run that never needs a password never touches it.

use std::sync::{Arc, OnceLock};

use crate::config::DEFAULT_PROFILE;

/// The keyring service every entry is stored under.
pub const SERVICE: &str = "sql-review";

/// The account an entry is stored under: the profile, lowercased (`default`
/// when there is none), with `-write` for the write login.
pub fn account(profile: Option<&str>, write: bool) -> String {
    let profile = profile
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_PROFILE)
        .to_ascii_lowercase();
    if write {
        format!("{profile}-write")
    } else {
        profile
    }
}

/// A handle on a credential store, opened when first used. Cloning shares
/// it.
#[derive(Clone)]
pub struct Keyring(Arc<Inner>);

struct Inner {
    open: fn() -> Result<Store, String>,
    store: OnceLock<Result<Store, String>>,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keyring")
    }
}

impl Keyring {
    /// The platform's keyring.
    pub fn os() -> Self {
        Self::with(imp::open_os)
    }

    /// An in-memory store with nothing in it, shared by every clone: what
    /// the tests use in place of the platform's.
    pub fn mock() -> Self {
        Self::with(imp::open_mock)
    }

    pub(crate) fn with(open: fn() -> Result<Store, String>) -> Self {
        Keyring(Arc::new(Inner {
            open,
            store: OnceLock::new(),
        }))
    }

    fn store(&self) -> Result<&Store, String> {
        self.0
            .store
            .get_or_init(|| (self.0.open)())
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The password stored for `account`, or `None` when there is none.
    pub fn get(&self, account: &str) -> Result<Option<String>, String> {
        imp::get(self.store()?, account).map_err(|e| failed("read", account, e))
    }

    pub fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        imp::set(self.store()?, account, secret).map_err(|e| failed("write", account, e))
    }

    /// Remove `account`'s password. `false` when there was none.
    pub fn delete(&self, account: &str) -> Result<bool, String> {
        imp::delete(self.store()?, account).map_err(|e| failed("delete", account, e))
    }
}

fn failed(what: &str, account: &str, e: String) -> String {
    format!("could not {what} {SERVICE}/{account} in the OS keyring: {e}")
}

#[cfg(feature = "keyring")]
use imp::Store;

#[cfg(feature = "keyring")]
mod imp {
    use std::sync::Arc;

    use keyring_core::{CredentialStore, Entry, Error};

    use super::SERVICE;

    pub type Store = Arc<CredentialStore>;

    pub fn open_os() -> Result<Store, String> {
        #[cfg(target_os = "macos")]
        let store = apple_native_keyring_store::keychain::Store::new();
        #[cfg(windows)]
        let store = windows_native_keyring_store::Store::new();
        #[cfg(all(unix, not(target_os = "macos")))]
        let store = zbus_secret_service_keyring_store::Store::new();
        store
            .map(|s| s as Store)
            .map_err(|e| format!("the OS keyring is not available: {e}"))
    }

    pub fn open_mock() -> Result<Store, String> {
        keyring_core::mock::Store::new()
            .map(|s| s as Store)
            .map_err(|e| e.to_string())
    }

    fn entry(store: &Store, account: &str) -> Result<Entry, String> {
        Entry::new_in_store(store, SERVICE, account).map_err(|e| e.to_string())
    }

    pub fn get(store: &Store, account: &str) -> Result<Option<String>, String> {
        match entry(store, account)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn set(store: &Store, account: &str, secret: &str) -> Result<(), String> {
        entry(store, account)?
            .set_password(secret)
            .map_err(|e| e.to_string())
    }

    pub fn delete(store: &Store, account: &str) -> Result<bool, String> {
        match entry(store, account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(Error::NoEntry) => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(not(feature = "keyring"))]
use imp::Store;

/// Built without the `keyring` feature: there is no store to open, so every
/// lookup says so.
#[cfg(not(feature = "keyring"))]
mod imp {
    pub enum Store {}

    const ABSENT: &str = "this build of sql-review has no keyring support (the `keyring` feature)";

    pub fn open_os() -> Result<Store, String> {
        Err(ABSENT.into())
    }

    pub fn open_mock() -> Result<Store, String> {
        Err(ABSENT.into())
    }

    pub fn get(store: &Store, _: &str) -> Result<Option<String>, String> {
        match *store {}
    }

    pub fn set(store: &Store, _: &str, _: &str) -> Result<(), String> {
        match *store {}
    }

    pub fn delete(store: &Store, _: &str) -> Result<bool, String> {
        match *store {}
    }
}

#[cfg(all(test, feature = "keyring"))]
mod tests {
    use super::*;

    #[test]
    fn accounts_name_the_profile() {
        assert_eq!(account(None, false), "default");
        assert_eq!(account(Some(""), false), "default");
        assert_eq!(account(Some("QA"), false), "qa");
        assert_eq!(account(Some("qa"), true), "qa-write");
        assert_eq!(account(None, true), "default-write");
    }

    #[test]
    fn set_get_delete() {
        let k = Keyring::mock();
        assert_eq!(k.get("qa"), Ok(None));
        k.set("qa", "pw").unwrap();
        // A clone shares the store; another mock does not.
        assert_eq!(k.clone().get("qa"), Ok(Some("pw".into())));
        assert_eq!(Keyring::mock().get("qa"), Ok(None));
        assert_eq!(k.get("qa-write"), Ok(None));
        assert_eq!(k.delete("qa"), Ok(true));
        assert_eq!(k.delete("qa"), Ok(false));
        assert_eq!(k.get("qa"), Ok(None));
    }

    #[test]
    fn a_store_that_will_not_open_is_an_error() {
        let k = Keyring::with(|| Err("no Secret Service".into()));
        assert_eq!(k.get("qa"), Err("no Secret Service".into()));
    }
}
