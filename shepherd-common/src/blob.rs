//! Object store construction and the content-addressed key layout.

use std::sync::Arc;

use object_store::{
    ObjectStore, aws::AmazonS3Builder, local::LocalFileSystem, memory::InMemory, path::Path,
};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BlobBackend {
    #[default]
    S3,
    Local,
    Memory,
}

/// Blob settings, flattened into service configs (`BLOB_*` env vars).
#[derive(Debug, Clone, Deserialize)]
pub struct BlobConfig {
    #[serde(default)]
    pub blob_backend: BlobBackend,
    #[serde(default = "default_bucket")]
    pub blob_bucket: String,
    pub blob_endpoint: Option<String>,
    #[serde(default = "default_region")]
    pub blob_region: String,
    pub blob_access_key_id: Option<String>,
    pub blob_secret_access_key: Option<String>,
    #[serde(default)]
    pub blob_allow_http: bool,
    pub blob_local_path: Option<String>,
}

fn default_bucket() -> String {
    "shepherd-documents".into()
}

fn default_region() -> String {
    "us-east-1".into()
}

impl BlobConfig {
    pub fn build(&self) -> anyhow::Result<Arc<dyn ObjectStore>> {
        Ok(match self.blob_backend {
            BlobBackend::S3 => {
                let mut b = AmazonS3Builder::new()
                    .with_bucket_name(&self.blob_bucket)
                    .with_region(&self.blob_region)
                    .with_allow_http(self.blob_allow_http);
                if let Some(ep) = &self.blob_endpoint {
                    // Path-style addressing: S3-compatible servers (RustFS, MinIO) don't need DNS per bucket.
                    b = b.with_endpoint(ep).with_virtual_hosted_style_request(false);
                }
                if let (Some(k), Some(s)) = (&self.blob_access_key_id, &self.blob_secret_access_key)
                {
                    b = b.with_access_key_id(k).with_secret_access_key(s);
                }
                Arc::new(b.build()?)
            }
            BlobBackend::Local => {
                let root = self
                    .blob_local_path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("BLOB_LOCAL_PATH required for local backend"))?;
                std::fs::create_dir_all(root)?;
                Arc::new(LocalFileSystem::new_with_prefix(root)?)
            }
            BlobBackend::Memory => Arc::new(InMemory::new()),
        })
    }
}

/// Key for a blob with the given lowercase hex SHA-256: `sha256/ab/cd/<hash>`.
pub fn content_key(sha256_hex: &str) -> Path {
    Path::from(format!(
        "sha256/{}/{}/{}",
        &sha256_hex[0..2],
        &sha256_hex[2..4],
        sha256_hex
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_layout() {
        let h = "abcdef".to_string() + &"0".repeat(58);
        assert_eq!(content_key(&h).as_ref(), format!("sha256/ab/cd/{h}"));
    }
}
