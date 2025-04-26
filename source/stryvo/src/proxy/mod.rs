// Implements the core functionality of the Stryvo proxy, extending Pingora's HTTP proxy
// with advanced features like caching, rate limiting, load balancing, and request/response
// modification. It provides fine-grained control over the HTTP request/response lifecycle
// through a modular pipeline architecture.
//
// Key Components:
// - `StryvoProxyService`: Orchestrates proxy operations, integrating caching, rate limiting,
//   load balancing, and customizable request/response filters.
// - `Modifiers`: Manages request and response filters for flexible HTTP processing, including
//   CIDR-based blocking, header modifications, and more.
// - `RateLimiters`: Implements single and multi-key rate limiting to control request rates.
// - `CacheService` and `CacheBucket`: Enable response caching with in-memory and persistent
//   storage, supporting compression and configurable TTLs.
// - `RequestPipeline` and `ResponsePipeline`: Organize request and response processing into
//   modular stages for cache lookup, rate limiting, filtering, and logging.
// - `StryvoContext`: Tracks per-request state, including caching status, compression, and
//   upstream selection details.
//
// The module leverages Pingora's high-performance HTTP proxy framework, supporting multiple
// load balancing strategies (e.g., RoundRobin, Ketama) and extensible configuration via
// `ProxyConfig`. It ensures robust logging and error handling for reliable operation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::FutureExt;

use pingora::{server::Server, Error, ErrorType};
use pingora_core::{upstreams::peer::HttpPeer, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_load_balancing::{
    discovery,
    selection::{
        consistent::KetamaHashing, BackendIter, BackendSelection, FVNHash, Random, RoundRobin,
    },
    Backend, Backends, LoadBalancer,
};
use pingora_proxy::{ProxyHttp, Session};

use crate::{
    config::internal::{PathControl, ProxyConfig, SelectionKind},
    populate_listners,
    proxy::{
        request_modifiers::RequestModifyMod, request_selector::RequestSelector,
        response_modifiers::ResponseModifyMod,
    },
};

use crate::proxy::compression::CompressionStrategy;

use self::{
    cache::persistence::{CachePersistence, PersistenceConfig},
    cache::service::{CacheBucket, CacheService, SccMemoryCache},
    pipeline::{RequestPipeline, ResponsePipeline},
    rate_limiting::{multi::MultiRaterInstance, single::SingleInstance},
    request_filters::RequestFilterMod,
    service_registry::ServiceRegistry,
};

pub mod cache;
pub mod compression;
pub mod header_utils;
pub mod pipeline;
pub mod rate_limiting;
pub mod request_filters;
pub mod request_modifiers;
pub mod request_selector;
pub mod response_modifiers;
pub mod service_registry;

pub struct RateLimiters {
    request_filter_stage_multi: Vec<MultiRaterInstance>,
    request_filter_stage_single: Vec<SingleInstance>,
}

/// The [StryvoProxyService] is intended to capture the behaviors used to extend
/// the [HttpProxy] functionality by providing a [ProxyHttp] trait implementation.
///
/// The [ProxyHttp] trait allows us to provide callback-like control of various stages
/// of the [request/response lifecycle].
///
/// [request/response lifecycle]: https://github.com/cloudflare/pingora/blob/7ce6f4ac1c440756a63b0766f72dbeca25c6fc94/docs/user_guide/phase_chart.md
pub struct StryvoProxyService<BS: BackendSelection> {
    /// All modifiers used when implementing the [ProxyHttp] trait.
    pub modifiers: Modifiers,
    /// Load Balancer
    pub load_balancer: LoadBalancer<BS>,
    pub request_selector: RequestSelector,
    pub rate_limiters: RateLimiters,
    /// Cache service if enabled
    pub cache_service: Option<Arc<CacheService>>,
    /// Whether browser caching is enabled
    pub browser_cache_enabled: bool,
    /// Browser cache TTL in seconds (max-age value)
    pub browser_cache_ttl: Option<u64>,
}

/// Create a proxy service, with the type parameters chosen based on the config file
pub fn stryvo_proxy_service(
    conf: ProxyConfig,
    server: &Server,
) -> (Box<dyn pingora::services::Service>, Option<CachePersistence>) {
    // Pick the correctly monomorphized function. This makes the functions all have the
    // same signature of `fn(...) -> (Box<dyn Service>, Option<CachePersistence>)`.
    type ServiceMaker = fn(ProxyConfig, &Server) -> (Box<dyn pingora::services::Service>, Option<CachePersistence>);

    let service_maker: ServiceMaker = match conf.upstream_options.selection {
        SelectionKind::RoundRobin => StryvoProxyService::<RoundRobin>::from_basic_conf,
        SelectionKind::Random => StryvoProxyService::<Random>::from_basic_conf,
        SelectionKind::Fnv => StryvoProxyService::<FVNHash>::from_basic_conf,
        SelectionKind::Ketama => StryvoProxyService::<KetamaHashing>::from_basic_conf,
    };
    service_maker(conf, server)
}

impl<BS> StryvoProxyService<BS>
where
    BS: BackendSelection + Send + Sync + 'static,
    BS::Iter: BackendIter,
{
    /// Create a new [StryvoProxyService] from the given [ProxyConfig]
    pub fn from_basic_conf(
        conf: ProxyConfig,
        server: &Server,
    ) -> (Box<dyn pingora::services::Service>, Option<CachePersistence>) {
        let modifiers = Modifiers::from_conf(&conf.path_control).unwrap();

        // Build load balancer from upstream configurations
        let mut backends = BTreeSet::new();
        for uppy in conf.upstreams {
            let mut backend = Backend::new(&uppy._address.to_string())
                .expect("Failed to create backend from address");
            assert!(backend.ext.insert::<HttpPeer>(uppy).is_none());
            backends.insert(backend);
        }
        let disco = discovery::Static::new(backends);
        let upstreams = LoadBalancer::<BS>::from_backends(Backends::new(disco));
        upstreams
            .update()
            .now_or_never()
            .expect("static should not block")
            .expect("static should not error");

        // Initialize rate limiters
        let mut request_filter_stage_multi = vec![];
        let mut request_filter_stage_single = vec![];

        for rule in conf.rate_limiting.rules {
            match rule {
                rate_limiting::AllRateConfig::Single { kind, config } => {
                    let rater = SingleInstance::new(config, kind);
                    request_filter_stage_single.push(rater);
                }
                rate_limiting::AllRateConfig::Multi { kind, config } => {
                    let rater = MultiRaterInstance::new(config, kind);
                    request_filter_stage_multi.push(rater);
                }
            }
        }

        // Initialize cache if enabled
        let (cache_service, persistence) = if conf.cache_enabled {
            // Use cache configuration from ProxyConfig or defaults
            let cache_dir = conf
                .cache_dir
                .as_ref()
                .map(|dir| dir.join(&conf.name))
                .unwrap_or_else(|| PathBuf::from(format!("/tmp/stryvo-cache/{}", conf.name)));
            let shard_count = conf.shard_count.unwrap_or(32);
            let max_file_size = conf.max_file_size;

            let persistence_config = PersistenceConfig::new(cache_dir.clone(), 5, shard_count); // 5 seconds interval

            let memory_cache = SccMemoryCache::with_capacity(8192, cache_dir, shard_count, max_file_size)
                .with_ttl(conf.cache_ttl)
                .with_compression(CompressionStrategy::Auto)
                .with_persistence(persistence_config.clone());

            let cache_bucket = CacheBucket::new(memory_cache);
            let cache_service = Arc::new(CacheService::new(
                cache_bucket.clone(),
                format!("cache-{}", conf.name),
            ));

            // Create persistence instance
            let persistence = CachePersistence::new(cache_bucket, persistence_config);

            // Register the cache service with the service registry
            let registry = ServiceRegistry::new();
            registry.register(&*cache_service);

            tracing::info!(
                "Cache service registered for {} with {} shards",
                conf.name,
                shard_count
            );

            (Some(cache_service), Some(persistence))
        } else {
            (None, None)
        };

        let mut my_proxy = pingora_proxy::http_proxy_service_with_name(
            &server.configuration,
            Self {
                modifiers,
                load_balancer: upstreams,
                request_selector: conf.upstream_options.selector,
                rate_limiters: RateLimiters {
                    request_filter_stage_multi,
                    request_filter_stage_single,
                },
                cache_service,
                browser_cache_enabled: conf.browser_cache_enabled,
                browser_cache_ttl: if conf.browser_cache_enabled {
                    Some(conf.browser_cache_ttl)
                } else {
                    None
                },
            },
            &conf.name,
        );

        populate_listners(conf.listeners, &mut my_proxy);

        (Box::new(my_proxy), persistence)
    }
}

//
// MODIFIERS
//
// This section implements "Path Control Modifiers". As an overview of the initially
// planned control points:
//
//             ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐  ┌ ─ ─ ─ ─ ─ ─ ┐
//                  ┌───────────┐    ┌───────────┐    ┌───────────┐
//             │    │  Request  │    │           │    │  Request  │    │  │             │
// Request  ═══════▶│  Arrival  │═══▶│Which Peer?│═══▶│ Forwarded │═══════▶
//             │    │           │    │           │    │           │    │  │             │
//                  └───────────┘    └───────────┘    └───────────┘
//             │          │                │                │          │  │             │
//                        │                │                │
//             │          ├───On Error─────┼────────────────┤          │  │  Upstream   │
//                        │                │                │
//             │          │          ┌───────────┐    ┌───────────┐    │  │             │
//                        ▼          │ Response  │    │ Response  │
//             │                     │Forwarding │    │  Arrival  │    │  │             │
// Response ◀═══════════════════════│           │◀═══│           │◀══════
//             │                     └───────────┘    └───────────┘    │  │             │
//               ┌────────────────────────┐
//             └ ┤ Simplified Phase Chart │─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘  └ ─ ─ ─ ─ ─ ─ ┘
//               └────────────────────────┘
//
// At the moment, "Request Forwarded" corresponds with "upstream_request_filters".
//

/// All modifiers used when implementing the [ProxyHttp] trait.
pub struct Modifiers {
    /// Filters used during the handling of [ProxyHttp::request_filter]
    pub request_filters: Vec<Box<dyn RequestFilterMod>>,
    /// Filters used during the handling of [ProxyHttp::upstream_request_filter]
    pub upstream_request_filters: Vec<Box<dyn RequestModifyMod>>,
    /// Filters used during the handling of [ProxyHttp::upstream_response_filter]
    pub upstream_response_filters: Vec<Box<dyn ResponseModifyMod>>,
}

impl Modifiers {
    /// Build all modifiers from the provided [PathControl]
    pub fn from_conf(conf: &PathControl) -> Result<Self> {
        let mut conf = conf.clone();

        let mut request_filter_mods: Vec<Box<dyn RequestFilterMod>> = vec![];
        for mut filter in conf.request_filters.drain(..) {
            let kind = filter.remove("kind").ok_or_else(|| {
                Error::new(ErrorType::Custom("Missing 'kind' field in request filter"))
            })?;
            
            let f: Box<dyn RequestFilterMod> = match kind.as_str() {
                "block-cidr-range" => {
                    Box::new(request_filters::CidrRangeFilter::from_settings(filter)?)
                }
                other => {
                    tracing::warn!("Unknown request filter: '{}'", other);
                    return Err(Error::new(ErrorType::Custom("Unsupported request filter type")));
                }
            };
            request_filter_mods.push(f);
        }

        let mut upstream_request_filters: Vec<Box<dyn RequestModifyMod>> = vec![];
        for mut filter in conf.upstream_request_filters.drain(..) {
            let kind = filter.remove("kind").ok_or_else(|| {
                Error::new(ErrorType::Custom("Missing 'kind' field in upstream request filter"))
            })?;
            
            let f: Box<dyn RequestModifyMod> = match kind.as_str() {
                "remove-header-key-regex" => Box::new(
                    request_modifiers::RemoveHeaderKeyRegex::from_settings(filter)?
                ),
                "upsert-header" => {
                    Box::new(request_modifiers::UpsertHeader::from_settings(filter)?)
                }
                other => {
                    tracing::warn!("Unknown upstream request filter: '{}'", other);
                    return Err(Error::new(ErrorType::Custom("Unsupported upstream request filter type")));
                }
            };
            upstream_request_filters.push(f);
        }

        let mut upstream_response_filters: Vec<Box<dyn ResponseModifyMod>> = vec![];
        for mut filter in conf.upstream_response_filters.drain(..) {
            let kind = filter.remove("kind").ok_or_else(|| {
                Error::new(ErrorType::Custom("Missing 'kind' field in upstream response filter"))
            })?;
            
            let f: Box<dyn ResponseModifyMod> = match kind.as_str() {
                "remove-header-key-regex" => Box::new(
                    response_modifiers::RemoveHeaderKeyRegex::from_settings(filter)?
                ),
                "upsert-header" => {
                    Box::new(response_modifiers::UpsertHeader::from_settings(filter)?)
                }
                other => {
                    tracing::warn!("Unknown upstream response filter: '{}'", other);
                    return Err(Error::new(ErrorType::Custom("Unsupported upstream response filter type")));
                }
            };
            upstream_response_filters.push(f);
        }

        Ok(Self {
            request_filters: request_filter_mods,
            upstream_request_filters,
            upstream_response_filters,
        })
    }
}

/// Per-peer context for request processing
pub struct StryvoContext {
    /// Buffer used for request selection and timing information
    selector_buf: Vec<u8>,
    /// Store the upstream address for logging
    upstream_addr: Option<String>,
    /// Cache status for tracking caching outcomes (HIT, MISS, BYPASS)
    pub cache_status: Option<String>,
    /// Flag to indicate if the response should be cached
    pub should_cache_response: bool,
    /// Collected response body for caching
    pub cache_body: Option<Vec<Bytes>>,
    /// Request header for caching
    pub cache_req_header: Option<RequestHeader>,
    /// Response header for caching
    pub cache_resp_header: Option<ResponseHeader>,
    /// Selected compression encoding for client response
    pub compression_encoding: Option<compression::CompressionEncoding>,
    /// Browser cache enabled flag from service configuration
    pub browser_cache_enabled: bool,
    /// Browser cache TTL from service configuration
    pub browser_cache_ttl: Option<u64>,
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
            browser_cache_enabled: self.browser_cache_enabled,
            browser_cache_ttl: self.browser_cache_ttl,
        }
    }

    /// Handle the response body filtering with improved organization using HeaderUtils
    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        data: &mut std::option::Option<bytes::Bytes>,
        eof: bool,
        ctx: &mut Self::CTX,
    ) -> () {
        // Handle caching and compression in a more efficient way
        if let Some(data_ref) = data.as_ref() {
            // Store data for caching if needed (before compression)
            if ctx.should_cache_response {
                if let Some(body_chunks) = &mut ctx.cache_body {
                    if !data_ref.is_empty() {
                        tracing::debug!(
                            "Collecting response body chunk for cache: {} bytes",
                            data_ref.len()
                        );
                        body_chunks.push(data_ref.clone());
                    }
                }
            }
            
            // Apply compression if needed
            if let Some(encoding) = ctx.compression_encoding {
                if !data_ref.is_empty() {
                    if let Some(chunk) = data.as_mut() {
                        // Use HeaderUtils for compression operations
                        match compression::ClientCompression::compress(chunk, encoding) {
                            Ok(compressed) => {
                                *chunk = compressed;
                                tracing::debug!(
                                    "Compressed response chunk with {:?}: {} bytes",
                                    encoding,
                                    chunk.len()
                                );
                            }
                            Err(e) => {
                                tracing::warn!("Compression failed: {}", e);
                                // Continue with uncompressed data
                            }
                        }
                    }
                }
            }
        }

        // Handle EOF and cache storage
        if eof && ctx.should_cache_response {
            if let (Some(body_chunks), Some(req_header), Some(resp_header)) = 
                (&mut ctx.cache_body, &ctx.cache_req_header, &ctx.cache_resp_header) {
                
                if !body_chunks.is_empty() {
                    let total_size: usize = body_chunks.iter().map(|chunk| chunk.len()).sum();
                    tracing::debug!(
                        "Collected complete response body for cache: {} bytes",
                        total_size
                    );

                    let mut combined_body = BytesMut::with_capacity(total_size);
                    for chunk in body_chunks.drain(..) {
                        combined_body.extend_from_slice(&chunk);
                    }
                    let body = combined_body.freeze();

                    // Prepare response header for caching (without compression encoding)
                    let mut cache_resp_header = resp_header.clone();
                    cache_resp_header.remove_header("content-encoding");

                    // Clone cache_service to avoid borrowing self
                    if let Some(cache_service) = self.cache_service.clone() {
                        let storage = cache_service.bucket().storage();
                        let bucket = cache_service.bucket().clone();
                        let req_header = req_header.clone();
                        let resp_header = cache_resp_header;
                        
                        let url = req_header.uri.to_string();
                        let method = req_header.method.as_str().to_string();

                        tokio::spawn(async move {
                            match storage
                                .put_cache(&req_header, &resp_header, body, Some(&bucket))
                                .await
                            {
                                Ok(_) => {
                                    tracing::debug!(
                                        "Successfully cached response: {} {}",
                                        method,
                                        url
                                    );
                                }
                                Err(e) => {
                                    tracing::error!("Failed to cache response: {}", e);
                                }
                            }
                        });
                    }
                }
            }

            // Reset cache-related context fields
            ctx.should_cache_response = false;
            ctx.cache_req_header = None;
            ctx.cache_resp_header = None;
            ctx.cache_body = None;
        }
    }

    /// Handle the "Request filter" stage using the Pipeline pattern
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        // Initialize timing information
        let start_time = std::time::Instant::now();
        ctx.selector_buf.clear();
        ctx.selector_buf
            .extend_from_slice(&start_time.elapsed().as_nanos().to_ne_bytes());

        // Process cache lookup stage
        if let Some(cache_service) = &self.cache_service {
            if let Some(result) =
                RequestPipeline::process_cache_lookup(session, ctx, cache_service).await?
            {
                return Ok(result);
            }
        }

        // Process rate limiting stage
        if let Some(result) = RequestPipeline::process_rate_limiting(
            session,
            ctx,
            &self.rate_limiters.request_filter_stage_multi,
            &self.rate_limiters.request_filter_stage_single,
        )
        .await?
        {
            return Ok(result);
        }

        // Process request filters stage
        if let Some(result) =
            RequestPipeline::process_request_filters(session, ctx, &self.modifiers.request_filters)
                .await?
        {
            return Ok(result);
        }

        Ok(false)
    }

    /// Handle the "upstream peer" phase, where we pick which upstream to proxy to.
    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let key = (self.request_selector)(ctx, session);

        let backend = self.load_balancer.select(key, 256)
            .ok_or_else(|| pingora::Error::new_str("No backend available"))?;

        // Manually clear the selector buf to avoid accidental leaks
        ctx.selector_buf.clear();

        // Retrieve the HttpPeer from the associated backend metadata
        let peer = backend
            .ext
            .get::<HttpPeer>()
            .ok_or_else(|| pingora::Error::new_str("Missing backend metadata"))
            .map(|p| {
                // Store the upstream address in the context for logging
                ctx.upstream_addr = Some(p._address.to_string());
                Box::new(p.clone())
            });

        peer
    }

    /// Handle the "upstream request filter" phase, where we can choose to make
    /// modifications to the request, prior to it being passed along to the
    /// upstream.
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

    /// Handle the "upstream response filter" phase using the Pipeline pattern
    fn upstream_response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) {
        // Process cache status and headers
        ResponsePipeline::process_cache_status(
            session,
            upstream_response,
            ctx,
            self.cache_service.as_ref().map(|v| &**v),
            self.browser_cache_enabled,
            self.browser_cache_ttl,
        );

        // Process compression setup
        ResponsePipeline::process_compression_setup(session, upstream_response, ctx);

        // Process response filters
        ResponsePipeline::process_response_filters(
            session,
            upstream_response,
            ctx,
            &self.modifiers.upstream_response_filters,
        );

        // Process logging
        ResponsePipeline::process_logging(session, upstream_response, ctx);
    }
}

/// Helper function that extracts the value of a given key.
///
/// Returns an error if the key does not exist
fn extract_val(key: &str, map: &mut BTreeMap<String, String>) -> Result<String> {
    map.remove(key).ok_or_else(|| {
        // TODO: better "Error" creation
        tracing::error!("Missing key: '{key}'");
        Error::new_str("Missing configuration field!")
    })
}

/// Helper function to make sure the map is empty
///
/// This is used to reject unknown configuration keys
fn ensure_empty(map: &BTreeMap<String, String>) -> Result<()> {
    if !map.is_empty() {
        let keys = map.keys().map(String::as_str).collect::<Vec<&str>>();
        let all_keys = keys.join(", ");
        tracing::error!("Extra keys found: '{all_keys}'");
        Err(Error::new_str("Extra settings found!"))
    } else {
        Ok(())
    }
}