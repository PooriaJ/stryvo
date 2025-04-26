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

use std::sync::Arc;
use std::time::{Duration, Instant};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use pingora_core::{upstreams::peer::HttpPeer, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_load_balancing::{
    selection::{BackendIter, BackendSelection}, LoadBalancer
};
use pingora_proxy::{ProxyHttp, Session};
use tokio::sync::mpsc;
use crc32fast::Hasher;

use crate::proxy::{
    request_modifiers::RequestModifyMod,
    request_selector::RequestSelector,
    response_modifiers::ResponseModifyMod,
    compression::ClientCompression,
};

pub use self::service::{CacheBucket, CacheResult, CacheService};

pub mod persistence;
pub mod service;

pub struct RateLimiters {
    request_filter_stage_multi: Vec<crate::proxy::rate_limiting::multi::MultiRaterInstance>,
    request_filter_stage_single: Vec<crate::proxy::rate_limiting::single::SingleInstance>,
}

pub struct StryvoProxyService<BS: BackendSelection> {
    pub modifiers: Modifiers,
    pub load_balancer: LoadBalancer<BS>,
    pub request_selector: RequestSelector,
    pub rate_limiters: RateLimiters,
    pub cache: Option<Arc<CacheBucket>>,
    pub _cache_ttl: Duration,
    log_tx: mpsc::Sender<LogEntry>,
}

pub struct Modifiers {
    pub request_filters: Vec<Box<dyn crate::proxy::RequestFilterMod>>,
    pub upstream_request_filters: Vec<Box<dyn RequestModifyMod>>,
    pub upstream_response_filters: Vec<Box<dyn ResponseModifyMod>>,
}

#[derive(Debug)]
struct LogEntry {
    method: String,
    uri: String,
    status: u16,
    upstream: String,
    elapsed: Duration,
    http_version: String,
    user_agent: String,
    referer: String,
    cache_status: String,
}

impl LogEntry {
    fn log(&self) {
        crate::logging::log_access(
            &self.method,
            &self.uri,
            self.status,
            &self.upstream,
            self.elapsed,
            &self.http_version,
            &self.user_agent,
            &self.referer,
            &self.cache_status,
        );
    }
}

use crate::proxy::StryvoContext;

// Constants
const DEFAULT_MAX_CACHE_SIZE: usize = 1_048_576; // 1MB

impl<BS: BackendSelection> StryvoProxyService<BS> {
    fn generate_cache_key(&self, req_header: &RequestHeader) -> String {
        let mut hasher = Hasher::new();
        
        // Add method and URI to the key
        hasher.update(req_header.method.as_str().as_bytes());
        hasher.update(b" ");
        hasher.update(req_header.uri.to_string().as_bytes());
        
        // Add important headers that might affect caching
        if let Some(auth) = req_header.headers.get("authorization") {
            hasher.update(b"auth:");
            hasher.update(auth.as_bytes());
        }
        
        if let Some(vary_headers) = req_header.headers.get("vary") {
            if let Ok(vary_str) = vary_headers.to_str() {
                for header_name in vary_str.split(',').map(|s| s.trim().to_lowercase()) {
                    if let Some(header_val) = req_header.headers.get(&header_name) {
                        hasher.update(header_name.as_bytes());
                        hasher.update(b":");
                        hasher.update(header_val.as_bytes());
                    }
                }
            }
        }
        
        format!("{:x}", hasher.finalize())
    }
    
    fn get_max_cache_size() -> usize {
        std::env::var("STRYVO_MAX_FILE_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_CACHE_SIZE)
    }
    
    async fn handle_cached_response(&self, 
        session: &mut Session, 
        ctx: &mut StryvoContext,
        cache_key: &str, 
        body: Bytes, 
        headers: Vec<(String, String)>, 
        status: u16
    ) -> Result<bool> {
        ctx.cache_status = Some("HIT".to_string());
        tracing::debug!("Cache hit for {}", cache_key);

        let mut resp_header = ResponseHeader::build(status, Some(headers.len() + 2))?;
        for (key, value) in headers {
            if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
                resp_header.append_header(key, value)?;
            }
        }
        resp_header.append_header("x-cache", "HIT")?;

        // Handle compression if needed
        let body = self.apply_compression_if_needed(session, ctx, &body, &mut resp_header)?;

        if !body.is_empty() {
            resp_header.append_header("content-length", body.len().to_string())?;
        }

        session
            .downstream_session
            .write_response_header(Box::new(resp_header))
            .await?;
        
        if !body.is_empty() {
            session
                .downstream_session
                .write_response_body(body, true)
                .await?;
        }

        Ok(true)
    }
    
    fn apply_compression_if_needed(
        &self,
        session: &Session,
        ctx: &mut StryvoContext, 
        body: &Bytes,
        resp_header: &mut ResponseHeader
    ) -> Result<Bytes> {
        if let Some(encoding) = ClientCompression::select_encoding(session.req_header()) {
            ctx.compression_encoding = Some(encoding);
            match ClientCompression::compress(body, encoding) {
                Ok(compressed) => {
                    resp_header.append_header("content-encoding", encoding.as_str())?;
                    Ok(compressed)
                }
                Err(e) => {
                    tracing::warn!("Failed to compress response: {}", e);
                    Ok(body.clone())
                }
            }
        } else {
            Ok(body.clone())
        }
    }
    
    fn check_cache_size_limit(&self, existing_size: usize, new_chunk_size: usize) -> bool {
        let max_size = Self::get_max_cache_size();
        existing_size + new_chunk_size <= max_size
    }
}

#[async_trait]
impl<BS> ProxyHttp for StryvoProxyService<BS>
where
    BS: BackendSelection + Send + Sync + 'static,
    BS::Iter: BackendIter,
{
    type CTX = StryvoContext;

    fn new_ctx(&self) -> Self::CTX {
        StryvoContext {
            selector_buf: Vec::new(),
            upstream_addr: None,
            cache_status: None,
            should_cache_response: false,
            cache_body: Some(Vec::new()),
            cache_req_header: None,
            cache_resp_header: None,
            compression_encoding: None,
            browser_cache_enabled: false,
            browser_cache_ttl: None,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        // Record start time for timing metrics
        let start_time = Instant::now();
        ctx.selector_buf.clear();
        ctx.selector_buf
            .extend_from_slice(&start_time.elapsed().as_nanos().to_ne_bytes());

        // Check cache if enabled
        if let Some(cache) = &self.cache {
            let storage = cache.storage();
            let cache_key = self.generate_cache_key(session.req_header());

            match storage.get_cache(session.req_header(), Some(cache.as_ref())).await {
                Ok(CacheResult::Hit {
                    body,
                    headers,
                    status,
                }) => {
                    return self.handle_cached_response(session, ctx, &cache_key, body, headers, status).await;
                }
                Ok(CacheResult::Miss) => {
                    ctx.cache_status = Some("MISS".to_string());
                    tracing::debug!("Cache miss for {}", cache_key);
                }
                Err(e) => {
                    ctx.cache_status = Some("ERROR".to_string());
                    tracing::error!("Cache lookup error for {}: {}", cache_key, e);
                }
            }
        } else {
            ctx.cache_status = Some("BYPASS".to_string());
        }

        // Check rate limits
        let rate_limited = self
            .rate_limiters
            .request_filter_stage_multi
            .iter()
            .filter_map(|l| l.get_ticket(session))
            .chain(
                self.rate_limiters
                    .request_filter_stage_single
                    .iter()
                    .filter_map(|l| l.get_ticket(session)),
            )
            .any(|t| t.now_or_never() == crate::proxy::rate_limiting::Outcome::Declined);

        if rate_limited {
            tracing::trace!("Rejecting due to rate limiting failure");
            crate::logging::log_error(
                session.req_header().method.as_str(),
                session.req_header().uri.to_string().as_str(),
                "Rate limiting rejection",
            );
            session.downstream_session.respond_error(429).await;
            return Ok(true);
        }

        // Apply request filters
        for filter in &self.modifiers.request_filters {
            match filter.request_filter(session, ctx).await {
                o @ Ok(true) => return o,
                e @ Err(_) => {
                    if let Err(err) = &e {
                        crate::logging::log_error(
                            session.req_header().method.as_str(),
                            session.req_header().uri.to_string().as_str(),
                            &format!("Request filter error: {}", err),
                        );
                    }
                    return e;
                }
                Ok(false) => {}
            }
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let key = (self.request_selector)(ctx, session);
        let backend = self.load_balancer.select(key, 256);
        ctx.selector_buf.clear();

        let backend = backend.ok_or_else(|| pingora::Error::new_str("Unable to determine backend"))?;
        let peer = backend
            .ext
            .get::<HttpPeer>()
            .map(|p| {
                ctx.upstream_addr = Some(p._address.to_string());
                Box::new(p.clone())
            })
            .ok_or_else(|| pingora::Error::new_str("Fatal: Missing selected backend metadata"));

        peer
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        header: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        for filter in &self.modifiers.upstream_request_filters {
            filter.upstream_request_filter(session, header, ctx).await?;
        }
        Ok(())
    }

    fn upstream_response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) {
        let cache_status = match &ctx.cache_status {
            Some(status) => status.clone(),
            None => {
                if self.cache.is_some() {
                    "BYPASS".to_string()
                } else {
                    "NONE".to_string()
                }
            }
        };

        if let Err(e) = upstream_response.insert_header("x-cache", cache_status.as_str()) {
            tracing::warn!("Failed to set x-cache header: {}", e);
        }

        // Apply response filters
        for filter in &self.modifiers.upstream_response_filters {
            filter.upstream_response_filter(session, upstream_response, ctx);
        }

        // Check if response is cacheable
        if self.cache.is_some() && (200..300).contains(&upstream_response.status.as_u16()) {
            if CacheService::is_cacheable(session.req_header(), upstream_response) {
                ctx.should_cache_response = true;
                ctx.cache_req_header = Some(session.req_header().clone());
                ctx.cache_resp_header = Some(upstream_response.clone());
            } else {
                tracing::debug!(
                    "Response not cacheable based on headers: method={}, cache-control={:?}",
                    session.req_header().method.as_str(),
                    upstream_response.headers.get("cache-control")
                );
            }
        } else {
            tracing::debug!(
                "Response not cacheable due to status code: {}",
                upstream_response.status.as_u16()
            );
        }

        // Set up compression if needed
        if let Some(encoding) = ClientCompression::select_encoding(session.req_header()) {
            ctx.compression_encoding = Some(encoding);
            if let Err(e) = upstream_response.append_header("content-encoding", encoding.as_str()) {
                tracing::warn!("Failed to set content-encoding header: {}", e);
            }
        }

        // Calculate response time
        let elapsed = if ctx.selector_buf.len() >= 16 {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&ctx.selector_buf[0..16]);
            let start_nanos = u128::from_ne_bytes(bytes);
            let current_nanos = Instant::now().elapsed().as_nanos();
            Duration::from_nanos((current_nanos - start_nanos) as u64)
        } else {
            Duration::from_secs(0)
        };

        // Prepare log data
        let upstream = ctx.upstream_addr.as_ref().map(|s| s.as_str()).unwrap_or("unknown");
        let http_version = format!("{:?}", session.req_header().version);
        let user_agent = session
            .req_header()
            .headers
            .get("user-agent")
            .map(|v| v.to_str().unwrap_or("unknown"))
            .unwrap_or("unknown");
        let referer = session
            .req_header()
            .headers
            .get("referer")
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("");

        let log_entry = LogEntry {
            method: session.req_header().method.as_str().to_string(),
            uri: session.req_header().uri.to_string(),
            status: upstream_response.status.as_u16(),
            upstream: upstream.to_string(),
            elapsed,
            http_version,
            user_agent: user_agent.to_string(),
            referer: referer.to_string(),
            cache_status: cache_status.clone(),
        };

        log_entry.log();

        if let Err(e) = self.log_tx.try_send(log_entry) {
            tracing::warn!("Failed to send log entry: {}", e);
        }

        tracing::debug!("Cache status: {}", cache_status);
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        data: &mut Option<Bytes>,
        eof: bool,
        ctx: &mut Self::CTX,
    ) {
        // First, handle caching if enabled
        if ctx.should_cache_response {
            if let Some(body_chunks) = &mut ctx.cache_body {
                if let Some(chunk) = data.as_ref() {
                    if !chunk.is_empty() {
                        let total_size: usize = body_chunks.iter().map(|c| c.len()).sum();
                        
                        // Check size limit BEFORE adding chunk
                        if !self.check_cache_size_limit(total_size, chunk.len()) {
                            tracing::warn!("Response body too large to cache: {} bytes", total_size + chunk.len());
                            ctx.should_cache_response = false;
                            ctx.cache_body = None;
                        } else {
                            tracing::debug!("Collecting response body chunk for cache: {} bytes", chunk.len());
                            body_chunks.push(chunk.clone());
                        }
                    }
                }
            }
        }

        // Then, apply compression if needed
        if let Some(encoding) = ctx.compression_encoding {
            if let Some(chunk) = data.as_mut() {
                if !chunk.is_empty() {
                    match ClientCompression::compress(chunk, encoding) {
                        Ok(compressed) => {
                            *chunk = compressed;
                            tracing::debug!("Compressed response chunk with {:?}: {} bytes", encoding, chunk.len());
                        }
                        Err(e) => {
                            tracing::warn!("Failed to compress response chunk: {}", e);
                        }
                    }
                }
            }
        }

        // Finally, handle end of file for caching
        if eof && ctx.should_cache_response {
            if let (Some(body_chunks), Some(req_header), Some(resp_header)) = 
                (&mut ctx.cache_body, &ctx.cache_req_header, &ctx.cache_resp_header) {
                
                let total_size: usize = body_chunks.iter().map(|chunk| chunk.len()).sum();
                tracing::debug!("Collected complete response body for cache: {} bytes", total_size);

                let mut combined_body = BytesMut::with_capacity(total_size);
                for chunk in body_chunks.drain(..) {
                    combined_body.extend_from_slice(&chunk);
                }
                let body = combined_body.freeze();

                let mut cache_resp_header = resp_header.clone();
                cache_resp_header.remove_header("content-encoding");

                if let Some(cache) = self.cache.clone() {
                    let storage = cache.storage();
                    let bucket = cache.clone();
                    let req_header = req_header.clone();
                    let cache_key = self.generate_cache_key(&req_header);

                    tokio::spawn(async move {
                        if let Err(e) = storage.put_cache(&req_header, &cache_resp_header, body, Some(&bucket)).await {
                            tracing::error!("Failed to cache response for {}: {}", cache_key, e);
                        } else {
                            tracing::debug!("Cached response for {}", cache_key);
                        }
                    });
                }

                // Reset cache context
                ctx.should_cache_response = false;
                ctx.cache_req_header = None;
                ctx.cache_resp_header = None;
                ctx.cache_body = None;
            }
        }
    }
}