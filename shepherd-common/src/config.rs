use figment::{Figment, providers::Env};
use serde::de::DeserializeOwned;

/// Deserializes `T` from process environment variables (keys are case-insensitive).
pub fn from_env<T: DeserializeOwned>() -> anyhow::Result<T> {
    Ok(Figment::new().merge(Env::raw()).extract()?)
}
