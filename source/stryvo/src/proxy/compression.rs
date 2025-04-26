//! Compression module
//!
//! This module provides functionality for compressing HTTP responses based on
//! the client's Accept-Encoding header preferences and for the cache system.
//! It defines different compression strategies that can be used to optimize
//! storage and transfer efficiency.

use bytes::Bytes;
use pingora_http::{RequestHeader, ResponseHeader};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Content types that skip cache compression by default
pub const SKIP_COMPRESSION: &[&str] = &[
    "image/avif",
    "image/webp",
    "image/png",
    "image/jpeg",
    "font/woff2",
    "font/woff",
    "video/webm",
    "video/ogg",
    "video/mpeg",
    "video/mp4",
    "application/zip",
    "application/gzip",
    "application/x-compress",
    "application/x-gzip",
    "application/x-bzip2",
    "application/x-rar-compressed",
    "application/x-7z-compressed",
    "audio/",
];

// Default compression level constants
const DEFAULT_GZIP_LEVEL: u32 = 6;     // Standard compression level
const DEFAULT_BROTLI_QUALITY: i32 = 5; // Medium quality (range is 0-11)
const DEFAULT_ZSTD_LEVEL: i32 = 3;     // Good balance between speed and compression

/// Compression strategy for cached content
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionStrategy {
    /// No compression
    None,

    /// Compress all content
    All,

    /// Automatically determine what to compress based on content type
    ///
    /// This will skip compression for content types that are already compressed
    /// such as images, videos, and compressed archives.
    Auto,
}

impl Default for CompressionStrategy {
    fn default() -> Self {
        Self::Auto
    }
}

impl CompressionStrategy {
    /// Check if a content type should be compressed with this strategy
    pub fn should_compress(&self, content_type: Option<&str>) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Auto => {
                if let Some(content_type) = content_type {
                    !SKIP_COMPRESSION
                        .iter()
                        .any(|&skip| content_type.contains(skip))
                } else {
                    true
                }
            }
        }
    }

    /// Compress data using the selected strategy
    pub fn compress(&self, data: Bytes) -> Result<Bytes, String> {
        match self {
            Self::None | Self::Auto => Ok(data),
            Self::All => CompressUtils::compress_gzip(&data),
        }
    }

    /// Decompress data using the selected strategy
    pub fn decompress(&self, data: &Bytes) -> Result<Bytes, String> {
        match self {
            Self::None | Self::Auto => Ok(data.clone()),
            Self::All => CompressUtils::decompress_gzip(data),
        }
    }
}

/// Supported compression encodings for HTTP responses
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionEncoding {
    /// Gzip compression
    Gzip,
    /// Brotli compression
    Brotli,
    /// Zstd compression
    Zstd,
    /// No compression
    Identity,
}

impl CompressionEncoding {
    /// Convert the compression encoding to its string representation
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Brotli => "br",
            Self::Zstd => "zstd",
            Self::Identity => "identity",
        }
    }
    
    /// Get all supported encodings in priority order
    pub fn all_supported() -> &'static [CompressionEncoding] {
        &[
            CompressionEncoding::Brotli, 
            CompressionEncoding::Zstd,
            CompressionEncoding::Gzip,
            CompressionEncoding::Identity,
        ]
    }
}

/// Shared compression utilities
struct CompressUtils;

impl CompressUtils {
    /// Compress data using gzip
    pub fn compress_gzip(data: &Bytes) -> Result<Bytes, String> {
        // Using explicit compression level instead of default()
        let mut encoder = flate2::write::GzEncoder::new(
            Vec::with_capacity(data.len()), 
            flate2::Compression::new(DEFAULT_GZIP_LEVEL) // Standard compression level
        );
        
        encoder
            .write_all(data)
            .map_err(|e| format!("Gzip compression error: {}", e))?;
        let compressed = encoder
            .finish()
            .map_err(|e| format!("Gzip compression finish error: {}", e))?;
        Ok(Bytes::from(compressed))
    }

    /// Decompress data using gzip
    pub fn decompress_gzip(data: &Bytes) -> Result<Bytes, String> {
        let mut decoder = flate2::read::GzDecoder::new(&data[..]);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .map_err(|e| format!("Gzip decompression error: {}", e))?;
        Ok(Bytes::from(decompressed))
    }
    
    /// Compress data using brotli
    pub fn compress_brotli(data: &Bytes) -> Result<Bytes, String> {
        let mut output = Vec::with_capacity(data.len());
        let params = brotli::enc::BrotliEncoderParams {
            quality: DEFAULT_BROTLI_QUALITY,
            lgwin: 22,  // Default window size
            ..Default::default()
        };

        brotli::enc::BrotliCompress(&mut std::io::Cursor::new(data), &mut output, &params)
            .map_err(|_| "Brotli compression failed".to_string())?;

        Ok(Bytes::from(output))
    }
    
    /// Compress data using zstd
    pub fn compress_zstd(data: &Bytes) -> Result<Bytes, String> {
        let mut encoder = zstd::Encoder::new(Vec::with_capacity(data.len()), DEFAULT_ZSTD_LEVEL)
            .map_err(|e| format!("Zstd encoder creation error: {}", e))?;

        std::io::Write::write_all(&mut encoder, data)
            .map_err(|e| format!("Zstd compression error: {}", e))?;

        let compressed = encoder
            .finish()
            .map_err(|e| format!("Zstd compression finish error: {}", e))?;

        Ok(Bytes::from(compressed))
    }
}

/// Client-side compression handler
pub struct ClientCompression;

impl ClientCompression {
    /// Determine the best compression encoding based on the Accept-Encoding header
    pub fn select_encoding(req_header: &RequestHeader) -> Option<CompressionEncoding> {
        // Get the Accept-Encoding header value
        if let Some(accept_encoding) = req_header.headers.get("accept-encoding") {
            if let Ok(encoding_str) = accept_encoding.to_str() {
                let encoding = encoding_str.to_lowercase();

                // Check encodings in priority order
                for &enc in CompressionEncoding::all_supported() {
                    if enc == CompressionEncoding::Identity {
                        continue;
                    }
                    
                    if encoding.contains(enc.as_str()) {
                        return Some(enc);
                    }
                }
            }
        }

        // Default to no compression
        None
    }

    /// Check if the response should be compressed
    pub fn should_compress(resp_header: &ResponseHeader) -> bool {
        // Don't compress if already compressed
        if resp_header.headers.contains_key("content-encoding") {
            return false;
        }

        // Check content type - only compress text and application types
        if let Some(content_type) = resp_header.headers.get("content-type") {
            if let Ok(content_type_str) = content_type.to_str() {
                // Compress text and application content types, except those known to be compressed
                return (content_type_str.starts_with("text/")
                    || content_type_str.starts_with("application/json")
                    || content_type_str.starts_with("application/javascript")
                    || content_type_str.starts_with("application/xml"))
                    && !content_type_str.contains("zip")
                    && !content_type_str.contains("gzip")
                    && !content_type_str.contains("compressed");
            }
        }

        false
    }

    /// Compress data using the specified encoding
    pub fn compress(data: &Bytes, encoding: CompressionEncoding) -> Result<Bytes, String> {
        // Avoid compression for very small payloads where overhead might exceed benefits
        if data.len() < 150 {
            return Ok(data.clone());
        }
        
        match encoding {
            CompressionEncoding::Gzip => CompressUtils::compress_gzip(data),
            CompressionEncoding::Brotli => CompressUtils::compress_brotli(data),
            CompressionEncoding::Zstd => CompressUtils::compress_zstd(data),
            CompressionEncoding::Identity => Ok(data.clone()),
        }
    }
}