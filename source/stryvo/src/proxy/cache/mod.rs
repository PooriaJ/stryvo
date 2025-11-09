// This module implements the caching functionality for the Stryvo proxy service.
// It provides a framework for caching HTTP responses to improve performance by
// reducing upstream requests for frequently accessed resources. The cache system
// supports configurable persistence, compression, and eviction policies, and integrates
// with the proxy's request and response pipeline.
//
// Key Components:
// - `StryvoProxyService`: The main proxy service struct that orchestrates caching,
//   rate limiting, load balancing, and request/response modification.
// - `CacheBucket` and `CacheService`: Manage in-memory and persistent cache storage,
//   including cleanup of orphaned files and metadata synchronization.
// - `persistence`: Handles on-disk storage of cache metadata and body files.
// - `service`: Implements the core cache logic, including cache lookup, storage,
//   and eviction.
// - `RateLimiters` and `Modifiers`: Support rate limiting and request/response
//   filtering as part of the proxy's functionality.
// - `LogEntry`: Facilitates logging of cache hits, misses, and other request details.
//
// The cache system uses a sharded directory structure for body files, with metadata
// stored in memory and optionally persisted to disk. It supports compression (e.g., Brotli)
// and enforces cache-control directives. The module is designed to be extensible and
// integrates with Pingora's HTTP proxy framework for high-performance request handling.

// Cache module: expose persistence and core caching service API.
pub use self::service::CacheResult;
pub mod persistence;
pub mod service;