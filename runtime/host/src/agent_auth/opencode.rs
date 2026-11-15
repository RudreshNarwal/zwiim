//! OpenCode's keys: one per provider, all kept in OpenCode's single Keychain
//! item as a JSON object (`{"<provider>": "<key>"}`), and handed to OpenCode
//! as each provider's own environment variable at launch. Nothing is written
//! to OpenCode's `auth.json`; a provider signed in there (`opencode auth
//! login`) is the user's and wins over a saved key.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::LazyLock;

use zeroize::Zeroizing;

/// OpenCode's provider catalog (models.dev), bundled with the Host.
const CATALOG: &str = include_str!("../../resources/opencode/models-dev.json");

/// Provider id → the variable OpenCode reads its key from, for providers
/// that take an API key in one variable. OpenCode itself uses a provider's
/// variable as its key only when it is the provider's only one; with several
/// (Google, Bedrock, Azure, …) the first is often not the key at all.
static KEY_VARS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    #[derive(serde::Deserialize)]
    struct Provider {
        id: String,
        env: Vec<String>,
        auth_kind: Option<String>,
    }
    let providers: Vec<Provider> = serde_json::from_str(CATALOG).unwrap_or_default();
    providers
        .into_iter()
        .filter(|p| p.auth_kind.as_deref() == Some("api") && p.env.len() == 1)
        .filter_map(|p| Some((p.id, p.env.into_iter().next()?)))
        .collect()
});

/// The variable OpenCode reads `provider`'s API key from; `None` for a
/// provider that is unknown or signs in another way.
pub fn provider_env(provider: &str) -> Option<&'static str> {
    KEY_VARS.get(provider).map(String::as_str)
}

pub(super) type ProviderKeys = BTreeMap<String, Zeroizing<String>>;

/// The saved keys, from the stored JSON object. Unreadable reads as none.
pub(super) fn decode(raw: &str) -> ProviderKeys {
    serde_json::from_str::<BTreeMap<String, String>>(raw)
        .map(|keys| {
            keys.into_iter()
                .map(|(provider, key)| (provider, Zeroizing::new(key)))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn encode(keys: &ProviderKeys) -> Zeroizing<String> {
    let plain: BTreeMap<&str, &str> = keys
        .iter()
        .map(|(provider, key)| (provider.as_str(), key.as_str()))
        .collect();
    Zeroizing::new(serde_json::to_string(&plain).unwrap_or_default())
}

/// The providers signed in through OpenCode's own `auth.json` under
/// `base_dir`. Only the names are kept; the credentials are never read out.
pub(super) fn own_logins(base_dir: &Path) -> BTreeSet<String> {
    let Ok(raw) = std::fs::read(base_dir.join("auth.json")) else {
        return BTreeSet::new();
    };
    let raw = Zeroizing::new(raw);
    serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&raw)
        .map(|entries| entries.keys().cloned().collect())
        .unwrap_or_default()
}

/// Put each saved key into `env` under its provider's variable, unless the
/// agent already has that variable (its settings or the Host's environment)
/// or the provider has a sign-in of OpenCode's own. Returns whether any key
/// was handed over.
pub(super) fn hand_over(
    keys: &ProviderKeys,
    own: &BTreeSet<String>,
    inherited: &dyn Fn(&str) -> Option<String>,
    env: &mut BTreeMap<String, String>,
) -> bool {
    let mut handed = false;
    for (provider, key) in keys {
        let Some(var) = provider_env(provider) else {
            continue;
        };
        if own.contains(provider) || crate::adapters::credential_set(env, inherited, var) {
            continue;
        }
        env.insert(var.to_string(), key.to_string());
        handed = true;
    }
    handed
}
