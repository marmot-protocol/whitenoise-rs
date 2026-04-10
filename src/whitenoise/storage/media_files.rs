use crate::whitenoise::error::{Result, WhitenoiseError};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// Filesystem storage for media files using content-addressed storage
///
/// Directory structure:
/// ```text
/// <cache_dir>/
///   <hash>.<ext>
/// ```
///
/// Files are stored in a flat structure, deduplicated by content hash.
/// Multiple groups can reference the same file through database records.
/// The database maintains the relationship between groups and files, as
/// well as metadata like media type, mime type, etc.
///
/// This module handles:
/// - Creating the cache directory
/// - Atomic file writes
/// - File retrieval by path
/// - File existence checks
/// - Finding files by prefix
///
/// It does NOT handle:
/// - Database operations (see database::media_files)
/// - Network operations (see BlossomClient)
/// - Encryption/decryption (caller's responsibility)
/// - Group/file relationships (see database::media_files)
/// - Media type classification (see database::media_files)
pub struct MediaFileStorage {
    cache_dir: PathBuf,
}

impl MediaFileStorage {
    /// Creates a new MediaFileStorage instance
    ///
    /// # Arguments
    /// * `data_dir` - The application data directory
    ///
    /// # Returns
    /// A new MediaFileStorage instance with cache directory at `<data_dir>/media_cache/`
    pub(crate) async fn new(data_dir: &Path) -> Result<Self> {
        let cache_dir = data_dir.join("media_cache");

        // Create cache directory if it doesn't exist
        if !cache_dir.exists() {
            tokio::fs::create_dir_all(&cache_dir).await.map_err(|e| {
                WhitenoiseError::MediaCache(format!("Failed to create cache directory: {}", e))
            })?;
        }

        Ok(Self { cache_dir })
    }

    /// Stores a file in the cache using atomic write
    ///
    /// Files with the same content (hash) will be stored only once.
    /// Multiple groups can reference the same file via database records.
    ///
    /// # Arguments
    /// * `filename` - The filename to store as (typically `<hash>.<ext>`)
    /// * `data` - The file data to store
    ///
    /// # Returns
    /// The full path to the cached file
    ///
    /// # Errors
    /// Returns error if filesystem operations fail
    pub(crate) async fn store_file(&self, filename: &str, data: &[u8]) -> Result<PathBuf> {
        let file_path = self.cache_dir.join(filename);

        // If file already exists with identical content, return early (deduplication)
        if file_path.exists()
            && let Ok(existing_data) = tokio::fs::read(&file_path).await
            && existing_data == data
        {
            tracing::debug!(
                target: "whitenoise::storage::media_files",
                "File already exists with same content: {}",
                file_path.display()
            );
            return Ok(file_path);
        }

        // Write atomically using NamedTempFile with unique temp filename
        // This prevents race conditions when multiple threads write to the same file
        let temp_file = NamedTempFile::new_in(&self.cache_dir).map_err(|e| {
            WhitenoiseError::MediaCache(format!("Failed to create temp file: {}", e))
        })?;

        std::fs::write(temp_file.path(), data).map_err(|e| {
            WhitenoiseError::MediaCache(format!("Failed to write file data: {}", e))
        })?;

        // Persist atomically renames the temp file to the final path
        temp_file.persist(&file_path).map_err(|e| {
            WhitenoiseError::MediaCache(format!("Failed to persist temp file: {}", e))
        })?;

        Ok(file_path)
    }

    /// Finds a file with a given prefix in the cache
    ///
    /// Useful when you know the hash but not the exact extension.
    ///
    /// # Arguments
    /// * `prefix` - The filename prefix to search for
    ///
    /// # Returns
    /// The path to the first matching file, if any
    pub(crate) async fn find_file_with_prefix(&self, prefix: &str) -> Option<PathBuf> {
        if !self.cache_dir.exists() {
            return None;
        }

        if let Ok(mut entries) = tokio::fs::read_dir(&self.cache_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(filename) = entry.file_name().to_str()
                    && filename.starts_with(prefix)
                {
                    return Some(entry.path());
                }
            }
        }

        None
    }

    /// Returns the cache directory path
    pub(crate) fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Removes all cached media files and the cache directory
    ///
    /// This is used when deleting all application data.
    /// The cache directory will be automatically recreated when needed.
    ///
    /// # Returns
    /// Ok(()) on success
    ///
    /// # Errors
    /// Returns error if filesystem operations fail
    pub(crate) async fn wipe_all(&self) -> Result<()> {
        if self.cache_dir.exists() {
            tracing::debug!(
                target: "whitenoise::storage::media_files",
                "Removing media cache directory: {:?}",
                self.cache_dir
            );
            tokio::fs::remove_dir_all(&self.cache_dir)
                .await
                .map_err(|e| {
                    WhitenoiseError::MediaCache(format!("Failed to remove cache directory: {}", e))
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_storage_creation() {
        let temp_dir = TempDir::new().unwrap();
        let storage = MediaFileStorage::new(temp_dir.path()).await.unwrap();

        assert!(storage.cache_dir().exists());
        assert_eq!(storage.cache_dir(), temp_dir.path().join("media_cache"));
    }

    #[tokio::test]
    async fn test_store_and_retrieve_file() {
        let temp_dir = TempDir::new().unwrap();
        let storage = MediaFileStorage::new(temp_dir.path()).await.unwrap();

        let test_data = b"test file content";

        // Store file
        let path = storage.store_file("test.txt", test_data).await.unwrap();

        // Verify it exists
        assert!(path.exists());

        // Verify content
        let content = tokio::fs::read(&path).await.unwrap();
        assert_eq!(content, test_data);
    }

    #[tokio::test]
    async fn test_find_file_with_prefix() {
        let temp_dir = TempDir::new().unwrap();
        let storage = MediaFileStorage::new(temp_dir.path()).await.unwrap();

        // Store files with different extensions
        storage
            .store_file("abc123.jpg", b"jpeg data")
            .await
            .unwrap();
        storage.store_file("abc123.png", b"png data").await.unwrap();
        storage
            .store_file("def456.jpg", b"other jpeg")
            .await
            .unwrap();

        // Find by prefix
        let found = storage.find_file_with_prefix("abc123").await.unwrap();
        assert!(found.to_string_lossy().contains("abc123"));

        // Non-existent prefix
        assert!(storage.find_file_with_prefix("xyz").await.is_none());
    }

    #[tokio::test]
    async fn test_atomic_write() {
        let temp_dir = TempDir::new().unwrap();
        let storage = MediaFileStorage::new(temp_dir.path()).await.unwrap();

        // Store file
        let path = storage.store_file("atomic.txt", b"data").await.unwrap();

        // Verify actual file exists
        assert!(path.exists());

        // Verify no temp files left behind (NamedTempFile uses random names)
        let entries: Vec<_> = std::fs::read_dir(&storage.cache_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        // Should only have the one file we created
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name(), "atomic.txt");
    }

    #[tokio::test]
    async fn test_concurrent_writes_same_file() {
        let temp_dir = TempDir::new().unwrap();
        let storage = std::sync::Arc::new(MediaFileStorage::new(temp_dir.path()).await.unwrap());

        // Spawn multiple concurrent writes to the same file
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let storage = storage.clone();
                tokio::spawn(async move {
                    let data = format!("data from thread {}", i);
                    storage.store_file("same_file.txt", data.as_bytes()).await
                })
            })
            .collect();

        // Wait for all writes to complete
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        // File should exist and contain data from one of the writes
        let path = storage
            .find_file_with_prefix("same_file.txt")
            .await
            .unwrap();
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("data from thread "));

        // Verify no temp files left behind
        let entries: Vec<_> = std::fs::read_dir(&storage.cache_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        // Should only have the one file we created
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn test_deduplication() {
        let temp_dir = TempDir::new().unwrap();
        let storage = MediaFileStorage::new(temp_dir.path()).await.unwrap();

        let test_data = b"shared content";

        // Store the same file twice
        let path1 = storage.store_file("shared.txt", test_data).await.unwrap();

        let path2 = storage.store_file("shared.txt", test_data).await.unwrap();

        // Should be the same path
        assert_eq!(path1, path2);

        // File should exist with correct content
        let content = tokio::fs::read(&path1).await.unwrap();
        assert_eq!(content, test_data);
    }

    #[tokio::test]
    async fn test_wipe_all() {
        let temp_dir = TempDir::new().unwrap();
        let storage = MediaFileStorage::new(temp_dir.path()).await.unwrap();

        // Store some test files
        storage
            .store_file("test1.jpg", b"image data 1")
            .await
            .unwrap();
        storage
            .store_file("test2.png", b"image data 2")
            .await
            .unwrap();
        storage
            .store_file("test3.mp4", b"video data")
            .await
            .unwrap();

        // Verify cache directory exists and has files
        assert!(storage.cache_dir().exists());
        let entries: Vec<_> = std::fs::read_dir(storage.cache_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 3);

        // Wipe all
        storage.wipe_all().await.unwrap();

        // Cache directory should be removed
        assert!(!storage.cache_dir().exists());

        // Calling wipe_all again should not error (idempotent)
        storage.wipe_all().await.unwrap();
    }
}
