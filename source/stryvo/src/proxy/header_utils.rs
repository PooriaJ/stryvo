//! Header utilities for HTTP request and response handling
//!
//! This module provides centralized utilities for manipulating HTTP headers
//! to avoid code duplication between request and response handling.

use crate::proxy::compression::{ClientCompression, CompressionEncoding};
use pingora_http::{RequestHeader, ResponseHeader};

/// Utility functions for HTTP header manipulation
pub struct HeaderUtils;

impl HeaderUtils {
    /// Update compression-related headers in a response
    ///
    /// This centralizes the logic for handling content-encoding, content-length,
    /// and transfer-encoding headers when compression is applied.
    pub fn update_compression_headers(
        resp_header: &mut ResponseHeader,
        encoding: CompressionEncoding,
        compressed_len: Option<usize>,
    ) -> Result<(), String> {
        // Set the Content-Encoding header based on the compression method
        match encoding {
            CompressionEncoding::Gzip => {
                resp_header
                    .insert_header("content-encoding", "gzip")
                    .map_err(|e| e.to_string())?;
            }
            CompressionEncoding::Brotli => {
                resp_header
                    .insert_header("content-encoding", "br")
                    .map_err(|e| e.to_string())?;
            }
            CompressionEncoding::Zstd => {
                resp_header
                    .insert_header("content-encoding", "zstd")
                    .map_err(|e| e.to_string())?;
            }
            CompressionEncoding::Identity => {
                // No compression, no need to set Content-Encoding
                return Ok(());
            }
        }

        // Handle Content-Length and Transfer-Encoding
        resp_header.remove_header("content-length");
        
        if let Some(len) = compressed_len {
            // We know the compressed length, so set Content-Length
            resp_header
                .insert_header("content-length", &len.to_string())
                .map_err(|e| e.to_string())?;
        } else {
            // We don't know the compressed length yet, use chunked transfer encoding
            resp_header
                .insert_header("transfer-encoding", "chunked")
                .map_err(|e| e.to_string())?;
        }

        Ok(())
    }

    /// Get the preferred compression encoding from request headers
    ///
    /// Examines the Accept-Encoding header to determine the best compression method
    /// to use for the client.
    pub fn get_preferred_encoding(req_header: &RequestHeader) -> CompressionEncoding {
        ClientCompression::select_encoding(req_header).unwrap_or(CompressionEncoding::Identity)
    }

    /// Check if a response should be compressed based on content type and existing headers
    pub fn should_compress(resp_header: &ResponseHeader) -> bool {
        ClientCompression::should_compress(resp_header)
    }

    /// Set cache-related headers in a response
    ///
    /// Configures server identification, cache status reporting, and browser cache control
    /// based on the provided parameters.
    pub fn set_cache_headers(
        resp_header: &mut ResponseHeader,
        cache_status: &str,
        browser_cache_enabled: bool,
        browser_cache_ttl: Option<u64>,
    ) -> Result<(), String> {
        // Set Server header
        resp_header.remove_header("server");
        resp_header.append_header("server", "Stryvo")
            .map_err(|e| e.to_string())?;        

        // Set X-Cache header
        resp_header.remove_header("x-cache");
        resp_header.append_header("x-cache", cache_status)
            .map_err(|e| e.to_string())?;

        // Add Cache-Control headers for browser caching
        resp_header.remove_header("cache-control");
        
        if browser_cache_enabled {
            if let Some(ttl) = browser_cache_ttl {
                resp_header
                    .append_header("cache-control", &format!("public, max-age={}", ttl))
                    .map_err(|e| e.to_string())?;
            }
        } else {
            // Disable browser caching
            resp_header
                .append_header("cache-control", "no-store, no-cache, must-revalidate")
                .map_err(|e| e.to_string())?;
            resp_header.append_header("pragma", "no-cache")
                .map_err(|e| e.to_string())?;
        }

        Ok(())
    }
}