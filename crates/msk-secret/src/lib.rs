//! Secret store port. Secrets are resolved by the host and never serialized
//! into model-facing observations or event logs.

use std::collections::HashMap;
use std::sync::Mutex;

/// Wrapper that deliberately does not implement `Debug` or `Display`.
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Option<SecretString>;

    fn names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Reads `{prefix}{NORMALIZED_NAME}` from the process environment. Names are
/// uppercased and every non-alphanumeric byte becomes `_`, so `api-token`
/// maps to `MSK_SECRET_API_TOKEN`.
#[derive(Debug, Clone)]
pub struct EnvSecretStore {
    prefix: String,
}

impl EnvSecretStore {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    fn variable(&self, name: &str) -> String {
        let normalized: String = name
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        format!("{}{}", self.prefix, normalized)
    }
}

impl Default for EnvSecretStore {
    fn default() -> Self {
        Self::new("MSK_SECRET_")
    }
}

impl SecretStore for EnvSecretStore {
    fn get(&self, name: &str) -> Option<SecretString> {
        std::env::var(self.variable(name))
            .ok()
            .filter(|value| !value.is_empty())
            .map(SecretString::new)
    }

    fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for (key, value) in std::env::vars() {
            if let Some(name) = key.strip_prefix(&self.prefix)
                && !value.is_empty()
            {
                names.push(name.to_ascii_lowercase());
            }
        }
        names.sort();
        names
    }
}

/// Test/embedded store. Values are held only in memory.
#[derive(Default)]
pub struct MemorySecretStore {
    values: Mutex<HashMap<String, String>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, name: impl Into<String>, value: impl Into<String>) {
        if let Ok(mut values) = self.values.lock() {
            values.insert(name.into(), value.into());
        }
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, name: &str) -> Option<SecretString> {
        self.values
            .lock()
            .ok()
            .and_then(|values| values.get(name).cloned())
            .map(SecretString::new)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .values
            .lock()
            .map(|values| values.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_names_are_normalized() {
        let name = format!("MSK_TEST_SECRET_{}", std::process::id());
        let store = EnvSecretStore::new(format!("{name}_"));
        // Safe in tests: unique per process and not read by other tests.
        unsafe { std::env::set_var(format!("{name}_API_TOKEN"), "value") };
        assert_eq!(
            store
                .get("api-token")
                .map(|secret| secret.expose().to_string()),
            Some("value".into())
        );
        assert!(store.names().iter().any(|name| name == "api_token"));
        unsafe { std::env::remove_var(format!("{name}_API_TOKEN")) };
    }

    #[test]
    fn memory_store_roundtrip() {
        let store = MemorySecretStore::new();
        store.insert("token", "s3cret");
        assert_eq!(store.get("token").unwrap().expose(), "s3cret");
        assert!(store.get("missing").is_none());
        assert_eq!(store.names(), vec!["token".to_string()]);
    }
}
