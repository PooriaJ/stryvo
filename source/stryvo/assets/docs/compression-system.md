# Stryvo Compression System Documentation

## Overview

Stryvo's compression system optimizes HTTP response and cache storage efficiency by applying content compression based on client preferences and content characteristics. It supports multiple compression algorithms (gzip, Brotli, Zstd) with configurable strategies to balance performance, compatibility, and compression ratios.

## Technical Specifications

### Core Components

#### 1. Compression Strategy (`CompressionStrategy`)
- **Purpose**: Defines compression behavior for cached content
- **Location**: `src/compression.rs`
- **Key Features**:
  - Supports three strategies: `None`, `All`, and `Auto`
  - Automatically skips compression for pre-compressed content types (e.g., images, videos, archives)
  - Configurable via `CompressionStrategy` enum

#### 2. Compression Utilities (`CompressUtils`)
- **Purpose**: Implements compression and decompression logic
- **Location**: `src/compression.rs`
- **Supported Algorithms**:
  - **Gzip**: Standard compression with level 6 (balanced speed and ratio)
  - **Brotli**: High compression ratio with quality level 5
  - **Zstd**: High-performance compression with level 3
- **Key Features**:
  - Compresses data using specified algorithms
  - Handles errors with descriptive messages
  - Optimizes buffer allocation based on input size

#### 3. Client Compression Handler (`ClientCompression`)
- **Purpose**: Manages HTTP response compression based on client headers
- **Location**: `src/compression.rs`
- **Key Features**:
  - Parses `Accept-Encoding` header to select optimal compression
  - Prioritizes Brotli, Zstd, Gzip, and Identity in that order
  - Skips compression for small payloads (<150 bytes) or already compressed responses
  - Supports text-based and application-based content types (e.g., JSON, JavaScript, XML)

## Compression Flow

The following schematic illustrates the compression process for HTTP responses:

```
┌──────────────┐           ┌──────────────────┐           ┌──────────────────┐
│ Client       │           │ ClientCompression│           │ CompressUtils    │
│ Request      │           │                  │           │                  │
│ (Accept-     │───1.────▶│ select_encoding  │───3.────▶│ compress_gzip    │
│ Encoding)    │           │ should_compress  │           │ compress_brotli  │
└──────────────┘           └───────┬──────────┘           │ compress_zstd    │
                                   │                      └────────┬─────────┘
                                   │                               │
                                   │ 2. Content-Type,           │ 4. Compressed
                                   │    Size Check              │    Data
                                   │                               │
                                   ▼                               ▼
                            ┌──────────────┐           ┌──────────────────┐
                            │ Response      │◄───5.────│ Client Response  │
                            │ Processing    │           │ (Content-Encoding│
                            └──────────────┘           │ Header)          │
                                               └──────────────────┘
```

**Flow Steps**:
1. **Client Request**: The client sends an HTTP request with an `Accept-Encoding` header.
2. **Compression Decision**: `ClientCompression` evaluates `Content-Type` and `Content-Encoding` to determine if compression is needed.
3. **Encoding Selection**: `ClientCompression::select_encoding` chooses the best algorithm (Brotli, Zstd, Gzip, or Identity).
4. **Compression**: `CompressUtils` compresses the response data using the selected algorithm.
5. **Response Delivery**: The compressed response is sent with the appropriate `Content-Encoding` header.

## Component Details

### Compression Strategy

**Structure**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionStrategy {
    None,  // No compression
    All,   // Compress all content
    Auto,  // Compress based on content type
}
```

**Key Operations**:
- `should_compress(content_type)`: Determines if a content type should be compressed based on the strategy.
- `compress(data)`: Applies gzip compression for `All` strategy; otherwise, returns data unchanged.
- `decompress(data)`: Decompresses gzip data for `All` strategy; otherwise, returns data unchanged.

**Default Behavior**:
- Uses `Auto` strategy, skipping compression for content types like `image/*`, `video/*`, `font/*`, and compressed archives (e.g., `application/zip`).

### Compression Utilities

**Key Methods**:
```rust
struct CompressUtils;

impl CompressUtils {
    pub fn compress_gzip(data: &Bytes) -> Result<Bytes, String> { ... }
    pub fn decompress_gzip(data: &Bytes) -> Result<Bytes, String> { ... }
    pub fn compress_brotli(data: &Bytes) -> Result<Bytes, String> { ... }
    pub fn compress_zstd(data: &Bytes) -> Result<Bytes, String> { ... }
}
```

**Configuration**:
- **Gzip**: Uses compression level 6 for a balance of speed and compression.
- **Brotli**: Uses quality level 5 and a window size of 22.
- **Zstd**: Uses compression level 3 for high performance.

### Client Compression Handler

**Key Methods**:
```rust
pub struct ClientCompression;

impl ClientCompression {
    pub fn select_encoding(req_header: &RequestHeader) -> Option<CompressionEncoding> { ... }
    pub fn should_compress(resp_header: &ResponseHeader) -> bool { ... }
    pub fn compress(data: &Bytes, encoding: CompressionEncoding) -> Result<Bytes, String> { ... }
}
```

**Logic**:
- **Encoding Selection**: Checks `Accept-Encoding` header and prioritizes Brotli, Zstd, Gzip, or Identity.
- **Compression Decision**: Compresses only text-based or application-based content types (e.g., `text/*`, `application/json`) unless already compressed or too small (<150 bytes).
- **Compression Execution**: Delegates to `CompressUtils` for the selected algorithm.

## Performance Optimization

### Algorithm Selection
- Prioritizes modern algorithms (Brotli, Zstd) for better compression ratios when supported.
- Falls back to Gzip for broad compatibility or Identity for unsupported clients.
- Skips compression for small payloads to avoid overhead.

### Resource Efficiency
- Uses pre-allocated buffers based on input size to minimize memory overhead.
- Employs standard compression levels (e.g., Gzip level 6, Brotli quality 5) to balance CPU usage and compression ratio.
- Avoids compressing already compressed content types to save CPU cycles.

## Monitoring and Debugging

### Debug Information
- **Compression Statistics**: Monitor compression ratios and algorithm usage via application logs.
- **Error Handling**: Compression errors return descriptive messages (e.g., "Gzip compression error: ...").
- **Header Inspection**: Log `Accept-Encoding` and `Content-Type` headers for troubleshooting.

### Common Issues and Solutions
1. **Low Compression Ratios**:
   - **Cause**: Compressing already compressed content (e.g., images).
   - **Solution**: Use `Auto` strategy to skip known compressed types.
2. **Client Incompatibility**:
   - **Cause**: Client doesn't support selected encoding.
   - **Solution**: Ensure fallback to Gzip or Identity via `select_encoding`.
3. **High CPU Usage**:
   - **Cause**: Compressing large or incompressible content.
   - **Solution**: Enforce size thresholds and content type checks.

## Best Practices

### Configuration
- Use `CompressionStrategy::Auto` for most use cases to avoid compressing pre-compressed content.
- Set appropriate content type filters to optimize compression decisions.
- Monitor payload sizes to ensure compression is applied only when beneficial.

### Performance
- Prefer Brotli for modern clients to maximize compression ratios.
- Use Gzip for legacy clients or real-time applications requiring faster compression.
- Implement caching for compressed responses to reduce repeated compression overhead.

### Security
- Validate input data to prevent compression bombs (e.g., limit input size).
- Handle compression errors gracefully to avoid service disruptions.
- Log compression events for audit and analysis.

## Limitations
- Streaming compression is not currently supported.
- No dynamic adjustment of compression levels based on system load.
- Limited content type analysis compared to entropy-based approaches in some systems.

This documentation reflects the current implementation of Stryvo's compression system as of the provided code. For further details on integration or extension, refer to the source code in `src/compression.rs`.