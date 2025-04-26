//! Request processing pipeline
//!
//! This module implements a Pipeline pattern for HTTP request processing,

use std::time::{Duration, Instant};

use pingora::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;

use crate::proxy::{
    cache::service::CacheService,
    cache::CacheResult,
    compression::{self, CompressionEncoding},
    header_utils::HeaderUtils,
    rate_limiting::Outcome,
    StryvoContext,
};

/// Pipeline for processing HTTP requests
pub struct RequestPipeline;

impl RequestPipeline {
    /// Process cache lookup stage
    ///
    /// This handles checking if a request can be served from cache
    /// and returns early with the cached response if possible.
    pub async fn process_cache_lookup(
        session: &mut Session,
        ctx: &mut StryvoContext,
        cache_service: &CacheService,
    ) -> Result<Option<bool>> {
        let req_header = session.req_header();
        let cache_key = format!("{} {}", req_header.method.as_str(), req_header.uri);
        ctx.cache_req_header = Some(req_header.clone());
    
        tracing::debug!(cache_key = %cache_key, "Checking cache");
        match cache_service.bucket().storage().get_cache(req_header, Some(cache_service.bucket())).await {
            Ok(CacheResult::Hit {
                body,
                headers,
                status,
            }) => {
                ctx.cache_status = Some("HIT".to_string());
                tracing::debug!(cache_key = %cache_key, "Cache hit");
    
                // Pre-allocate header capacity
                let mut resp_header = ResponseHeader::build(status, Some(headers.len() + 3))?;
    
                // Copy relevant cached headers
                for (key, value) in headers {
                    let key_lower = key.to_lowercase();
                    if key_lower != "content-encoding" && key_lower != "content-length" {
                        resp_header.append_header(key, value)?;
                    }
                }
    
                // Determine body and compression
                let (body_to_send, encoding) = match HeaderUtils::get_preferred_encoding(req_header) {
                    CompressionEncoding::Identity => (body, CompressionEncoding::Identity),
                    encoding => match compression::ClientCompression::compress(&body, encoding) {
                        Ok(compressed) => {
                            tracing::debug!(
                                cache_key = %cache_key,
                                original_size = body.len(),
                                compressed_size = compressed.len(),
                                "Compressed cached response"
                            );
                            (compressed, encoding)
                        }
                        Err(e) => {
                            tracing::warn!(
                                cache_key = %cache_key,
                                error = %e,
                                "Failed to compress cached response"
                            );
                            (body, CompressionEncoding::Identity)
                        }
                    },
                };
    
                // Update compression headers
                if encoding != CompressionEncoding::Identity {
                    if let Err(e) = HeaderUtils::update_compression_headers(&mut resp_header, encoding, None) {
                        tracing::warn!(
                            cache_key = %cache_key,
                            error = %e,
                            "Failed to update compression headers"
                        );
                    }
                }
    
                // Set Content-Length for non-empty body
                if !body_to_send.is_empty() {
                    resp_header.append_header("content-length", body_to_send.len().to_string())?;
                }
    
                // Set cache headers
                if let Err(e) = HeaderUtils::set_cache_headers(
                    &mut resp_header,
                    "HIT",
                    ctx.browser_cache_enabled,
                    ctx.browser_cache_ttl,
                ) {
                    tracing::warn!(
                        cache_key = %cache_key,
                        error = %e,
                        "Failed to set cache headers"
                    );
                }
    
                tracing::debug!(cache_key = %cache_key, headers = ?resp_header.headers, "Sending HIT response");
                session
                    .downstream_session
                    .write_response_header(Box::new(resp_header))
                    .await?;
                session
                    .downstream_session
                    .write_response_body(body_to_send, true)
                    .await?;
                Ok(Some(true))
            }
            Ok(CacheResult::Miss) => {
                ctx.cache_status = Some("MISS".to_string());
                ctx.should_cache_response = true;
                tracing::debug!(cache_key = %cache_key, "Cache miss");
                cache_service.enable(session);
                Ok(None)
            }
            Err(e) => {
                ctx.cache_status = Some("ERROR".to_string());
                tracing::warn!(cache_key = %cache_key, error = %e, "Cache lookup error");
                cache_service.enable(session);
                Ok(None)
            }
        }
    }

    /// Process rate limiting stage
    ///
    /// This handles checking if a request should be rate limited
    /// and returns early with a 429 response if needed.
    pub async fn process_rate_limiting(
        session: &mut Session,
        _ctx: &mut StryvoContext,
        request_filter_stage_multi: &[crate::proxy::rate_limiting::multi::MultiRaterInstance],
        request_filter_stage_single: &[crate::proxy::rate_limiting::single::SingleInstance],
    ) -> Result<Option<bool>> {
        let multis = request_filter_stage_multi
            .iter()
            .filter_map(|l| l.get_ticket(session));
        let singles = request_filter_stage_single
            .iter()
            .filter_map(|l| l.get_ticket(session));
        if singles
            .chain(multis)
            .any(|t| t.now_or_never() == Outcome::Declined)
        {
            tracing::trace!("Rejecting due to rate limiting failure");
            crate::logging::log_error(
                session.req_header().method.as_str(),
                session.req_header().uri.to_string().as_str(),
                "Rate limiting rejection",
            );
            session.downstream_session.respond_error(429).await;
            return Ok(Some(true));
        }
        Ok(None)
    }

    /// Process request filters stage
    ///
    /// This applies all configured request filters
    pub async fn process_request_filters(
        session: &mut Session,
        ctx: &mut StryvoContext,
        request_filters: &[Box<dyn crate::proxy::request_filters::RequestFilterMod>],
    ) -> Result<Option<bool>> {
        for filter in request_filters {
            match filter.request_filter(session, ctx).await {
                _o @ Ok(true) => return Ok(Some(true)),
                e @ Err(_) => {
                    if let Err(err) = &e {
                        crate::logging::log_error(
                            session.req_header().method.as_str(),
                            session.req_header().uri.to_string().as_str(),
                            &format!("Request filter error: {}", err),
                        );
                    }
                    return e.map(Some);
                }
                Ok(false) => {}
            }
        }
        Ok(None)
    }
}

/// Pipeline for processing HTTP responses
pub struct ResponsePipeline;

impl ResponsePipeline {
    /// Process cache status and headers
    ///
    /// This determines if a response should be cached and sets appropriate headers
    pub fn process_cache_status(
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut StryvoContext,
        cache_service: Option<&CacheService>,
        browser_cache_enabled: bool,
        browser_cache_ttl: Option<u64>,
    ) -> String {
        // Determine cache status
        let cache_status = if let Some(_cache_service) = cache_service {
            if let Some(status) = &ctx.cache_status {
                if status == "MISS" && upstream_response.status.as_u16() == 200 {
                    ctx.should_cache_response = true;
                    ctx.cache_resp_header = Some(upstream_response.clone());
                }
                status.as_str()
            } else {
                let req_header = session.req_header();
                if CacheService::is_cacheable(req_header, upstream_response)
                    && upstream_response.status.as_u16() == 200
                {
                    ctx.cache_req_header = Some(req_header.clone());
                    ctx.cache_resp_header = Some(upstream_response.clone());
                    ctx.should_cache_response = true;
                    tracing::debug!(
                        "Response is cacheable, will collect body in upstream_response_body_filter"
                    );
                    "MISS"
                } else {
                    tracing::debug!(
                        "Response not cacheable: status={}",
                        upstream_response.status.as_u16()
                    );
                    "BYPASS"
                }
            }
        } else {
            "BYPASS"
        };

        // Set cache headers and handle potential errors
        if let Err(e) = HeaderUtils::set_cache_headers(
            upstream_response,
            cache_status,
            browser_cache_enabled,
            browser_cache_ttl,
        ) {
            tracing::warn!("Failed to set cache headers: {}", e);
        }
        
        tracing::debug!("Setting x-cache header to: {}", cache_status);

        cache_status.to_string()
    }

    /// Process compression setup
    ///
    /// This determines if a response should be compressed and sets up the compression
    pub fn process_compression_setup(
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut StryvoContext,
    ) {
        if HeaderUtils::should_compress(upstream_response) {
            let encoding = HeaderUtils::get_preferred_encoding(session.req_header());
            if encoding != CompressionEncoding::Identity {
                ctx.compression_encoding = Some(encoding);
                // We don't know the compressed length yet, so just update the encoding headers
                if let Err(e) =
                    HeaderUtils::update_compression_headers(upstream_response, encoding, None)
                {
                    tracing::warn!("Failed to update compression headers: {}", e);
                    ctx.compression_encoding = None;
                } else {
                    tracing::debug!("Response will be compressed with {:?}", encoding);
                }
            }
        }
    }

    /// Process response filters
    ///
    /// This applies all configured response filters
    pub fn process_response_filters(
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut StryvoContext,
        response_filters: &[Box<dyn crate::proxy::response_modifiers::ResponseModifyMod>],
    ) {
        for filter in response_filters {
            filter.upstream_response_filter(session, upstream_response, ctx);
        }
    }

    /// Process logging
    ///
    /// This logs the request/response details
    pub fn process_logging(
        session: &mut Session,
        upstream_response: &ResponseHeader,
        ctx: &mut StryvoContext,
    ) {
        // Calculate elapsed time since request started
        let elapsed = if ctx.selector_buf.len() >= 16 {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&ctx.selector_buf[0..16]);
            let start_nanos = u128::from_ne_bytes(bytes);
            let current_nanos = Instant::now().elapsed().as_nanos();
            Duration::from_nanos((current_nanos - start_nanos) as u64)
        } else {
            Duration::from_secs(0)
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

        // Log the access
        crate::logging::log_access(
            session.req_header().method.as_str(),
            session.req_header().uri.to_string().as_str(),
            upstream_response.status.as_u16(),
            upstream,
            elapsed,
            &http_version,
            user_agent,
            referer,
            ctx.cache_status.as_ref().map(String::as_str).unwrap_or(""),
        );
    }
}