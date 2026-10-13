//! Leaf identifier types shared by trace and catalog code; imports nothing from either,
//! so neither module has to depend on the other.

use serde::{Deserialize, Serialize};

/// A configured upstream's name (its config key), never an index, so history survives a hot-swap.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UpstreamName(String);

impl UpstreamName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for UpstreamName {
    fn from(name: &str) -> Self {
        Self(name.to_string())
    }
}

impl From<String> for UpstreamName {
    fn from(name: String) -> Self {
        Self(name)
    }
}

impl std::fmt::Display for UpstreamName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
