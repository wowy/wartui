//! The API keys kept apart from the other settings, as a phone keeps them in its Keychain or
//! Keystore.
//!
//! [`Split`] is a [`SettingsStore`] over two stores: the `[api-keys]` table goes to a
//! [`SecretStore`], and the rest to a settings store, so [`Settings`](super::Settings) and
//! [`load`](super::load) work on one settings text as they do on a desktop's file.
//!
//! **The secret store sees only the keys**, as the `[api-keys]` table alone in the settings' own
//! spelling. A save always writes them, so clearing a key clears it there too, and writes them
//! before the rest: a failure then leaves the other settings as they were.
//!
//! **A load merges the two.** A key the secret store holds wins. A key in the settings' own text,
//! written before the split or by hand, is used while the secret store holds none for it, and
//! moves to the secret store on the next save. Settings text that does not parse is given back as
//! it is, so [`load`](super::load) refuses it with the store's place, as it does for a file.

use std::io;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{ApiKeys, SettingsStore};

/// The settings' table the keys live in.
const TABLE: &str = "api-keys";

/// Where the API keys are kept when they are kept apart: a keychain.
pub trait SecretStore: std::fmt::Debug + Send + Sync {
    /// The stored keys' text, or `None` when none is stored.
    ///
    /// # Errors
    /// The store's own, when it holds text it cannot give.
    fn load(&self) -> io::Result<Option<String>>;
    /// Replace the stored keys' text.
    ///
    /// # Errors
    /// The store's own failure.
    fn save(&self, text: &str) -> io::Result<()>;
}

/// A settings store that keeps the API keys in `secrets` and everything else in `settings`.
#[derive(Debug)]
pub struct Split {
    settings: Arc<dyn SettingsStore>,
    secrets: Arc<dyn SecretStore>,
}

impl Split {
    /// The settings in `settings`, their API keys in `secrets`.
    #[must_use]
    pub fn new(settings: Arc<dyn SettingsStore>, secrets: Arc<dyn SecretStore>) -> Self {
        Self { settings, secrets }
    }

    /// The keys the secret store holds, or `None` when it holds none.
    fn secret_keys(&self) -> io::Result<Option<ApiKeys>> {
        let Some(text) = self.secrets.load()? else { return Ok(None) };
        let secret: Secret = toml::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Some(secret.api_keys))
    }
}

/// The secret store's text: the `[api-keys]` table alone.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct Secret {
    #[serde(default)]
    api_keys: ApiKeys,
}

impl SettingsStore for Split {
    fn load(&self) -> io::Result<Option<String>> {
        let secret = self.secret_keys()?.filter(ApiKeys::any_set);
        let text = self.settings.load()?;
        let Some(secret) = secret else { return Ok(text) };
        let Some(text) = text else {
            return Ok(Some(
                toml::to_string(&Secret { api_keys: secret }).map_err(io::Error::other)?,
            ));
        };
        // Text that does not parse, or keys that do not, are `load`'s to refuse.
        let Ok(mut table) = text.parse::<toml::Table>() else { return Ok(Some(text)) };
        let own = match table.remove(TABLE).map(ApiKeys::deserialize).transpose() {
            Ok(own) => own.unwrap_or_default(),
            Err(_) => return Ok(Some(text)),
        };
        let pick = |secret: String, own: String| if secret.is_empty() { own } else { secret };
        let merged = ApiKeys {
            wdgwars: pick(secret.wdgwars, own.wdgwars),
            wigle_name: pick(secret.wigle_name, own.wigle_name),
            wigle_token: pick(secret.wigle_token, own.wigle_token),
        };
        let merged = toml::Value::try_from(merged).map_err(io::Error::other)?;
        table.insert(TABLE.to_owned(), merged);
        Ok(Some(toml::to_string(&table).map_err(io::Error::other)?))
    }

    fn save(&self, text: &str, _secret: bool) -> io::Result<()> {
        let invalid = |error| io::Error::new(io::ErrorKind::InvalidData, error);
        let mut table = text.parse::<toml::Table>().map_err(invalid)?;
        let api_keys = table
            .remove(TABLE)
            .map(ApiKeys::deserialize)
            .transpose()
            .map_err(invalid)?
            .unwrap_or_default();
        let secret = toml::to_string(&Secret { api_keys }).map_err(io::Error::other)?;
        let rest = toml::to_string(&table).map_err(io::Error::other)?;
        self.secrets.save(&secret)?;
        self.settings.save(&rest, false)
    }

    fn place(&self) -> String {
        self.settings.place()
    }
}
