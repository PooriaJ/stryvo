use std::sync::Arc;
use std::path::PathBuf;
use async_trait::async_trait;
use pingora_core::{upstreams::peer::HttpPeer, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use tracing::{debug, error, info, warn};

use crate::config::internal::VHostConfig;
use crate::proxy::vhost::certificate_manager::CertificateManager;
use crate::proxy::vhost::router::VHostRouter;
use crate::proxy::StryvoContext;
use crate::proxy::cache::service::{CacheService, CacheBucket, SccMemoryCache};
use crate::proxy::cache::persistence::PersistenceConfig;
use crate::proxy::compression::CompressionStrategy;
use crate::proxy::service_registry::ServiceRegistry;
use crate::acme::manager::{AcmeManager, CertEvent};

/// VHost-aware proxy service that handles multiple domains with SNI certificate selection
pub struct VHostProxyService {
    /// Certificate manager for SNI-based certificate selection
    pub certificate_manager: Arc<CertificateManager>,
    /// VHost router for domain-to-upstream mapping
    pub vhost_router: VHostRouter,
    /// VHost configuration
    pub vhost_config: VHostConfig,
    /// Cache service for internal caching
    pub cache_service: Option<Arc<CacheService>>,
    /// ACME manager for automatic SSL certificate management
    pub acme_manager: Option<Arc<AcmeManager>>,
    /// Flag to track if ACME acquisition has been triggered
    acme_triggered: Arc<std::sync::atomic::AtomicBool>,
}

impl VHostProxyService {
    /// Create a new VHostProxyService from configuration
    /// This properly initializes the service with certificate management and routing
    pub async fn new(vhost_config: VHostConfig, acme_config: Option<crate::acme::config::AcmeConfig>) -> Result<Self> {
        // Validate the configuration first
        if let Err(e) = vhost_config.validate() {
            error!("VHost configuration validation failed: {}", e);
            return Err(pingora::Error::new_str("Invalid VHost configuration"));
        }

        // Create certificate manager with proper error handling
        let certificate_manager = Arc::new(
            CertificateManager::new(&vhost_config)
                .await
                .map_err(|e| {
                    error!("Failed to create certificate manager for service '{}': {}", vhost_config.name, e);
                    e
                })?
        );

        // Create VHost router with domain mappings
        let vhost_router = VHostRouter::new(&vhost_config);

        let cert_stats = certificate_manager.get_certificate_stats().await;
        info!(
            "Created VHost proxy service '{}' with {} domains and {} listeners (certificates loaded: {})",
            vhost_config.name,
            vhost_config.domains.len(),
            vhost_config.listeners.len(),
            cert_stats.total_domains
        );

        // Log configured domains for debugging
        for domain_name in vhost_config.domains.keys() {
            debug!("Configured domain: {}", domain_name);
        }

        // Initialize cache if enabled
        let cache_service = if vhost_config.cache_enabled {
            // Use cache configuration from VHostConfig or defaults
            let cache_dir = vhost_config
                .cache_dir
                .as_ref()
                .map(|dir| dir.join(&vhost_config.name))
                .unwrap_or_else(|| PathBuf::from(format!("/tmp/stryvo-cache/{}", vhost_config.name)));
            let shard_count = vhost_config.shard_count.unwrap_or(32);
            let max_file_size = vhost_config.max_file_size;

            let persistence_config = PersistenceConfig::new(cache_dir.clone(), 5, shard_count); // 5 seconds interval

            let memory_cache = SccMemoryCache::with_capacity(8192, cache_dir, shard_count, max_file_size)
                .with_ttl(vhost_config.cache_ttl)
                .with_compression(CompressionStrategy::Auto)
                .with_persistence(persistence_config.clone());

            let cache_bucket = CacheBucket::new(memory_cache);
            let cache_service = Arc::new(CacheService::new(
                cache_bucket.clone(),
                format!("vhost-cache-{}", vhost_config.name),
            ));

            // Register the cache service with the service registry
            let registry = ServiceRegistry::new();
            registry.register(&*cache_service);

            info!(
                "VHost cache service registered for {} with {} shards",
                vhost_config.name,
                shard_count
            );

            Some(cache_service)
        } else {
            None
        };

        // Initialize ACME manager if auto_ssl is enabled for any domain and ACME config is provided
        let acme_manager = if Self::has_auto_ssl_domains(&vhost_config) && acme_config.is_some() {
            let acme_config = acme_config.unwrap();
            
            if acme_config.enabled {
                info!("Auto SSL domains detected and ACME enabled - initializing ACME manager for service '{}'", vhost_config.name);
                
                let mut acme_manager = AcmeManager::new(acme_config.clone());
                
                // Initialize the ACME manager
                if let Err(e) = acme_manager.initialize().await {
                    error!("Failed to initialize ACME manager for service '{}': {}", vhost_config.name, e);
                    warn!("Continuing without ACME manager - auto SSL domains will use bootstrap certificates");
                    None
                } else {
                    info!("ACME manager initialized successfully for service '{}'", vhost_config.name);
                    
                    // Collect domains that need certificates
                    let auto_ssl_domains: Vec<String> = vhost_config.domains.iter()
                        .filter_map(|(name, config)| {
                            if config.ssl_config.as_ref().map(|s| s.auto_ssl).unwrap_or(false) {
                                Some(name.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    
                    if !auto_ssl_domains.is_empty() {
                        info!("Auto SSL enabled for {} domain(s): {:?}", auto_ssl_domains.len(), auto_ssl_domains);
                        
                        // Filter domains: only mark missing or near-expiration certificates (< renewal_days)
                        let mut pending_domains: Vec<String> = Vec::new();
                        for domain in auto_ssl_domains.iter() {
                            if let Some(info) = acme_manager.get_certificate_info(domain).await {
                                let days_left = info.days_until_expiration();
                                let threshold = acme_config.renewal_days as i64;
                                if info.needs_renewal(acme_config.renewal_days) {
                                    info!(
                                        "Auto-SSL: cached certificate for '{}' near expiration ({} days left < {} days); marking for renewal",
                                        domain, days_left, threshold
                                    );
                                    pending_domains.push(domain.clone());
                                } else {
                                    info!(
                                        "Auto-SSL: using cached certificate for '{}' ({} days left ≥ {} days); not marking as pending",
                                        domain, days_left, threshold
                                    );
                                }
                            } else {
                                info!("Auto-SSL: no cached certificate found for '{}' — marking for acquisition", domain);
                                pending_domains.push(domain.clone());
                            }
                        }
                        
                        if !pending_domains.is_empty() {
                            acme_manager.add_pending_domains(pending_domains.clone()).await;
                            info!(
                                "Added {} domain(s) to ACME manager for acquisition/renewal: {:?}",
                                pending_domains.len(), pending_domains
                            );
                        } else {
                            info!("No domains require ACME acquisition or renewal at startup");
                        }
                    }
                    
                    // Create ACME manager arc after adding pending domains
                    let acme_manager_arc = Arc::new(acme_manager);

                    // Eagerly install any existing ACME certificates into the TLS manager
                    let existing_certs = acme_manager_arc.list_certificates().await;
                    if !existing_certs.is_empty() {
                        info!(
                            "Installing {} existing ACME certificate(s) into TLS manager for service '{}'",
                            existing_certs.len(), vhost_config.name
                        );
                        for cert in existing_certs {
                            if let Some(primary) = cert.primary_domain() {
                                if vhost_config.domains.contains_key(primary.as_str()) {
                                    match certificate_manager
                                        .reload_domain_certificate_with_paths(&primary, cert.cert_path.clone(), cert.key_path.clone())
                                        .await
                                    {
                                        Ok(()) => info!("Installed existing certificate for domain '{}'", primary),
                                        Err(e) => error!("Failed to install existing certificate for domain '{}': {}", primary, e),
                                    }
                                } else {
                                    debug!("Existing certificate '{}' not mapped to a configured domain; skipping", primary);
                                }
                            }
                        }
                    }

                    // Subscribe to ACME certificate change events and hot-reload TLS
                    {
                        let mut rx = acme_manager_arc.subscribe();
                        let cert_mgr = Arc::clone(&certificate_manager);
                        let service_name = vhost_config.name.clone();
                        let configured_domains = vhost_config.domains.clone();
                        tokio::spawn(async move {
                            loop {
                                match rx.recv().await {
                                    Ok(CertEvent::Installed { domain, cert_path, key_path }) => {
                                        if configured_domains.contains_key(domain.as_str()) {
                                            match cert_mgr
                                                .reload_domain_certificate_with_paths(&domain, cert_path.clone(), key_path.clone())
                                                .await
                                            {
                                                Ok(()) => info!(
                                                    "[{}] Hot-reloaded installed certificate for domain '{}'",
                                                    service_name, domain
                                                ),
                                                Err(e) => error!(
                                                    "[{}] Failed to hot-reload installed certificate for '{}': {}",
                                                    service_name, domain, e
                                                ),
                                            }
                                        } else {
                                            debug!(
                                                "[{}] Cert installed for unconfigured domain '{}' — ignoring",
                                                service_name, domain
                                            );
                                        }
                                    }
                                    Ok(CertEvent::Renewed { domain, cert_path, key_path }) => {
                                        if configured_domains.contains_key(domain.as_str()) {
                                            match cert_mgr
                                                .reload_domain_certificate_with_paths(&domain, cert_path.clone(), key_path.clone())
                                                .await
                                            {
                                                Ok(()) => info!(
                                                    "[{}] Hot-reloaded renewed certificate for domain '{}'",
                                                    service_name, domain
                                                ),
                                                Err(e) => error!(
                                                    "[{}] Failed to hot-reload renewed certificate for '{}': {}",
                                                    service_name, domain, e
                                                ),
                                            }
                                        } else {
                                            debug!(
                                                "[{}] Cert renewed for unconfigured domain '{}' — ignoring",
                                                service_name, domain
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        // Receiver errors only occur on channel closed or lag; continue listening
                                        debug!(
                                            "[{}] Cert event receiver error: {:?} — continuing",
                                            service_name, e
                                        );
                                    }
                                }
                            }
                        });
                    }

                    Some(acme_manager_arc)
                }
            } else {
                info!("Auto SSL domains detected but ACME is disabled - using bootstrap certificates only");
                None
            }
        } else if Self::has_auto_ssl_domains(&vhost_config) {
            // Downgrade to info: some deployments may pre-provision certs without ACME
            info!("Auto SSL domains detected but no ACME configuration provided - using bootstrap certificates only");
            None
        } else {
            None
        };

        Ok(Self {
            certificate_manager,
            vhost_router,
            vhost_config,
            cache_service,
            acme_manager,
            acme_triggered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Extract SNI domain from the TLS connection during handshake
    /// This method attempts to extract the Server Name Indication from the TLS session
    // SNI extraction is now handled by the TLS callback in certificate_manager.rs



    /// Get the service name for logging and identification
    pub fn get_service_name(&self) -> &str {
        &self.vhost_config.name
    }

    /// Get the number of configured domains
    pub fn get_domain_count(&self) -> usize {
        self.vhost_config.domains.len()
    }

    /// Create SNI TLS settings for listeners
    pub async fn create_sni_tls_settings(&self) -> Result<pingora::listeners::tls::TlsSettings> {
        self.certificate_manager.create_sni_tls_settings().await
            .map_err(|e| {
                error!("Failed to create SNI TLS settings for service '{}': {}", 
                       self.vhost_config.name, e);
                Box::new(e)
            })
    }

    /// Check if any domains in the VHost configuration have auto_ssl enabled
    fn has_auto_ssl_domains(vhost_config: &VHostConfig) -> bool {
        // Check default domain
        if let Some(ssl_config) = &vhost_config.default_domain.ssl_config {
            if ssl_config.auto_ssl {
                return true;
            }
        }
        
        // Check all configured domains
        for domain_config in vhost_config.domains.values() {
            if let Some(ssl_config) = &domain_config.ssl_config {
                if ssl_config.auto_ssl {
                    return true;
                }
            }
        }
        
        false
    }

}



#[async_trait]
impl ProxyHttp for VHostProxyService {
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
            // Initialize VHost-related fields
            selected_domain: None,
            sni_domain: None,
            host_header: None,
            certificate_used: None,
            request_id: None,
        }
    }

    /// Handle the upstream peer selection phase
    /// This is where we extract the Host header and route to the appropriate upstream
    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        debug!("VHost upstream peer selection started for service '{}'", self.vhost_config.name);
        
        // Extract Host header from the request for routing decisions
        let host_domain = VHostRouter::extract_host_header(session.req_header());
        
        // Store the Host header in context for logging and debugging
        ctx.host_header = host_domain.clone();
        
        // Log the routing decision with appropriate level based on presence of Host header
        match &host_domain {
            Some(domain) => {
                info!("VHost routing: extracted Host header '{}' - routing to domain-specific upstream (service: {})", 
                      domain, self.vhost_config.name);
            }
            None => {
                // Handle missing Host header gracefully - this is normal for HTTP/1.0 or malformed requests
                info!("VHost routing: no Host header found in request - using default upstream (service: {}) - this is normal for HTTP/1.0 or direct IP access", 
                      self.vhost_config.name);
            }
        }
        
        // Select appropriate upstream based on domain mapping using VHostRouter
        let upstream = self.vhost_router.select_upstream_peer(host_domain.clone()).await
            .map_err(|e| {
                // Enhanced error handling with more context
                match &host_domain {
                    Some(domain) => {
                        let error_msg = format!("VHost upstream selection failed for domain '{}' in service '{}': {} - this may indicate a configuration issue", 
                                               domain, self.vhost_config.name, e);
                        error!("{}", error_msg);
                        error!("Please verify that domain '{}' has valid upstream backends configured", domain);
                        pingora::Error::new_str("Failed to select upstream for domain")
                    }
                    None => {
                        let error_msg = format!("VHost default upstream selection failed for service '{}': {} - this is a critical configuration error", 
                                               self.vhost_config.name, e);
                        error!("{}", error_msg);
                        error!("Please verify that the default domain configuration has valid upstream backends");
                        pingora::Error::new_str("Failed to select default upstream")
                    }
                }
            })?;
        
        // Store selected domain and upstream information in context for logging
        ctx.selected_domain = host_domain.clone();
        ctx.upstream_addr = Some(upstream._address.to_string());
        
        // Log successful upstream selection with detailed information
        match &ctx.selected_domain {
            Some(domain) => {
                info!("VHost upstream selection successful: selected {} for domain '{}' (service: {})", 
                      upstream._address, domain, self.vhost_config.name);
            }
            None => {
                info!("VHost upstream selection successful: selected default upstream {} for request without Host header (service: {})", 
                      upstream._address, self.vhost_config.name);
            }
        }
        
        Ok(upstream)
    }

    /// Handle request filtering phase
    /// This is where we extract SNI information and prepare for certificate selection
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        // Note: SNI extraction and certificate selection is handled by the TLS callback
        // in certificate_manager.rs during the TLS handshake, not here in the HTTP layer.
        // ACME issuance/renewal is now handled by the ACME manager's daily background job
        // scheduled at local midnight to avoid per-request overhead.

        // Handle ACME HTTP-01 challenges first (before cache lookup)
        let uri_path = session.req_header().uri.path();
        
        // Check if this is an ACME challenge request
        if uri_path.starts_with("/.well-known/acme-challenge/") {
            let token = uri_path.trim_start_matches("/.well-known/acme-challenge/");
            
            info!("ACME challenge request received for token: {} (service: {})", token, self.vhost_config.name);
            
            if let Some(acme_manager) = &self.acme_manager {
                // Try to get the challenge response from ACME manager
                if let Some(challenge_response) = acme_manager.get_challenge_response(token).await {
                    info!("Serving ACME challenge response for token: {} (length: {} bytes)", token, challenge_response.len());
                    
                    // Create a successful HTTP response with the challenge content
                    let mut resp = ResponseHeader::build(200, None).unwrap();
                    resp.insert_header("content-type", "text/plain").unwrap();
                    resp.insert_header("content-length", challenge_response.len().to_string()).unwrap();
                    resp.insert_header("cache-control", "no-cache").unwrap();
                    
                    // Send the response header
                    session.write_response_header(Box::new(resp), false).await?;
                    
                    // Send the challenge response body
                    session.write_response_body(Some(challenge_response.into()), true).await?;
                    
                    // Terminate the request - we've handled it completely
                    return Ok(true);
                } else {
                    warn!("ACME challenge request for unknown token: {} (no ACME manager or token not found)", token);
                }
            } else {
                warn!("ACME challenge request received but no ACME manager available for token: {}", token);
            }
            
            // Return 404 for unknown challenge tokens or when ACME manager is not available
            info!("Returning 404 for ACME challenge token: {}", token);
            let mut resp = ResponseHeader::build(404, None).unwrap();
            resp.insert_header("content-type", "text/plain").unwrap();
            resp.insert_header("content-length", "9".to_string()).unwrap();
            
            session.write_response_header(Box::new(resp), false).await?;
            session.write_response_body(Some("Not Found".into()), true).await?;
            
            return Ok(true);
        }
        
        // Handle cache lookup if cache service is available
        if let Some(cache_service) = &self.cache_service {
            match crate::proxy::pipeline::RequestPipeline::process_cache_lookup(
                session,
                ctx,
                cache_service,
            ).await? {
                Some(should_terminate) => {
                    return Ok(should_terminate);
                }
                None => {
                    debug!("Cache lookup completed, continuing with upstream request");
                }
            }
        } else {
            ctx.cache_status = Some("BYPASS".to_string());
        }
        
        // Continue with normal request processing (don't terminate the request)
        Ok(false)
    }

    /// Handle upstream request filtering
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        _upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // No additional upstream request filtering needed for basic VHost functionality
        Ok(())
    }

    /// Handle upstream response body filtering
    /// This is where we collect response body data for caching and apply compression
    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        data: &mut Option<bytes::Bytes>,
        eof: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Handle compression and caching in a more efficient way
        if let Some(chunk) = data.as_mut() {
            if !chunk.is_empty() {
                let original_len = chunk.len();
                
                // Store data for caching if needed (before compression)
                if ctx.should_cache_response {
                    if let Some(body_chunks) = &mut ctx.cache_body {
                        tracing::debug!(
                            "VHost collecting response body chunk for cache: {} bytes (service: {})",
                            original_len,
                            self.vhost_config.name
                        );
                        body_chunks.push(chunk.clone());
                    }
                }
                
                // Apply compression if needed
                if let Some(encoding) = ctx.compression_encoding {
                    // Use compression for the response data
                    match crate::proxy::compression::ClientCompression::compress(chunk, encoding) {
                        Ok(compressed) => {
                            *chunk = compressed;
                            tracing::debug!(
                                "VHost compressed response chunk: {} -> {} bytes with {:?} (service: {})",
                                original_len,
                                chunk.len(),
                                encoding,
                                self.vhost_config.name
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "VHost compression failed for chunk: {} (service: {})",
                                e,
                                self.vhost_config.name
                            );
                            // Reset compression encoding on failure to avoid header mismatch
                            ctx.compression_encoding = None;
                        }
                    }
                }
            }
        }

        // Handle end of file for caching
        if eof && ctx.should_cache_response {
            if let (Some(body_chunks), Some(req_header), Some(resp_header)) = 
                (&mut ctx.cache_body, &ctx.cache_req_header, &ctx.cache_resp_header) {
                
                if !body_chunks.is_empty() {
                    let total_size: usize = body_chunks.iter().map(|chunk| chunk.len()).sum();
                    tracing::debug!(
                        "VHost collected complete response body for cache: {} bytes (service: {})",
                        total_size,
                        self.vhost_config.name
                    );

                    let mut combined_body = bytes::BytesMut::with_capacity(total_size);
                    for chunk in body_chunks.drain(..) {
                        combined_body.extend_from_slice(&chunk);
                    }
                    let body = combined_body.freeze();

                    // Prepare response header for caching (without compression encoding)
                    let mut cache_resp_header = resp_header.clone();
                    cache_resp_header.remove_header("content-encoding");

                    // Clone cache service to avoid borrowing self
                    if let Some(cache_service) = self.cache_service.clone() {
                        let storage = cache_service.bucket().storage();
                        let bucket = cache_service.bucket().clone();
                        let req_header = req_header.clone();
                        let resp_header = cache_resp_header;
                        let service_name = self.vhost_config.name.clone();
                        
                        let url = req_header.uri.to_string();
                        let method = req_header.method.as_str().to_string();

                        tokio::spawn(async move {
                            match storage
                                .put_cache(&req_header, &resp_header, body, Some(&bucket))
                                .await
                            {
                                Ok(_) => {
                                    tracing::debug!(
                                        "VHost successfully cached response: {} {} (service: {})",
                                        method,
                                        url,
                                        service_name
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "VHost failed to cache response: {} (service: {})",
                                        e,
                                        service_name
                                    );
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
        Ok(())
    }

    /// Handle upstream response filtering
    /// This is where we log VHost-specific information and can add custom headers
    fn upstream_response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Process cache status and headers using the ResponsePipeline
        let _cache_status = crate::proxy::pipeline::ResponsePipeline::process_cache_status(
            session,
            upstream_response,
            ctx,
            self.cache_service.as_ref().map(|v| &**v), // Now VHost service has cache service
            self.vhost_config.browser_cache_enabled,
            if self.vhost_config.browser_cache_enabled {
                Some(self.vhost_config.browser_cache_ttl)
            } else {
                None
            },
        );

        // Process compression setup - this was missing!
        crate::proxy::pipeline::ResponsePipeline::process_compression_setup(
            session,
            upstream_response,
            ctx,
        );

        // Prepare response for caching if should_cache_response is true
        if ctx.should_cache_response {
            // Store response header for caching
            ctx.cache_resp_header = Some(upstream_response.clone());
            
            // Initialize cache body collection
            ctx.cache_body = Some(Vec::new());
            
            tracing::debug!(
                "VHost prepared response for caching: status {} (service: {})",
                upstream_response.status,
                self.vhost_config.name
            );
        }

        // Add comprehensive logging for VHost-specific information
        info!(
            "VHost response completed - service: '{}', domain: {:?}, sni: {:?}, upstream: {:?}, status: {}, certificate: {:?}",
            self.vhost_config.name,
            ctx.selected_domain,
            ctx.sni_domain,
            ctx.upstream_addr,
            upstream_response.status,
            ctx.certificate_used
        );

        // Log additional details for debugging and monitoring
        match (&ctx.selected_domain, &ctx.sni_domain) {
            (Some(domain), Some(sni)) if domain == sni => {
                debug!("VHost response: domain and SNI match ('{}') - optimal configuration (service: {})", 
                       domain, self.vhost_config.name);
            }
            (Some(domain), Some(sni)) => {
                warn!("VHost response: domain ('{}') and SNI ('{}') mismatch - potential configuration issue (service: {})", 
                      domain, sni, self.vhost_config.name);
            }
            (Some(domain), None) => {
                debug!("VHost response: HTTP request with domain '{}' (no SNI) (service: {})", 
                       domain, self.vhost_config.name);
            }
            (None, Some(sni)) => {
                warn!("VHost response: HTTPS request with SNI '{}' but no Host header - unusual (service: {})", 
                      sni, self.vhost_config.name);
            }
            (None, None) => {
                debug!("VHost response: request without domain or SNI - using defaults (service: {})", 
                       self.vhost_config.name);
            }
        }


        // Process access logging
        self.process_access_logging(session, upstream_response, ctx);
        
        Ok(())
    }

    /// Handle upstream connection failures
    /// This method is called when the proxy fails to connect to the upstream server
    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<pingora_core::Error>,
    ) -> Box<pingora_core::Error> {
        // Extract request information for logging
        let method = _session.req_header().method.as_str();
        let uri = _session.req_header().uri.to_string();
        let host = ctx.host_header.as_deref();
        
        // Determine appropriate status code based on error type
        let status_code = if e.to_string().contains("timeout") {
            504 // Gateway Timeout
        } else {
            502 // Bad Gateway for connection failures
        };

        // Log the connection failure with standardized format
        crate::logging::log_proxy_error(
            method,
            &uri,
            host,
            &format!("Fail to connect: {}", e),
            Some(status_code),
            Some(1),   // First attempt
            Some(false), // Not retrying by default
            ctx.request_id.as_deref().map(|id| crate::logging::RequestTraceContext {
                request_id: id.to_string(),
                span_id: crate::logging::generate_request_id(),
                trace_id: id.to_string(),
                custom_attributes: std::collections::HashMap::new(),
            }).as_ref(),
            false, // Use text format to match pingora logs
        );

        // Log additional context for debugging
        error!(
            "VHost upstream connection failed: service='{}', domain={:?}, upstream={}, error={}",
            self.vhost_config.name,
            ctx.selected_domain,
            _peer._address,
            e
        );

        // Explicitly mark this error as non-retryable to ensure Pingora returns a 5xx
        e.set_retry(false);

        e
    }

    /// Handle errors that occur while proxying the request
    /// This method is called when errors occur during the proxying process after connection is established
    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        mut e: Box<pingora_core::Error>,
        ctx: &mut Self::CTX,
        _retry: bool,
    ) -> Box<pingora_core::Error> {
        // Extract request information for logging
        let method = session.req_header().method.as_str();
        let uri = session.req_header().uri.to_string();
        let host = ctx.host_header.as_deref();
        
        // Determine appropriate status code based on error type
        let status_code = if e.to_string().contains("timeout") {
            504 // Gateway Timeout
        } else if e.to_string().contains("connection") {
            502 // Bad Gateway
        } else {
            502 // Default to Bad Gateway for proxy errors
        };

        // Log the proxy error with standardized format
        crate::logging::log_proxy_error(
            method,
            &uri,
            host,
            &format!("Fail to proxy: {}", e),
            Some(status_code),
            Some(1),   // First attempt
            Some(false), // Not retrying by default
            ctx.request_id.as_deref().map(|id| crate::logging::RequestTraceContext {
                request_id: id.to_string(),
                span_id: crate::logging::generate_request_id(),
                trace_id: id.to_string(),
                custom_attributes: std::collections::HashMap::new(),
            }).as_ref(),
            false, // Use text format to match pingora logs
        );

        // Log additional context for debugging
        error!(
            "VHost proxy error: service='{}', domain={:?}, upstream={}, error={}",
            self.vhost_config.name,
            ctx.selected_domain,
            peer._address,
            e
        );

        // Explicitly mark this error as non-retryable to ensure Pingora returns a 5xx
        e.set_retry(false);

        e
    }
}

impl VHostProxyService {
    /// Process access logging for VHost requests
    fn process_access_logging(
        &self,
        session: &mut Session,
        upstream_response: &ResponseHeader,
        ctx: &mut StryvoContext,
    ) {
        // Calculate elapsed time since request started
        let elapsed = if ctx.selector_buf.len() >= 16 {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&ctx.selector_buf[0..16]);
            let start_nanos = u128::from_ne_bytes(bytes);
            let current_nanos = std::time::Instant::now().elapsed().as_nanos();
            std::time::Duration::from_nanos((current_nanos - start_nanos) as u64)
        } else {
            std::time::Duration::from_secs(0)
        };

        // Get upstream information from the context
        let upstream = ctx
            .upstream_addr
            .as_ref()
            .map(String::as_str)
            .unwrap_or("unknown");

        // Extract HTTP protocol version
        let http_version = format!("{:?}", session.req_header().version);

        // Extract User-Agent header
        let user_agent = session
            .req_header()
            .headers
            .get("user-agent")
            .map(|v| v.to_str().unwrap_or("unknown"))
            .unwrap_or("unknown");

        // Extract Referer header
        let referer = session
            .req_header()
            .headers
            .get("referer")
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("");

        // Generate request ID if not already present
        let request_id = ctx.request_id.clone().unwrap_or_else(|| {
            let id = crate::logging::generate_request_id();
            ctx.request_id = Some(id.clone());
            id
        });

        // Get client IP from session
        let client_ip = session.client_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Calculate response size (approximate)
        let bytes_sent = upstream_response.headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        // Extract bytes received from request headers
        let bytes_received = session.req_header().headers.get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        // Extract SSL information if available
        // TODO: Fix SSL access for Pingora 0.6.0 - session.ssl() method may not be available
        let (ssl_protocol, _ssl_cipher): (Option<String>, Option<String>) = {
            // Temporarily disabled until find the correct API for Pingora 0.6.0
            (None, None)
        };

        // Collect useful custom headers
        let mut custom_headers = std::collections::HashMap::new();
        
        // Add X-Forwarded-For if present
        if let Some(xff) = session.req_header().headers.get("x-forwarded-for") {
            if let Ok(xff_str) = xff.to_str() {
                custom_headers.insert("x-forwarded-for".to_string(), xff_str.to_string());
            }
        }
        
        // Add X-Real-IP if present
        if let Some(real_ip) = session.req_header().headers.get("x-real-ip") {
            if let Ok(real_ip_str) = real_ip.to_str() {
                custom_headers.insert("x-real-ip".to_string(), real_ip_str.to_string());
            }
        }
        
        // Add Accept-Encoding for compression analysis
        if let Some(accept_encoding) = session.req_header().headers.get("accept-encoding") {
            if let Ok(encoding_str) = accept_encoding.to_str() {
                custom_headers.insert("accept-encoding".to_string(), encoding_str.to_string());
            }
        }
        
        // Add Content-Encoding from response
        if let Some(content_encoding) = upstream_response.headers.get("content-encoding") {
            if let Ok(encoding_str) = content_encoding.to_str() {
                custom_headers.insert("content-encoding".to_string(), encoding_str.to_string());
            }
        }
        
        // Add Content-Type for analysis
        if let Some(content_type) = upstream_response.headers.get("content-type") {
            if let Ok(type_str) = content_type.to_str() {
                custom_headers.insert("content-type".to_string(), type_str.to_string());
            }
        }

        // Convert headers to HashMap for comprehensive logging
        let mut headers_map = std::collections::HashMap::new();
        
        // Add request headers
        for (name, value) in session.req_header().headers.iter() {
            if let Ok(value_str) = value.to_str() {
                headers_map.insert(name.as_str().to_lowercase(), value_str.to_string());
            }
        }
        
        // Add response headers for analysis
        for (name, value) in upstream_response.headers.iter() {
            if let Ok(value_str) = value.to_str() {
                let key = format!("response-{}", name.as_str().to_lowercase());
                headers_map.insert(key, value_str.to_string());
            }
        }
        
        // Add custom headers we collected
        for (key, value) in &custom_headers {
            headers_map.insert(key.clone(), value.clone());
        }

        // Use comprehensive logging function with all available data
        let entry = crate::logging::create_comprehensive_access_entry(
            session.req_header().method.as_str(),
            &session.req_header().uri.to_string(),
            upstream_response.status.as_u16(),
            upstream,
            elapsed,
            &http_version,
            user_agent,
            referer,
            ctx.cache_status.as_ref().map(String::as_str).unwrap_or(""),
            &headers_map,
            Some(&client_ip),
            Some(bytes_sent),
            bytes_received,
            ssl_protocol.as_deref(),
            bytes_received, // body_size
            Some(elapsed), // upstream_response_time
            Some(custom_headers.clone()),
        );

        // Use enhanced access logging with structured format for better analysis
        crate::logging::log_access_enhanced(&entry, true);

        // Create trace context and performance metrics
        let trace_context = crate::logging::RequestTraceContext {
            request_id: request_id.clone(),
            span_id: crate::logging::generate_request_id(),
            trace_id: request_id.clone(),
            custom_attributes: std::collections::HashMap::new(),
        };

        // Calculate compression ratio if both encodings are present
        let compression_ratio = if custom_headers.contains_key("accept-encoding") && 
                                  custom_headers.contains_key("content-encoding") {
            // Simple heuristic: if content is compressed, estimate 70% compression
            Some(0.7)
        } else {
            None
        };
        
        // Check if connection might be reused (heuristic based on HTTP version)
        let connection_reused = http_version.contains("1.1") || http_version.contains("2");
        
        // Estimate upstream response time (could be enhanced with actual timing)
        let upstream_response_time = if elapsed.as_millis() > 50 {
            Some((elapsed.as_millis() as u64).saturating_sub(10)) // Subtract estimated proxy overhead
        } else {
            Some(elapsed.as_millis() as u64)
        };
        
        // Estimate SSL handshake time for HTTPS requests
        let ssl_handshake_time = if ssl_protocol.is_some() {
            Some(20) // Typical SSL handshake time in ms
        } else {
            None
        };

        let perf_metrics = crate::logging::PerformanceMetrics {
            request_processing_time_ms: elapsed.as_millis() as u64,
            upstream_connect_time_ms: Some(5), // Estimated connection time
            upstream_response_time_ms: upstream_response_time,
            cache_lookup_time_ms: if ctx.cache_status.is_some() { Some(2) } else { None },
            ssl_handshake_time_ms: ssl_handshake_time,
            dns_lookup_time_ms: Some(3), // Estimated DNS lookup time
            total_bytes_sent: bytes_sent,
            total_bytes_received: bytes_received.unwrap_or(0),
            connection_reused,
            compression_ratio,
        };

        // Log performance metrics if enabled
        crate::logging::log_performance_metrics(&trace_context, &perf_metrics, true);
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::config::internal::{DomainConfig, TlsConfig, UpstreamOptions};
    use std::path::PathBuf;

    fn create_test_vhost_config() -> VHostConfig {
        use crate::config::internal::{ListenerConfig, ListenerKind};
        
        let mut domains = HashMap::new();
        
        // Use paths relative to the workspace root (where tests run from)
        // Prefer certs in assets/certs/, but support legacy locations too
        let cert_path = {
            let candidates = [
                "source/stryvo/assets/certs/test.local.crt",
                "source/stryvo/assets/test.crt",
                "assets/certs/test.local.crt",
                "assets/test.crt",
            ];
            candidates
                .iter()
                .map(|p| PathBuf::from(p))
                .find(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("source/stryvo/assets/certs/test.local.crt"))
        };
        
        let key_path = {
            let candidates = [
                "source/stryvo/assets/certs/test.local.key",
                "source/stryvo/assets/test.key",
                "assets/certs/test.local.key",
                "assets/test.key",
            ];
            candidates
                .iter()
                .map(|p| PathBuf::from(p))
                .find(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("source/stryvo/assets/certs/test.local.key"))
        };
        
        domains.insert("example.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.10:80", false, String::new())],
            ssl_config: Some(TlsConfig {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                auto_ssl: false,
            }),
            upstream_options: UpstreamOptions::default(),
        });

        VHostConfig {
            name: "test-vhost".to_string(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "0.0.0.0:8080".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains,
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("192.168.1.100:80", false, String::new())],
                ssl_config: Some(TlsConfig {
                    cert_path,
                    key_path,
                    auto_ssl: false,
                }),
                upstream_options: UpstreamOptions::default(),
            },
            path_control: crate::config::internal::PathControl::default(),
            rate_limiting: crate::config::internal::RateLimitingConfig::default(),
            cache_enabled: false,
            cache_ttl: 300,
            browser_cache_enabled: false,
            browser_cache_ttl: 3600,
            cache_dir: None,
            max_file_size: None,
            shard_count: None,
        }
    }

    #[tokio::test]
    async fn test_vhost_service_creation() {
        let config = create_test_vhost_config();
        
        // First test the validation separately
        match config.validate() {
            Ok(()) => println!("Configuration validation passed"),
            Err(e) => println!("Configuration validation failed: {}", e),
        }
        
        // This test should succeed since the test certificate files exist
        let result = VHostProxyService::new(config, None).await;
        
        // Print the error for debugging if it fails
        if let Err(ref e) = result {
            println!("VHost service creation failed: {}", e);
        }
        
        // We expect this to succeed with the test certificate files
        assert!(result.is_ok(), "VHost service creation should succeed with valid test certificates");
        
        if let Ok(service) = result {
            assert_eq!(service.vhost_config.name, "test-vhost");
            assert_eq!(service.vhost_config.domains.len(), 1);
            assert!(service.vhost_config.domains.contains_key("example.com"));
            assert_eq!(service.get_service_name(), "test-vhost");
            assert_eq!(service.get_domain_count(), 1);
        }
    }

    #[test]
    fn test_extract_host_header() {
        use pingora_http::RequestHeader;
        use http::Method;
        use crate::proxy::vhost::router::VHostRouter;
        
        let mut req_header = RequestHeader::build(Method::GET, b"/", None).unwrap();
        req_header.insert_header("host", "example.com:443").unwrap();
        
        let host = VHostRouter::extract_host_header(&req_header);
        assert_eq!(host, Some("example.com".to_string()));
    }

    #[test]
    fn test_extract_host_header_no_port() {
        use pingora_http::RequestHeader;
        use http::Method;
        use crate::proxy::vhost::router::VHostRouter;
        
        let mut req_header = RequestHeader::build(Method::GET, b"/", None).unwrap();
        req_header.insert_header("host", "example.com").unwrap();
        
        let host = VHostRouter::extract_host_header(&req_header);
        assert_eq!(host, Some("example.com".to_string()));
    }

    #[test]
    fn test_extract_host_header_missing() {
        use pingora_http::RequestHeader;
        use http::Method;
        use crate::proxy::vhost::router::VHostRouter;
        
        let req_header = RequestHeader::build(Method::GET, b"/", None).unwrap();
        
        let host = VHostRouter::extract_host_header(&req_header);
        assert_eq!(host, None);
    }

    #[tokio::test]
    async fn test_certificate_manager_sni_extraction() {
        use crate::proxy::vhost::certificate_manager::CertificateManager;
        // Test uses the certificate manager's SNI functionality
        
        let config = create_test_vhost_config();
        let cert_manager = match CertificateManager::new(&config).await {
            Ok(manager) => manager,
            Err(e) => {
                println!("Failed to create certificate manager: {}", e);
                panic!("Certificate manager creation failed: {}", e);
            }
        };
        
        // Test that we can create SNI-aware TLS settings
        let tls_settings_result = cert_manager.create_sni_tls_settings().await;
        assert!(tls_settings_result.is_ok());
        
        // Test that we can create domain-specific TLS settings
        let domain_settings_result = cert_manager.create_domain_tls_settings().await;
        assert!(domain_settings_result.is_ok());
        
        if let Ok(domain_settings) = domain_settings_result {
            // Should have settings for the configured domain plus default
            assert!(domain_settings.len() >= 1);
            assert!(domain_settings.contains_key("default"));
        }
    }

    #[tokio::test]
    async fn test_vhost_routing_logic() {
        let config = create_test_vhost_config();
        let service = match VHostProxyService::new(config, None).await {
            Ok(service) => service,
            Err(e) => {
                println!("Failed to create VHost service for routing test: {}", e);
                panic!("VHost service creation failed: {}", e);
            }
        };
        
        // Test routing to configured domain
        let upstream_result = service.vhost_router.select_upstream_peer(Some("example.com".to_string())).await;
        assert!(upstream_result.is_ok());
        
        if let Ok(upstream) = upstream_result {
            assert_eq!(upstream._address.to_string(), "192.168.1.10:80");
        }
        
        // Test routing to unknown domain (should use default)
        let default_upstream_result = service.vhost_router.select_upstream_peer(Some("unknown.com".to_string())).await;
        assert!(default_upstream_result.is_ok());
        
        if let Ok(default_upstream) = default_upstream_result {
            assert_eq!(default_upstream._address.to_string(), "192.168.1.100:80");
        }
        
        // Test routing with no domain (should use default)
        let no_domain_result = service.vhost_router.select_upstream_peer(None).await;
        assert!(no_domain_result.is_ok());
        
        if let Ok(no_domain_upstream) = no_domain_result {
            assert_eq!(no_domain_upstream._address.to_string(), "192.168.1.100:80");
        }
    }

    #[tokio::test]
    async fn test_upstream_peer_selection_with_host_header() {
        use pingora_http::RequestHeader;
        use http::Method;
        
        let config = create_test_vhost_config();
        let service = match VHostProxyService::new(config, None).await {
            Ok(service) => service,
            Err(e) => {
                println!("Failed to create VHost service for upstream peer test: {}", e);
                panic!("VHost service creation failed: {}", e);
            }
        };
        
        // Create a mock session with Host header
        let mut req_header = RequestHeader::build(Method::GET, b"/test", None).unwrap();
        req_header.insert_header("host", "example.com").unwrap();
        
        // Note: We can't easily test the full upstream_peer method without a complete Session
        // but we can test the VHostRouter logic directly which is what upstream_peer uses
        
        // Test Host header extraction
        let host = crate::proxy::vhost::router::VHostRouter::extract_host_header(&req_header);
        assert_eq!(host, Some("example.com".to_string()));
        
        // Test upstream selection based on extracted host
        let upstream_result = service.vhost_router.select_upstream_peer(host).await;
        assert!(upstream_result.is_ok());
        
        if let Ok(upstream) = upstream_result {
            assert_eq!(upstream._address.to_string(), "192.168.1.10:80");
        }
    }

    #[tokio::test]
    async fn test_upstream_peer_selection_graceful_fallback() {
        use pingora_http::RequestHeader;
        use http::Method;
        
        let config = create_test_vhost_config();
        let service = match VHostProxyService::new(config, None).await {
            Ok(service) => service,
            Err(e) => {
                println!("Failed to create VHost service for fallback test: {}", e);
                panic!("VHost service creation failed: {}", e);
            }
        };
        
        // Test with missing Host header (graceful handling)
        let req_header = RequestHeader::build(Method::GET, b"/test", None).unwrap();
        let host = crate::proxy::vhost::router::VHostRouter::extract_host_header(&req_header);
        assert_eq!(host, None);
        
        // Test upstream selection with no host (should use default gracefully)
        let upstream_result = service.vhost_router.select_upstream_peer(host).await;
        assert!(upstream_result.is_ok());
        
        if let Ok(upstream) = upstream_result {
            assert_eq!(upstream._address.to_string(), "192.168.1.100:80");
        }
        
        // Test with invalid/unknown domain (should use default gracefully)
        let mut req_header_invalid = RequestHeader::build(Method::GET, b"/test", None).unwrap();
        req_header_invalid.insert_header("host", "invalid-domain.com").unwrap();
        
        let invalid_host = crate::proxy::vhost::router::VHostRouter::extract_host_header(&req_header_invalid);
        assert_eq!(invalid_host, Some("invalid-domain.com".to_string()));
        
        let invalid_upstream_result = service.vhost_router.select_upstream_peer(invalid_host).await;
        assert!(invalid_upstream_result.is_ok());
        
        if let Ok(upstream) = invalid_upstream_result {
            assert_eq!(upstream._address.to_string(), "192.168.1.100:80");
        }
    }

    #[test]
    fn test_stryvo_context_vhost_fields() {
        use crate::proxy::StryvoContext;
        
        // Test that StryvoContext can be created with vhost fields
        let mut ctx = StryvoContext {
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
            // VHost-related fields
            selected_domain: Some("example.com".to_string()),
            sni_domain: Some("example.com".to_string()),
            host_header: Some("example.com:443".to_string()),
            certificate_used: Some("certificate-for-example.com".to_string()),
            request_id: None,
        };
        
        // Test that we can access and modify vhost fields
        assert_eq!(ctx.selected_domain, Some("example.com".to_string()));
        assert_eq!(ctx.sni_domain, Some("example.com".to_string()));
        assert_eq!(ctx.host_header, Some("example.com:443".to_string()));
        assert_eq!(ctx.certificate_used, Some("certificate-for-example.com".to_string()));
        
        // Test that we can modify the fields
        ctx.selected_domain = Some("api.example.com".to_string());
        ctx.sni_domain = Some("api.example.com".to_string());
        ctx.host_header = Some("api.example.com".to_string());
        ctx.certificate_used = Some("certificate-for-api.example.com".to_string());
        
        assert_eq!(ctx.selected_domain, Some("api.example.com".to_string()));
        assert_eq!(ctx.sni_domain, Some("api.example.com".to_string()));
        assert_eq!(ctx.host_header, Some("api.example.com".to_string()));
        assert_eq!(ctx.certificate_used, Some("certificate-for-api.example.com".to_string()));
    }
}