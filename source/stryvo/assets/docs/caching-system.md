# Caching System Documentation

## Overview
The Stryvo caching system provides high-performance, disk-based caching with sharding, hashing, metadata persistence, and intelligent cache policies. The system is built around the `SccMemoryCache` structure with file system persistence and automatic cleanup.

## Technical Specifications

### Core Components

#### SccMemoryCache (Main Cache Engine)
```rust
pub struct SccMemoryCache {
    cache: Arc<Mutex<HashMap<String, CacheMetadata>>>,
    cache_dir: PathBuf,
    shard_count: usize,
    capacity: usize,
    ttl: Duration,
    compression: CompressionManager,
    persistence: Option<PersistenceConfig>,
}
```

#### Cache Metadata
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMetadata {
    pub headers: HashMap<String, String>,
    pub status: u16,
    pub created_at: Instant,
    pub ttl: Duration,
    pub body_path: PathBuf,
    pub body_size: usize,
}
```

#### Cache Bucket (Service Wrapper)
```rust
#[derive(Clone)]
pub struct CacheBucket {
    storage: Arc<SccMemoryCache>,
    persistence_config: Option<PersistenceConfig>,
}
```

#### Persistence Configuration
```rust
#[derive(Debug, Clone)]
pub struct PersistenceConfig {
    pub cache_dir: PathBuf,
    pub metadata_dir: PathBuf,
    pub cleanup_interval: Duration,
    pub max_file_age: Duration,
}
```

## Cache Flow Diagram

```
HTTP Request
     ↓
┌─────────────────┐
│   Cache Lookup  │
│ generate_cache_ │
│     key()       │
└─────────────────┘
     ↓
┌─────────────────┐
│  Shard & Hash   │
│ CRC32(key) %    │
│  shard_count    │
└─────────────────┘
     ↓
┌─────────────────┐
│ Check Metadata  │
│ - TTL validity  │
│ - File exists   │
└─────────────────┘
     ↓
    Hit? ──No──┐
     │         ↓
    Yes   ┌─────────────────┐
     ↓    │ Upstream Fetch  │
┌─────────────────┐         │
│ Decompress &    │         │
│ Return Cached   │         │
│    Response     │         │
└─────────────────┘         │
     ↓                      ↓
┌─────────────────┐ ┌─────────────────┐
│ Access Logging  │ │ Store in Cache  │
└─────────────────┘ │ - Compress      │
                    │ - Write to disk │
                    │ - Update meta   │
                    │ - Persistence   │
                    └─────────────────┘
                           ↓
                    ┌─────────────────┐
                    │ Return Response │
                    └─────────────────┘
```

## Component Functionality

### Disk-Based Storage System

#### Sharding Strategy
The cache uses a sharding system to distribute files across multiple directories:

```rust
fn get_cache_file_path(&self, cache_key: &str) -> PathBuf {
    let hash = crc32fast::hash(cache_key.as_bytes());
    let shard_id = hash % self.shard_count as u32;
    let file_name = format!("{:08x}", hash);
    self.cache_dir.join(format!("shard_{}", shard_id)).join(file_name)
}
```

**Key Features:**
- Uses CRC32 hashing for consistent distribution
- Configurable shard count (default based on system)
- Prevents directory overcrowding
- Enables parallel access patterns

#### Cache Key Generation
```rust
fn generate_cache_key(&self, uri: &str, headers: &HeaderMap) -> String {
    let mut hasher = DefaultHasher::new();
    uri.hash(&mut hasher);
    
    // Include relevant headers for cache variance
    for (name, value) in headers.iter() {
        if CACHE_VARIANT_HEADERS.contains(&name.as_str().to_lowercase().as_str()) {
            name.hash(&mut hasher);
            value.hash(&mut hasher);
        }
    }
    
    format!("{:016x}", hasher.finish())
}
```

### Cache Operations

#### Cache Get Operation
```rust
pub async fn get_cache(&self, cache_key: &str) -> Option<(Vec<u8>, HashMap<String, String>, u16)> {
    let metadata = {
        let cache = self.cache.lock().await;
        cache.get(cache_key).cloned()
    }?;
    
    // TTL validation
    if metadata.created_at.elapsed() > metadata.ttl {
        self.remove_cache(cache_key).await.ok();
        return None;
    }
    
    // File existence check
    if !metadata.body_path.exists() {
        self.remove_cache(cache_key).await.ok();
        return None;
    }
    
    // Read and decompress
    match tokio::fs::read(&metadata.body_path).await {
        Ok(compressed_body) => {
            match self.compression.decompress(compressed_body) {
                Ok(body) => Some((body, metadata.headers, metadata.status)),
                Err(_) => None,
            }
        }
        Err(_) => None,
    }
}
```

#### Cache Put Operation
```rust
pub async fn put_cache(
    &self,
    cache_key: &str,
    body: Vec<u8>,
    resp_header: &ResponseHeader,
    bucket: Option<&CacheBucket>,
) -> Result<(), String> {
    // Cache-Control validation
    let skip_cache = resp_header.headers.get("cache-control")
        .map_or(false, |cc| cc.to_str().map_or(false, |cc| {
            cc.to_lowercase().contains("no-store")
        }));
    
    if skip_cache {
        return Ok(());
    }
    
    // Compression
    let compressed_body = if self.compression.should_compress(content_type) {
        self.compression.compress(body.clone())?
    } else {
        body.clone()
    };
    
    // Atomic file write
    let body_path = self.get_cache_file_path(&cache_key);
    let temp_path = body_path.with_extension("tmp");
    
    tokio::fs::write(&temp_path, &compressed_body).await?;
    tokio::fs::rename(&temp_path, &body_path).await?;
    
    // Metadata storage
    let metadata = CacheMetadata {
        headers: extract_headers(resp_header),
        status: resp_header.status.as_u16(),
        created_at: Instant::now(),
        ttl: self.ttl,
        body_path: body_path.clone(),
        body_size: compressed_body.len(),
    };
    
    // LRU eviction
    let evicted_file_path = {
        let mut cache = self.cache.lock().await;
        let mut evicted_file_path = None;
        
        if cache.len() >= self.capacity && !cache.contains_key(&cache_key) {
            if let Some(oldest) = cache.iter().min_by_key(|(_, e)| e.created_at) {
                let key = oldest.0.clone();
                evicted_file_path = Some(oldest.1.body_path.clone());
                cache.remove(&key);
            }
        }
        
        cache.insert(cache_key.clone(), metadata);
        evicted_file_path
    };
    
    // Cleanup evicted files
    if let Some(file_path) = evicted_file_path {
        if file_path.exists() {
            tokio::fs::remove_file(&file_path).await.ok();
        }
    }
    
    Ok(())
}
```

### Persistence and Cleanup

#### Metadata Persistence
The system supports persistent metadata storage:

```rust
pub fn start_persistence_service(&self) -> Option<JoinHandle<()>> {
    let config = self.persistence_config.as_ref()?.clone();
    let cache_dir = config.cache_dir.clone();
    let metadata_dir = config.metadata_dir.clone();
    
    // Create directories
    std::fs::create_dir_all(&cache_dir).ok()?;
    std::fs::create_dir_all(&metadata_dir).ok()?;
    
    let service = CachePersistence::new(self.clone(), config);
    Some(service.start_service())
}
```

#### Orphaned Files Cleanup
```rust
pub fn orphaned_files_cleanup(&self) -> Option<JoinHandle<()>> {
    let bucket = self.clone();
    let interval = Duration::from_secs(60);
    
    Some(tokio::spawn(async move {
        let mut cleanup_interval = tokio::time::interval(interval);
        loop {
            cleanup_interval.tick().await;
            if let Err(e) = bucket.cleanup_orphaned_files().await {
                error!("Orphaned files cleanup failed: {}", e);
            }
        }
    }))
}
```

## Configuration

### Basic Configuration
```yaml
cache:
  enabled: true
  cache-dir: "/tmp/stryvo-cache"
  cache-ttl: "1h"
  capacity: 10000
  shard-count: 16
  compression: true
```

### Service-Specific Configuration
```yaml
services:
  - name: "api-cache"
    cache:
      ttl: "30m"
      capacity: 5000
      persistence:
        enabled: true
        metadata-dir: "/var/cache/stryvo/metadata"
        cleanup-interval: "5m"
```

### Advanced Configuration
```yaml
cache:
  persistence:
    enabled: true
    metadata-dir: "/var/cache/stryvo/metadata"
    cleanup-interval: "300s"
    max-file-age: "24h"
  compression:
    enabled: true
    algorithms: ["gzip", "brotli"]
    min-size: 1024
  sharding:
    count: 32
    strategy: "crc32"
```

## Cache Strategies

### TTL-Based Expiration
- Configurable per-service TTL
- Automatic cleanup of expired entries
- Lazy expiration during access

### LRU Eviction
- Capacity-based eviction
- Tracks creation time for LRU ordering
- Automatic file cleanup on eviction

### Compression Integration
- Automatic compression based on content type
- Configurable compression algorithms
- Size-based compression decisions

### Cache-Control Respect
- Honors `Cache-Control: no-store` headers
- Supports cache variance headers
- Configurable header-based cache keys

## Performance Optimization

### Sharding Benefits
1. **Parallel Access**: Multiple shards allow concurrent operations
2. **Directory Size**: Prevents large directory performance issues
3. **Load Distribution**: Even distribution across filesystem

### Atomic Operations
1. **Temp File Pattern**: Write to .tmp then rename for atomicity
2. **Metadata Consistency**: In-memory metadata with disk persistence
3. **Cleanup Safety**: Orphaned file detection and removal

### Memory Efficiency
1. **Metadata Only**: Keep only metadata in memory
2. **Lazy Loading**: Read file content only when needed
3. **Compression**: Reduce disk usage and I/O

## Monitoring and Debugging

### Cache Metrics
- Hit/miss ratios
- Eviction counts
- Storage utilization
- Compression ratios

### Logging Integration
```rust
debug!("Cache hit for key: {}", cache_key);
debug!("Cache miss for key: {}", cache_key);
debug!("Storing in cache with key: {}", cache_key);
debug!("Evicted cache entry: {}", key);
```

### Common Issues
1. **Disk Space**: Monitor cache directory size
2. **Permissions**: Ensure write access to cache directories
3. **TTL Tuning**: Balance between freshness and performance
4. **Shard Count**: Optimize based on expected cache size

## Best Practices

### Configuration
1. **Shard Count**: Use powers of 2 for better distribution
2. **TTL Settings**: Match content update frequency
3. **Capacity Limits**: Set based on available disk space
4. **Compression**: Enable for text-based content

### Operations
1. **Monitoring**: Track cache hit rates and disk usage
2. **Cleanup**: Regular orphaned file cleanup
3. **Persistence**: Enable for production environments
4. **Backup**: Consider cache directory in backup strategies

### Security
1. **Directory Permissions**: Restrict cache directory access
2. **Path Validation**: Ensure cache keys don't escape directories
3. **Content Validation**: Verify cached content integrity
4. **Cleanup**: Regular removal of old cache files