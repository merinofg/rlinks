//! File storage (favicons, preview images, later snapshots/uploads): Cloudflare R2 through its
//! S3 API, or a local directory. Same interface either way, via `object_store`.

use anyhow::Result;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};
use std::sync::Arc;

#[derive(Clone)]
pub struct Storage(Arc<dyn ObjectStore>);

impl Storage {
    pub fn local(dir: &std::path::Path) -> Result<Storage> {
        std::fs::create_dir_all(dir)?;
        Ok(Storage(Arc::new(LocalFileSystem::new_with_prefix(dir)?)))
    }

    pub fn r2(endpoint: &str, bucket: &str, key_id: &str, secret: &str) -> Result<Storage> {
        let s3 = AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_bucket_name(bucket)
            .with_region("auto") // R2 ignores the region but S3 signing needs one
            .with_access_key_id(key_id)
            .with_secret_access_key(secret)
            .build()?;
        Ok(Storage(Arc::new(s3)))
    }

    pub async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        self.0.put(&Path::parse(key)?, data.into()).await?;
        Ok(())
    }

    /// `None` when the object doesn't exist.
    pub async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        match self.0.get(&Path::parse(key)?).await {
            Ok(r) => Ok(Some(r.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Last modification time, `None` when the object doesn't exist.
    pub async fn modified(&self, key: &str) -> Result<Option<DateTime<Utc>>> {
        match self.0.head(&Path::parse(key)?).await {
            Ok(meta) => Ok(Some(meta.last_modified)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        match self.0.delete(&Path::parse(key)?).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Best-effort removal of files whose rows are being deleted; failures are only logged
    /// (an orphaned file is harmless, its name is random and no longer referenced).
    pub async fn remove_all(&self, keys: &[String]) {
        for key in keys {
            if let Err(e) = self.delete(key).await {
                tracing::warn!("removing {key}: {e:#}");
            }
        }
    }
}

/// Snapshots are stored gzipped (they are large, repetitive HTML).
pub async fn gzip(data: Vec<u8>) -> Result<Vec<u8>> {
    Ok(tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&data)?;
        enc.finish()
    })
    .await??)
}

pub async fn gunzip(data: Bytes) -> Result<Vec<u8>> {
    Ok(tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&data[..])
            .read_to_end(&mut out)
            .map(|_| out)
    })
    .await??)
}

/// Image content type from the file's magic bytes (favicons are stored without extension).
pub fn sniff_image(data: &[u8]) -> &'static str {
    let head = &data[..data.len().min(256)];
    match head {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        [0, 0, 1, 0, ..] => "image/x-icon",
        _ if String::from_utf8_lossy(head).contains("<svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_put_get_delete() {
        let dir = std::env::temp_dir().join(format!("ld-storage-{}", rand::random::<u64>()));
        let s = Storage::local(&dir).unwrap();
        assert!(s.get("favicons/x").await.unwrap().is_none());
        assert!(s.modified("favicons/x").await.unwrap().is_none());
        s.put("favicons/x", Bytes::from_static(b"\x89PNG....")).await.unwrap();
        assert_eq!(sniff_image(&s.get("favicons/x").await.unwrap().unwrap()), "image/png");
        assert!(s.modified("favicons/x").await.unwrap().is_some());
        s.delete("favicons/x").await.unwrap();
        s.delete("favicons/x").await.unwrap(); // idempotent
        assert!(s.get("favicons/x").await.unwrap().is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn gzip_round_trip() {
        let gz = gzip(b"<html>hi</html>".to_vec()).await.unwrap();
        assert_eq!(gunzip(gz.into()).await.unwrap(), b"<html>hi</html>");
    }
}
