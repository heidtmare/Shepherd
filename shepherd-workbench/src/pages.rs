//! Page images for the workbench, rendered from the source PDF with pdfium and cached in memory.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use uuid::Uuid;

/// Small FIFO-evicting cache.
pub struct Cache<K, V> {
    cap: usize,
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> Cache<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get(&self, k: &K) -> Option<V> {
        self.map.get(k).cloned()
    }

    pub fn put(&mut self, k: K, v: V) {
        if self.map.insert(k.clone(), v).is_none() {
            self.order.push_back(k);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

type ImageCache = Cache<(Uuid, u32), Arc<Vec<u8>>>;

pub struct Pages {
    pub dpi: f32,
    pdfs: Mutex<Cache<Uuid, Bytes>>,
    images: Mutex<ImageCache>,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct PageInfo {
    pub number: u32,
    /// PDF points.
    pub width: f32,
    pub height: f32,
}

impl Pages {
    pub fn new(dpi: f32) -> Self {
        Self {
            dpi,
            pdfs: Mutex::new(Cache::new(16)),
            images: Mutex::new(Cache::new(96)),
        }
    }

    pub fn cached_pdf(&self, source_artifact: Uuid) -> Option<Bytes> {
        self.pdfs.lock().expect("pdf cache").get(&source_artifact)
    }

    pub fn put_pdf(&self, source_artifact: Uuid, bytes: Bytes) {
        self.pdfs
            .lock()
            .expect("pdf cache")
            .put(source_artifact, bytes);
    }

    /// PNG for 1-based `page`, rendering on miss.
    pub async fn png(
        &self,
        source_artifact: Uuid,
        pdf: Bytes,
        page: u32,
    ) -> anyhow::Result<Arc<Vec<u8>>> {
        if let Some(img) = self
            .images
            .lock()
            .expect("image cache")
            .get(&(source_artifact, page))
        {
            return Ok(img);
        }
        let dpi = self.dpi;
        let png = tokio::task::spawn_blocking(move || {
            shepherd_forms::render::render_png(&pdf, page as usize - 1, dpi)
        })
        .await??;
        let png = Arc::new(png);
        self.images
            .lock()
            .expect("image cache")
            .put((source_artifact, page), png.clone());
        Ok(png)
    }

    pub async fn info(pdf: Bytes) -> anyhow::Result<Vec<PageInfo>> {
        let pages =
            tokio::task::spawn_blocking(move || shepherd_forms::render::render(&pdf, 9.0, None))
                .await??;
        Ok(pages
            .into_iter()
            .enumerate()
            .map(|(i, p)| PageInfo {
                number: i as u32 + 1,
                width: p.width_pt,
                height: p.height_pt,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::Cache;

    #[test]
    fn evicts_oldest() {
        let mut c = Cache::new(2);
        c.put(1, "a");
        c.put(2, "b");
        c.put(3, "c");
        assert_eq!(
            (c.get(&1), c.get(&2), c.get(&3)),
            (None, Some("b"), Some("c"))
        );
    }
}
