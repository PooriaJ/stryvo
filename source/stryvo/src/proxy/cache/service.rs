// Implements the core caching logic for the Stryvo proxy, managing in-memory cache storage,
// persistence, and cleanup of orphaned files. Provides cache lookup, storage, and eviction
// with support for compression and configurable TTL.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::Session;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::fs::{create_dir_all, read, rename, write};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use super::persistence::{CachePersistence, PersistenceConfig};
use crate::proxy::compression::CompressionStrategy;
use crate::proxy::service_registry::BackgroundService;

pub const DEFAULT_CACHE_TTL: u64 = 3600;

pub enum CacheResult {
    Hit {
        body: Bytes,
        headers: Vec<(String, String)>,
        status: u16,
    },
    Miss,
}

pub struct SccMemoryCache {
    pub cache: Mutex<HashMap<String, CacheMetadata>>,
    pub capacity: usize,
    pub compression: CompressionStrategy,
    pub ttl: u64,
    pub max_file_size: Option<usize>,
    pub persistence: Option<PersistenceConfig>,
    pub cache_dir: PathBuf,
    pub shard_count: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CacheMetadata {
    pub headers: Vec<(String, String)>,
    pub status: u16,
    #[serde(serialize_with = "serialize_instant", deserialize_with = "deserialize_instant")]
    pub created_at: Instant,
    pub ttl: u64,
    pub body_path: PathBuf,
    pub body_size: usize,
}

fn serialize_instant<S>(instant: &Instant, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let duration_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(serde::ser::Error::custom)?;
    let instant_duration = instant
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
    let timestamp = duration_since_epoch
        .checked_sub(instant_duration)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    serializer.serialize_u64(timestamp)
}

fn deserialize_instant<'de, D>(deserializer: D) -> Result<Instant, D::Error>
where
    D: Deserializer<'de>,
{
    let timestamp = u64::deserialize(deserializer)?;
    let duration_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(serde::de::Error::custom)?;
    let instant_duration = duration_since_epoch
        .checked_sub(Duration::from_secs(timestamp))
        .ok_or_else(|| serde::de::Error::custom("Invalid timestamp"))?;
    Ok(Instant::now() - instant_duration)
}

impl SccMemoryCache {
    pub fn with_capacity(capacity: usize, cache_dir: PathBuf, shard_count: usize, max_file_size: Option<usize>) -> Self {
        Self {
            cache: Mutex::new(HashMap::with_capacity(capacity)),
            capacity,
            compression: CompressionStrategy::default(),
            ttl: DEFAULT_CACHE_TTL,
            max_file_size,
            persistence: None,
            cache_dir,
            shard_count,
        }
    }

    pub fn with_compression(mut self, compression: CompressionStrategy) -> Self {
        self.compression = compression;
        self
    }

    pub fn with_ttl(mut self, ttl: u64) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn with_persistence(mut self, persistence_config: PersistenceConfig) -> Self {
        info!("Configuring cache persistence with path: {:?}", persistence_config.cache_dir);
        self.persistence = Some(persistence_config.clone());
        self.cache_dir = persistence_config.cache_dir;
        self.shard_count = persistence_config.shard_count;
        self
    }

    fn generate_cache_key(&self, req_header: &RequestHeader) -> String {
        format!("{} {}", req_header.method.as_str(), req_header.uri.to_string())
    }

    pub fn get_cache_file_path(&self, cache_key: &str) -> PathBuf {
        let hash = {
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(cache_key.as_bytes());
            hasher.finalize()
        };
        let shard_id = (hash % self.shard_count as u32) as usize;
        let file_name = format!("{:x}.bin", hash);
        self.cache_dir.join(format!("shard-{:02}", shard_id)).join(file_name)
    }

    async fn ensure_cache_dirs(&self) -> Result<(), String> {
        if let Err(e) = create_dir_all(&self.cache_dir).await {
            return Err(format!("Failed to create cache directory: {}", e));
        }
        for i in 0..self.shard_count {
            let shard_dir = self.cache_dir.join(format!("shard-{:02}", i));
            if let Err(e) = create_dir_all(&shard_dir).await {
                return Err(format!("Failed to create shard directory: {}", e));
            }
        }
        Ok(())
    }

    async fn handle_persistence_after_removal(&self, cache_key: &str, bucket: Option<&CacheBucket>) {
        if let (Some(persistence_config), Some(bucket)) = (&self.persistence, bucket) {
            let persistence = CachePersistence::new(bucket.clone(), persistence_config.clone());
            
            if let Err(e) = persistence.store_immediate().await {
                error!("Failed to persist metadata after removing entry: {}", e);
            }
            
            if let Err(e) = persistence.clean_stale_metadata_with_key(cache_key).await {
                error!("Failed to clean stale metadata for key {}: {}", cache_key, e);
            }
        }
    }

    pub async fn get_cache(&self, req_header: &RequestHeader, bucket: Option<&CacheBucket>) -> Result<CacheResult, String> {
        let cache_key = self.generate_cache_key(req_header);
        debug!("Cache lookup with key: {}", cache_key);

        let metadata_result = {
            let cache = self.cache.lock().await;
            cache.get(&cache_key).cloned()
        };

        if let Some(metadata) = &metadata_result {
            if metadata.created_at.elapsed().as_secs() > metadata.ttl {
                debug!("Cache entry expired for key: {}", cache_key);
                
                let expired_path = metadata.body_path.clone();
                {
                    let mut cache = self.cache.lock().await;
                    cache.remove(&cache_key);
                }
                
                if expired_path.exists() {
                    if let Err(e) = tokio::fs::remove_file(&expired_path).await {
                        error!("Failed to delete expired cache file {}: {}", expired_path.display(), e);
                    }
                }
                
                self.handle_persistence_after_removal(&cache_key, bucket).await;
                
                return Ok(CacheResult::Miss);
            }
        }

        if metadata_result.is_none() {
            let body_path = self.get_cache_file_path(&cache_key);
            if body_path.exists() {
                debug!("Deleting orphaned cache file: {}", body_path.display());
                if let Err(e) = tokio::fs::remove_file(&body_path).await {
                    error!("Failed to delete orphaned cache file {}: {}", body_path.display(), e);
                    warn!("Continuing after failed deletion of orphaned cache file");
                } else {
                    debug!("Successfully deleted orphaned cache file: {}", body_path.display());
                }

                if let (Some(persistence_config), Some(bucket)) = (&self.persistence, bucket) {
                    let persistence = CachePersistence::new(bucket.clone(), persistence_config.clone());
                    debug!("Triggering immediate metadata persistence after purging key: {}", cache_key);
                    if let Err(e) = persistence.store_immediate().await {
                        error!("Failed to persist metadata after purging orphaned entry: {}", e);
                    } else {
                        debug!("Successfully persisted metadata after purging orphaned entry: {}", cache_key);
                    }
                    debug!("Triggering immediate cleanup for key: {}", cache_key);
                    if let Err(e) = persistence.clean_stale_metadata_with_key(&cache_key).await {
                        error!("Failed to clean stale metadata for key {}: {}", cache_key, e);
                    } else {
                        info!("Post-purge cleanup completed for key: {}", cache_key);
                    }
                }

                return Ok(CacheResult::Miss);
            } else {
                debug!("No orphaned cache file found for key: {}", cache_key);
            }
        }

        if let Some(metadata) = metadata_result {
            let body_path = metadata.body_path.clone();
            
            if !body_path.exists() {
                debug!("Cache file missing for key: {}, removing from cache", cache_key);
                {
                    let mut cache = self.cache.lock().await;
                    cache.remove(&cache_key);
                }
                
                self.handle_persistence_after_removal(&cache_key, bucket).await;
                return Ok(CacheResult::Miss);
            }
            
            let body_bytes = match read(&body_path).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    error!("Failed to read cache file {}: {}", body_path.display(), e);
                    
                    {
                        let mut cache = self.cache.lock().await;
                        cache.remove(&cache_key);
                    }
                    
                    self.handle_persistence_after_removal(&cache_key, bucket).await;
                    return Ok(CacheResult::Miss);
                }
            };
            
            let body = match self.compression.decompress(&Bytes::from(body_bytes)) {
                Ok(body) => body,
                Err(e) => {
                    error!("Failed to decompress cache file {}: {}", body_path.display(), e);
                    
                    {
                        let mut cache = self.cache.lock().await;
                        cache.remove(&cache_key);
                    }
                    
                    if let Err(del_err) = tokio::fs::remove_file(&body_path).await {
                        error!("Failed to delete corrupted cache file {}: {}", body_path.display(), del_err);
                    }
                    
                    self.handle_persistence_after_removal(&cache_key, bucket).await;
                    return Ok(CacheResult::Miss);
                }
            };
            
            return Ok(CacheResult::Hit {
                body,
                headers: metadata.headers.clone(),
                status: metadata.status,
            });
        }

        debug!("Cache miss for key: {}", cache_key);
        Ok(CacheResult::Miss)
    }

    pub async fn put_cache(
        &self,
        req_header: &RequestHeader,
        resp_header: &ResponseHeader,
        body: Bytes,
        bucket: Option<&CacheBucket>,
    ) -> Result<(), String> {
        if let Some(max_size) = self.max_file_size {
            if body.len() > max_size {
                debug!("Skipping cache store for key: {} due to body size {} > {}", 
                       req_header.uri, body.len(), max_size);
                return Ok(());
            }
        }
        
        let cache_key = self.generate_cache_key(req_header);
        debug!("Storing in cache with key: {}", cache_key);

        let skip_cache = resp_header.headers.get("cache-control").map_or(false, |cc| {
            cc.to_str().map_or(false, |cc| {
                let cc = cc.to_lowercase();
                cc.contains("no-store")
            })
        });
        
        if skip_cache {
            debug!("Skipping cache store for key: {} due to cache-control: no-store", cache_key);
            return Ok(());
        }

        let headers = resp_header
            .headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
            
        let content_type = resp_header
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok());
            
        let compressed_body = if self.compression.should_compress(content_type) {
            match self.compression.compress(body.clone()) {
                Ok(compressed) => compressed,
                Err(e) => {
                    error!("Failed to compress body: {}", e);
                    return Err(format!("Failed to compress body: {}", e));
                }
            }
        } else {
            body.clone()
        };
        
        if let Err(e) = self.ensure_cache_dirs().await {
            error!("Failed to ensure cache directories: {}", e);
            return Err(e);
        }
        
        let body_path = self.get_cache_file_path(&cache_key);
        let temp_path = body_path.with_extension("tmp");
        
        if let Err(e) = write(&temp_path, &compressed_body).await {
            error!("Failed to write cache file: {}", e);
            return Err(format!("Failed to write cache file: {}", e));
        }
        
        if let Err(e) = rename(&temp_path, &body_path).await {
            error!("Failed to rename cache file: {}", e);
            if let Err(cleanup_err) = tokio::fs::remove_file(&temp_path).await {
                debug!("Failed to clean up temp file {}: {}", temp_path.display(), cleanup_err);
            }
            return Err(format!("Failed to rename cache file: {}", e));
        }

        let metadata = CacheMetadata {
            headers,
            status: resp_header.status.as_u16(),
            created_at: Instant::now(),
            ttl: self.ttl,
            body_path: body_path.clone(),
            body_size: compressed_body.len(),
        };

        let evicted_file_path = {
            let mut cache = self.cache.lock().await;
            let mut evicted_file_path = None;
            
            if cache.len() >= self.capacity && !cache.contains_key(&cache_key) {
                if let Some(oldest) = cache.iter().min_by_key(|(_, e)| e.created_at) {
                    let key = oldest.0.clone();
                    evicted_file_path = Some(oldest.1.body_path.clone());
                    cache.remove(&key);
                    debug!("Evicted cache entry: {}", key);
                }
            }
            
            cache.insert(cache_key.clone(), metadata);
            evicted_file_path
        };

        if let Some(file_path) = evicted_file_path {
            if file_path.exists() {
                match tokio::fs::remove_file(&file_path).await {
                    Ok(_) => debug!("Successfully deleted evicted cache file: {}", file_path.display()),
                    Err(e) => {
                        error!("Failed to delete evicted cache file {}: {}", file_path.display(), e);
                        warn!("Continuing after failed deletion of evicted cache file");
                    }
                }
            }
            
            self.handle_persistence_after_removal(&cache_key, bucket).await;
        }

        if let (Some(persistence_config), Some(bucket)) = (&self.persistence, bucket) {
            debug!("Triggering immediate metadata persistence for key: {}", cache_key);
            let persistence = CachePersistence::new(bucket.clone(), persistence_config.clone());
            if let Err(e) = persistence.store_immediate().await {
                error!("Failed to persist metadata after cache update: {}", e);
            } else {
                debug!("Successfully persisted metadata after cache update for key: {}", cache_key);
            }
        }

        debug!("Successfully stored in cache, size: {}", body.len());
        Ok(())
    }
}

#[derive(Clone)]
pub struct CacheBucket {
    storage: Arc<SccMemoryCache>,
    persistence_config: Option<PersistenceConfig>,
}

impl CacheBucket {
    pub fn new(storage: SccMemoryCache) -> Self {
        let persistence_config = storage.persistence.clone();
        Self {
            storage: Arc::new(storage),
            persistence_config,
        }
    }

    pub fn storage(&self) -> Arc<SccMemoryCache> {
        self.storage.clone()
    }

    pub fn enable(&self, _session: &mut Session) {}

    pub fn start_persistence_service(&self) -> Option<JoinHandle<()>> {
        if self.persistence_config.is_none() {
            warn!("No persistence configuration found, skipping persistence service");
            return None;
        }
        let config = self.persistence_config.as_ref().unwrap().clone();
        let cache_dir = config.cache_dir.clone();
        let metadata_dir = config.metadata_dir.clone();

        if let Some(parent) = cache_dir.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!("Failed to create cache directory: {}", e);
                return None;
            }
        }

        if let Err(e) = std::fs::create_dir_all(&metadata_dir) {
            warn!("Failed to create metadata directory: {}", e);
            return None;
        }

        debug!(
            "Cache persistence service configured with cache_dir: {:?}, metadata_dir: {:?}",
            cache_dir, metadata_dir
        );

        let bucket_clone = self.clone();
        tokio::spawn(async move {
            if let Err(e) = bucket_clone.initialize_cache_dirs().await {
                warn!("Failed to initialize cache directories: {}", e);
            }
        });

        let service = CachePersistence::new(self.clone(), config);
        let service_clone = service.clone();
        tokio::spawn(async move {
            info!("Forcing initial metadata store operation");
            if let Err(e) = service_clone.store().await {
                error!("Failed to perform initial metadata store: {}", e);
            } else {
                info!("Initial metadata store completed successfully");
            }
        });

        if let Some(cleanup_handle) = self.orphaned_files_cleanup() {
            info!("Started orphaned files cleanup task");
            tokio::spawn(async move {
                cleanup_handle.await.unwrap_or_else(|e| {
                    error!("Orphaned files cleanup task failed: {}", e);
                });
            });
        }

        Some(service.start_service())
    }

    pub async fn initialize_cache_dirs(&self) -> Result<(), String> {
        self.storage.ensure_cache_dirs().await
    }

    pub fn orphaned_files_cleanup(&self) -> Option<JoinHandle<()>> {
        if self.persistence_config.is_none() {
            return None;
        }

        let bucket = self.clone();
        let interval = Duration::from_secs(60);
        
        info!("Orphaned files cleanup task initialized");
        Some(tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(interval);
            loop {
                interval_timer.tick().await;
                debug!("Starting orphaned cache files cleanup");

                if let Some(config) = &bucket.persistence_config {
                    let storage = bucket.storage();
                    
                    let valid_paths = {
                        let cache = storage.cache.lock().await;
                        cache.values()
                            .map(|meta| meta.body_path.clone())
                            .collect::<HashSet<_>>()
                    };

                    let mut deleted_count = 0;
                    let mut error_count = 0;

                    let cache_dir = &config.cache_dir;
                    let entries = match std::fs::read_dir(cache_dir) {
                        Ok(entries) => entries,
                        Err(e) => {
                            error!("Failed to read cache directory {}: {}", cache_dir.display(), e);
                            continue;
                        }
                    };

                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() && path.file_name().map_or(false, |name| name.to_string_lossy().starts_with("shard-")) {
                            debug!("Scanning shard directory: {}", path.display());
                            let shard_entries = match std::fs::read_dir(&path) {
                                Ok(entries) => entries,
                                Err(e) => {
                                    error!("Failed to read shard directory {}: {}", path.display(), e);
                                    error_count += 1;
                                    continue;
                                }
                            };

                            for shard_entry in shard_entries.flatten() {
                                let file_path = shard_entry.path();
                                if file_path.is_file() && file_path.extension().map_or(false, |ext| ext == "bin") {
                                    if !valid_paths.contains(&file_path) {
                                        debug!("Found orphaned cache file: {}", file_path.display());
                                        match tokio::fs::remove_file(&file_path).await {
                                            Ok(_) => {
                                                debug!("Successfully deleted orphaned cache file: {}", file_path.display());
                                                deleted_count += 1;
                                            }
                                            Err(e) => {
                                                debug!("Failed to delete orphaned cache file {}: {}", file_path.display(), e);
                                                error_count += 1;
                                            }
                                        }
                                    }
                                }
                                
                                if file_path.is_file() && file_path.extension().map_or(false, |ext| ext == "tmp") {
                                    debug!("Found stray temporary file: {}", file_path.display());
                                    match tokio::fs::remove_file(&file_path).await {
                                        Ok(_) => {
                                            debug!("Successfully deleted stray temporary file: {}", file_path.display());
                                            deleted_count += 1;
                                        }
                                        Err(e) => {
                                            debug!("Failed to delete stray temporary file {}: {}", file_path.display(), e);
                                            error_count += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    info!(
                        "Orphaned cache files cleanup complete. Deleted {} files, {} errors", 
                        deleted_count, error_count
                    );
                }
            }
        }))
    }
}

pub struct CacheService {
    bucket: Arc<CacheBucket>,
    name: String,
}

impl CacheService {
    pub fn new(bucket: CacheBucket, name: String) -> Self {
        Self {
            bucket: Arc::new(bucket),
            name,
        }
    }

    pub fn bucket(&self) -> &CacheBucket {
        &self.bucket
    }

    pub fn enable(&self, session: &mut Session) {
        self.bucket.enable(session);
    }

    pub fn start_persistence_service(&self) -> Option<JoinHandle<()>> {
        self.bucket.start_persistence_service()
    }

    pub fn is_cacheable(req_header: &RequestHeader, resp_header: &ResponseHeader) -> bool {
        if req_header.method != "GET" {
            tracing::debug!("Not cacheable: method is {}", req_header.method.as_str());
            return false;
        }

        if let Some(cache_control) = resp_header.headers.get("cache-control") {
            if let Ok(cc) = cache_control.to_str() {
                let cc = cc.to_lowercase();
                if cc.contains("no-store") || cc.contains("private") {
                    tracing::debug!("Not cacheable: Cache-Control contains {}", cc);
                    return false;
                }
                if cc.contains("no-cache") || cc.contains("max-age=0") {
                    tracing::debug!("Not cacheable: Cache-Control contains {}", cc);
                    return false;
                }
            }
        }

        true
    }
}

impl BackgroundService for CacheService {
    fn start(&self) -> JoinHandle<()> {
        let service_name = self.name.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            info!(
                "Starting cache service '{}' in existing Tokio runtime",
                service_name
            );
            if let Some(persistence_handle) = self.start_persistence_service() {
                info!("Cache persistence service '{}' started", service_name);
                return persistence_handle;
            }
            handle.spawn(async move {
                info!("Cache service '{}' started in memory-only mode", service_name);
            })
        } else {
            warn!("No Tokio runtime detected for cache service '{}'", service_name);
            warn!("The ServiceRegistry should provide a runtime - this is likely a bug");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .expect("Failed to create minimal runtime");
            rt.spawn(async {})
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}