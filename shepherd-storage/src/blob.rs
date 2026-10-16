//! Content-addressed blob writes.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use object_store::{
    GetOptions, GetRange, GetResult, ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart,
    path::Path,
};
use sha2::{Digest, Sha256};
use shepherd_common::blob::content_key;
use uuid::Uuid;

/// Bodies up to this size use a single PUT; larger ones switch to multipart.
const SINGLE_PUT_MAX: usize = 8 * 1024 * 1024;
const PART_SIZE: usize = 8 * 1024 * 1024;
const PART_CONCURRENCY: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("payload exceeds {0} bytes")]
    TooLarge(u64),
    #[error("empty payload")]
    Empty,
    #[error("payload does not start with the expected signature")]
    BadPrefix,
    #[error("{0}")]
    Body(String),
    #[error(transparent)]
    Store(#[from] object_store::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub sha256: String,
    pub size: u64,
    pub key: Path,
    /// The object already existed and was reused. The caller must confirm it still exists under
    /// `gc::hold` before referencing it, since GC may be deleting it as an orphan.
    pub existed: bool,
}

#[derive(Clone)]
pub struct BlobStore {
    store: Arc<dyn ObjectStore>,
}

impl BlobStore {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    pub fn inner(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    /// Streams `body` into the store while hashing it; the object lands at its content key.
    /// The body is never fully buffered once it exceeds `SINGLE_PUT_MAX`.
    pub async fn put_stream<S, E>(
        &self,
        mut body: S,
        max: u64,
        prefix: Option<&[u8]>,
    ) -> Result<Stored, BlobError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        let mut head = BytesMut::new();
        let mut upload: Option<(WriteMultipart, Path)> = None;

        let result: Result<(), BlobError> = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|e| BlobError::Body(e.to_string()))?;
                size += chunk.len() as u64;
                if size > max {
                    return Err(BlobError::TooLarge(max));
                }
                hasher.update(&chunk);
                match &mut upload {
                    Some((w, _)) => {
                        w.wait_for_capacity(PART_CONCURRENCY).await?;
                        w.put(chunk);
                    }
                    None => {
                        head.extend_from_slice(&chunk);
                        if let Some(p) = prefix
                            && head.len() >= p.len()
                            && !head.starts_with(p)
                        {
                            return Err(BlobError::BadPrefix);
                        }
                        if head.len() > SINGLE_PUT_MAX {
                            let tmp = Path::from(format!("tmp/{}", Uuid::now_v7()));
                            let mp = self.store.put_multipart(&tmp).await?;
                            let mut w = WriteMultipart::new_with_chunk_size(mp, PART_SIZE);
                            w.put(std::mem::take(&mut head).freeze());
                            upload = Some((w, tmp));
                        }
                    }
                }
            }
            Ok(())
        }
        .await;

        if let Err(e) = result {
            if let Some((w, _)) = upload {
                let _ = w.abort().await;
            }
            return Err(e);
        }
        if size == 0 {
            return Err(BlobError::Empty);
        }
        if let Some(p) = prefix
            && upload.is_none()
            && !head.starts_with(p)
        {
            return Err(BlobError::BadPrefix);
        }

        let sha256 = hex::encode(hasher.finalize());
        let key = content_key(&sha256);
        let existed = match upload {
            None => self.put_if_absent(&key, head.freeze()).await?,
            Some((w, tmp)) => {
                w.finish().await?;
                let existed = self.exists(&key).await?;
                if existed {
                    self.store.delete(&tmp).await?;
                } else {
                    self.store.rename(&tmp, &key).await?;
                }
                existed
            }
        };
        Ok(Stored {
            sha256,
            size,
            key,
            existed,
        })
    }

    pub async fn put_bytes(&self, bytes: Bytes) -> Result<Stored, BlobError> {
        if bytes.is_empty() {
            return Err(BlobError::Empty);
        }
        let sha256 = hex::encode(Sha256::digest(&bytes));
        let key = content_key(&sha256);
        let size = bytes.len() as u64;
        let existed = self.put_if_absent(&key, bytes).await?;
        Ok(Stored {
            sha256,
            size,
            key,
            existed,
        })
    }

    pub async fn get(
        &self,
        key: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<GetResult, object_store::Error> {
        let opts = GetOptions {
            range: range.map(GetRange::Bounded),
            ..Default::default()
        };
        self.store.get_opts(&Path::from(key), opts).await
    }

    /// Cheap reachability check: a HEAD on a key that normally does not exist.
    pub async fn ping(&self) -> Result<(), object_store::Error> {
        match self.store.head(&Path::from("healthcheck")).await {
            Ok(_) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn exists(&self, key: &Path) -> Result<bool, object_store::Error> {
        match self.store.head(key).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Identical content maps to the same key, so skipping an existing object is safe.
    /// Returns whether the object already existed.
    pub async fn put_if_absent(
        &self,
        key: &Path,
        bytes: Bytes,
    ) -> Result<bool, object_store::Error> {
        if self.exists(key).await? {
            return Ok(true);
        }
        self.store.put(key, PutPayload::from_bytes(bytes)).await?;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use object_store::memory::InMemory;

    fn store() -> BlobStore {
        BlobStore::new(Arc::new(InMemory::new()))
    }

    fn chunks(
        data: &[u8],
        size: usize,
    ) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Unpin {
        stream::iter(
            data.chunks(size)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn small_stream_single_put() {
        let s = store();
        let data = b"%PDF-1.7 hello";
        let out = s
            .put_stream(chunks(data, 3), 1024, Some(b"%PDF-"))
            .await
            .unwrap();
        assert_eq!(out.sha256, hex::encode(Sha256::digest(data)));
        assert_eq!(out.size, data.len() as u64);
        let got = s
            .get(out.key.as_ref(), None)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(&got[..], data);
    }

    #[tokio::test]
    async fn large_stream_multipart_and_dedupe() {
        let s = store();
        let mut data = b"%PDF-".to_vec();
        data.resize(SINGLE_PUT_MAX * 2 + 17, 7);
        let a = s
            .put_stream(chunks(&data, 1 << 20), u64::MAX, Some(b"%PDF-"))
            .await
            .unwrap();
        let b = s
            .put_stream(chunks(&data, 1 << 20), u64::MAX, Some(b"%PDF-"))
            .await
            .unwrap();
        assert_eq!((a.existed, b.existed), (false, true));
        assert_eq!((a.key, a.sha256), (b.key.clone(), b.sha256));
        let got = s
            .get(b.key.as_ref(), Some(5..10))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(&got[..], &data[5..10]);
        let tmp: Vec<_> = s.inner().list(Some(&Path::from("tmp"))).collect().await;
        assert!(tmp.is_empty(), "temporary upload objects must be removed");
    }

    #[tokio::test]
    async fn rejects_bad_prefix_and_oversize() {
        let s = store();
        assert!(matches!(
            s.put_stream(chunks(b"<html>", 2), 1024, Some(b"%PDF-"))
                .await,
            Err(BlobError::BadPrefix)
        ));
        assert!(matches!(
            s.put_stream(chunks(b"%PDF", 4), 1024, Some(b"%PDF-")).await,
            Err(BlobError::BadPrefix)
        ));
        assert!(matches!(
            s.put_stream(chunks(&[b'%'; 100], 10), 50, None).await,
            Err(BlobError::TooLarge(50))
        ));
        assert!(matches!(
            s.put_stream(chunks(b"", 1), 50, None).await,
            Err(BlobError::Empty)
        ));
    }
}
