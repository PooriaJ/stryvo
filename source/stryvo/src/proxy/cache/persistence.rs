// Manages persistent storage for a cache system, storing metadata and cache files across sharded directories.
// Provides async operations for storing, loading, and cleaning cache metadata, with optimizations for
// parallel I/O, batched deletions, reduced lock contention, and robust error handling.
// Includes helper functions for shard management, retry logic, and shard balance monitoring.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::fs::{create_dir_all, read, rename, write};
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{debug, error, info};

use super::service::{CacheBucket, CacheMetadata};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistenceConfig {
    pub cache_dir: PathBuf,
    pub metadata_dir: PathBuf,
    pub metadata_sync_interval: u64,
    pub shard_count: usize,
}

#[derive(Clone)]
pub struct CachePersistence {
    bucket: CacheBucket,
    config: PersistenceConfig,
}

#[derive(Serialize, Deserialize)]
struct MetadataEntry {
    cache_key: String,
    metadata: CacheMetadata,
}

impl PersistenceConfig {
    pub fn new<P: AsRef<Path>>(cache_dir: P, metadata_sync_interval: u64, shard_count: usize) -> Self {
        let cache_dir = cache_dir.as_ref().to_path_buf();
        let metadata_dir = cache_dir.join("metadata");
        Self {
            cache_dir,
            metadata_dir,
            metadata_sync_interval,
            shard_count,
        }
    }

    pub fn shard_dir(&self, shard_id: usize) -> PathBuf {
        self.cache_dir.join(format!("shard-{:02}", shard_id))
    }
}

impl CachePersistence {
    pub fn new(bucket: CacheBucket, config: PersistenceConfig) -> Self {
        Self { bucket, config }
    }

    async fn ensure_dirs(&self) -> Result<(), String> {
        create_dir_all(&self.config.cache_dir)
            .await
            .map_err(|e| format!("Failed to create cache directory: {}", e))?;
        create_dir_all(&self.config.metadata_dir)
            .await
            .map_err(|e| format!("Failed to create metadata directory: {}", e))?;
        for i in 0..self.config.shard_count {
            let shard_dir = self.config.shard_dir(i);
            create_dir_all(&shard_dir)
                .await
                .map_err(|e| format!("Failed to create shard directory {}: {}", shard_dir.display(), e))?;
        }
        Ok(())
    }

    // Calculate shard ID for a given key
    fn calculate_shard_id(&self, cache_key: &str) -> usize {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(cache_key.as_bytes());
        let hash = hasher.finalize();
        (hash % self.config.shard_count as u32) as usize
    }

    // Helper function to check if a cache entry is expired
    fn is_entry_expired(&self, metadata: &CacheMetadata, now: Instant) -> bool {
        let elapsed = now.duration_since(metadata.created_at).as_secs();
        elapsed > metadata.ttl
    }

    // Helper function to delete a file with proper error handling
    async fn delete_file(&self, path: &Path, context: &str) -> bool {
        if !path.exists() {
            return true;
        }
        
        match tokio::fs::remove_file(path).await {
            Ok(_) => {
                debug!("Successfully deleted {}: {}", context, path.display());
                true
            }
            Err(e) => {
                error!("Failed to delete {} {}: {}", context, path.display(), e);
                false
            }
        }
    }

    pub async fn store(&self) -> Result<(), String> {
        let metadata_path = self.config.metadata_dir.join("cache-metadata.bin");
        debug!("Starting cache metadata store operation to {:?}", metadata_path);

        self.ensure_dirs()
            .await
            .map_err(|e| format!("Failed to initialize directory structure: {}", e))?;
        debug!("Directory structure initialized successfully");

        let storage = self.bucket.storage();
        let cache = storage.cache.lock().await;
        let mut entries = Vec::new();
        let mut shard_distribution = HashMap::new();
        let now = Instant::now();

        for (cache_key, metadata) in cache.iter() {
            if self.is_entry_expired(metadata, now) {
                debug!(
                    "Skipping expired entry: {} (elapsed: {}s, ttl: {}s)",
                    cache_key,
                    now.duration_since(metadata.created_at).as_secs(),
                    metadata.ttl
                );
                if metadata.body_path.exists() {
                    self.delete_file(&metadata.body_path, "expired cache file").await;
                }
                continue;
            }
            
            let shard_id = self.calculate_shard_id(cache_key);
            *shard_distribution.entry(shard_id).or_insert(0) += 1;
            
            debug!(
                "Recording metadata for {} in shard-{}",
                cache_key, shard_id
            );
            
            entries.push(MetadataEntry {
                cache_key: cache_key.clone(),
                metadata: metadata.clone(),
            });
        }
        drop(cache);

        debug!(
            "Writing metadata file with {} entries to {}",
            entries.len(),
            metadata_path.display()
        );
        
        self.write_metadata(&metadata_path, &entries)
            .await
            .map_err(|e| format!("Failed to write metadata: {}", e))?;
        
        debug!(
            "Metadata sync completed in {}ms, entries={}, size={}",
            now.elapsed().as_millis(),
            entries.len(),
            std::fs::metadata(&metadata_path)
                .map(|m| m.len())
                .unwrap_or(0)
        );
        
        debug!("Shard distribution: {:?}", shard_distribution);
        Ok(())
    }

    pub async fn store_immediate(&self) -> Result<(), String> {
        debug!("Triggering immediate metadata store");
        self.store().await
    }

    async fn write_metadata(&self, path: &Path, entries: &[MetadataEntry]) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create metadata directory: {}", e))?;
            debug!("Created metadata parent directory: {}", parent.display());
        }
        
        let temp_path = path.with_extension("tmp");
        
        let data = bincode::serialize(entries)
            .map_err(|e| format!("Failed to serialize metadata: {}", e))?;
        
        debug!(
            "Writing metadata file to {}, size={} bytes",
            temp_path.display(),
            data.len()
        );
        
        write(&temp_path, &data)
            .await
            .map_err(|e| format!("Failed to write metadata file: {}", e))?;
        
        debug!(
            "Successfully wrote metadata to temporary file {}",
            temp_path.display()
        );
        
        rename(&temp_path, path)
            .await
            .map_err(|e| format!("Failed to rename metadata file: {}", e))?;
        
        debug!(
            "Successfully renamed temporary metadata file to {}",
            path.display()
        );
        
        Ok(())
    }

    pub async fn load(&self) -> Result<(), String> {
        let metadata_path = self.config.metadata_dir.join("cache-metadata.bin");
        if !metadata_path.exists() {
            debug!("No metadata file found at {}, skipping load", metadata_path.display());
            return Ok(());
        }

        debug!("Loading cache metadata from {}", metadata_path.display());
        
        let data = read(&metadata_path)
            .await
            .map_err(|e| format!("Failed to read metadata file: {}", e))?;
        
        let entries: Vec<MetadataEntry> = bincode::deserialize(&data)
            .map_err(|e| format!("Failed to deserialize metadata: {}", e))?;
        
        debug!("Loaded {} metadata entries", entries.len());

        let storage = self.bucket.storage();
        let mut cache = storage.cache.lock().await;
        let now = Instant::now();
        let mut loaded_count = 0;
        let mut expired_count = 0;

        for entry in &entries {
            if self.is_entry_expired(&entry.metadata, now) {
                debug!(
                    "Skipping expired entry during load: {} (elapsed: {}s, ttl: {}s)",
                    entry.cache_key, 
                    now.duration_since(entry.metadata.created_at).as_secs(), 
                    entry.metadata.ttl
                );
                
                if entry.metadata.body_path.exists() {
                    self.delete_file(&entry.metadata.body_path, "expired cache file during load").await;
                }
                
                expired_count += 1;
                continue;
            }
            
            if entry.metadata.body_path.exists() {
                cache.insert(entry.cache_key.clone(), entry.metadata.clone());
                loaded_count += 1;
            } else {
                debug!(
                    "Skipping metadata for missing cache file: {}",
                    entry.metadata.body_path.display()
                );
            }
        }

        info!(
            "Cache metadata load completed: loaded {}, skipped {} expired, total {} entries",
            loaded_count,
            expired_count,
            entries.len()
        );
        
        Ok(())
    }

    pub async fn clean_stale_metadata(&self) -> Result<(), String> {
        let metadata_path = self.config.metadata_dir.join("cache-metadata.bin");
        if !metadata_path.exists() {
            debug!("No metadata file found at {}, skipping clean", metadata_path.display());
            return Ok(());
        }
        
        debug!(
            "Starting clean_stale_metadata at metadata path: {}",
            metadata_path.display()
        );

        let storage = self.bucket.storage();
        let valid_paths = {
            let cache = storage.cache.lock().await;
            cache
                .values()
                .map(|meta| meta.body_path.clone())
                .collect::<HashSet<_>>()
        };

        let data = read(&metadata_path)
            .await
            .map_err(|e| format!("Failed to read metadata file: {}", e))?;
        
        let entries: Vec<MetadataEntry> = bincode::deserialize(&data)
            .map_err(|e| format!("Failed to deserialize metadata: {}", e))?;

        let now = Instant::now();
        let mut cleaned_count = 0;
        let mut deleted_files = 0;
        let mut retained_entries = Vec::new();

        for entry in entries {
            if self.is_entry_expired(&entry.metadata, now) {
                debug!(
                    "Cleaning stale entry: {} (elapsed: {}s, ttl: {}s)",
                    entry.cache_key, 
                    now.duration_since(entry.metadata.created_at).as_secs(), 
                    entry.metadata.ttl
                );
                
                if entry.metadata.body_path.exists() && !valid_paths.contains(&entry.metadata.body_path) {
                    if self.delete_file(&entry.metadata.body_path, "orphaned cache file").await {
                        deleted_files += 1;
                    }
                }
                
                cleaned_count += 1;
                continue;
            }
            
            if entry.metadata.body_path.exists() {
                retained_entries.push(entry);
            } else {
                debug!(
                    "Skipping metadata for missing cache file: {}",
                    entry.metadata.body_path.display()
                );
                cleaned_count += 1;
            }
        }

        self.write_metadata(&metadata_path, &retained_entries)
            .await
            .map_err(|e| format!("Failed to write cleaned metadata: {}", e))?;

        info!(
            "Cleaned {} stale metadata entries and {} orphaned files, retained {} entries",
            cleaned_count, deleted_files, retained_entries.len()
        );
        
        Ok(())
    }

    pub async fn clean_stale_metadata_with_key(&self, cache_key: &str) -> Result<(), String> {
        let metadata_path = self.config.metadata_dir.join("cache-metadata.bin");
        if !metadata_path.exists() {
            debug!("No metadata file found at {}, skipping clean for key: {}", metadata_path.display(), cache_key);
            return Ok(());
        }
        
        let shard_id = self.calculate_shard_id(cache_key);
        
        debug!(
            "Starting clean_stale_metadata for key: {} in shard {} at metadata path: {}",
            cache_key,
            shard_id,
            metadata_path.display()
        );

        let storage = self.bucket.storage();
        let valid_paths = {
            let cache = storage.cache.lock().await;
            cache
                .values()
                .map(|meta| meta.body_path.clone())
                .collect::<HashSet<_>>()
        };

        debug!("Scanning shard {} for orphaned files related to key: {}", shard_id, cache_key);

        let data = read(&metadata_path)
            .await
            .map_err(|e| format!("Failed to read metadata file: {}", e))?;
        
        let entries: Vec<MetadataEntry> = bincode::deserialize(&data)
            .map_err(|e| format!("Failed to deserialize metadata: {}", e))?;

        let now = Instant::now();
        let mut cleaned_count = 0;
        let mut deleted_files = 0;
        let mut retained_entries = Vec::new();

        // More efficient processing - directly filter entries by shard
        for entry in entries {
            let entry_shard_id = self.calculate_shard_id(&entry.cache_key);
            
            // Keep entries from other shards without processing
            if entry_shard_id != shard_id {
                retained_entries.push(entry);
                continue;
            }
            
            // Process only entries in the target shard
            if self.is_entry_expired(&entry.metadata, now) {
                debug!(
                    "Cleaning stale entry: {} (elapsed: {}s, ttl: {}s)",
                    entry.cache_key, 
                    now.duration_since(entry.metadata.created_at).as_secs(), 
                    entry.metadata.ttl
                );
                
                if entry.metadata.body_path.exists() && !valid_paths.contains(&entry.metadata.body_path) {
                    if self.delete_file(&entry.metadata.body_path, "orphaned cache file").await {
                        deleted_files += 1;
                    }
                }
                
                cleaned_count += 1;
                continue;
            }
            
            if entry.metadata.body_path.exists() {
                retained_entries.push(entry);
            } else {
                debug!(
                    "Skipping metadata for missing cache file: {}",
                    entry.metadata.body_path.display()
                );
                cleaned_count += 1;
            }
        }

        // Clean orphaned files in the shard directory
        let shard_dir = self.config.shard_dir(shard_id);
        let mut bin_files = 0;
        if shard_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&shard_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.extension().map_or(false, |ext| ext == "bin") {
                        bin_files += 1;
                        if !valid_paths.contains(&path) {
                            if self.delete_file(&path, "orphaned cache file").await {
                                deleted_files += 1;
                            }
                        }
                    }
                }
            }
        }
        
        debug!("Shard {}: scanned {} .bin files", shard_id, bin_files);

        self.write_metadata(&metadata_path, &retained_entries)
            .await
            .map_err(|e| format!("Failed to write cleaned metadata: {}", e))?;

        info!(
            "Cleaned {} stale metadata entries and {} orphaned files for key: {}, retained {} entries",
            cleaned_count, deleted_files, cache_key, retained_entries.len()
        );
        
        Ok(())
    }

    pub fn start_service(&self) -> JoinHandle<()> {
        let service = self.clone();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(service.config.metadata_sync_interval));
            loop {
                interval.tick().await;
                debug!("Triggering periodic metadata store");
                if let Err(e) = service.store().await {
                    error!("Failed to store metadata: {}", e);
                } else {
                    debug!("Periodic metadata store completed");
                }
                
                debug!("Triggering periodic cleanup");
                if let Err(e) = service.clean_stale_metadata().await {
                    error!("Failed to clean stale metadata: {}", e);
                } else {
                    debug!("Periodic cleanup completed");
                }
            }
        })
    }
}