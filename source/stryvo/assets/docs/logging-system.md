# Stryvo Logging System Documentation

## Overview

Stryvo's logging system provides asynchronous request and error logging with configurable file outputs, structured JSON or text formats, and tracing integration. It supports buffered writes, graceful shutdown, and detailed request metadata for efficient and reliable log handling.

## Technical Specifications

### Core Components

#### 1. Logging Configuration (`LoggingConfig`)
- **Purpose**: Defines logging behavior and output settings
- **Location**: `src/logging.rs`
- **Key Features**:
  - Configurable access and error log file paths
  - Log level control (e.g., `INFO`, `ERROR`)
  - Options for structured (JSON) or text logging
  - Toggles for request IDs and performance metrics

#### 2. Asynchronous File Writer (`AsyncFileWriter`)
- **Purpose**: Handles non-blocking log writing to files
- **Location**: `src/logging.rs`
- **Key Features**:
  - 64KB buffer for efficient I/O
  - Supports both Tokio and standard thread runtimes
  - Graceful shutdown with flush of buffered logs
  - 4096-entry channel for log messages

#### 3. Access Log Entry (`AccessLogEntry`)
- **Purpose**: Captures comprehensive request metadata
- **Location**: `src/logging.rs`
- **Key Features**:
  - Records timestamp, method, URI, status, and more
  - Supports optional fields (e.g., client IP, SSL details)
  - Outputs in JSON or text format

#### 4. Request Trace Context (`RequestTraceContext`)
- **Purpose**: Provides tracing context for request tracking
- **Location**: `src/logging.rs`
- **Key Features**:
  - Generates unique request, span, and trace IDs
  - Tracks request start time and custom attributes
  - Supports parent span for distributed tracing

#### 5. Performance Metrics (`PerformanceMetrics`)
- **Purpose**: Collects performance data for requests
- **Location**: `src/logging.rs`
- **Key Features**:
  - Tracks processing time, bytes sent/received, and connection reuse
  - Includes optional metrics (e.g., upstream response time, compression ratio)
  - Outputs in JSON or text format

## Logging Flow

The following schematic illustrates the logging process for HTTP requests:

```
HTTP Request
     ↓
┌─────────────────┐
│ Generate        │
│ Request ID &    │
│ Trace Context   │
└─────────────────┘
     ↓
┌─────────────────┐
│ Log Request     │
│ Start           │
└─────────────────┘
     ↓
┌─────────────────┐
│ Process Request │
│ (Proxy/Cache/   │
│  File Serving)  │
└─────────────────┘
     ↓
┌─────────────────┐
│ Collect         │
│ Performance     │
│ Metrics         │
└─────────────────┘
     ↓
┌─────────────────┐
│ Log Access      │
│ Entry           │
│ - JSON/Text     │
│ - All Fields    │
└─────────────────┘
     ↓
┌─────────────────┐
│ Async File      │
│ Writer          │
│ - Buffer        │
│ - Batch Write   │
└─────────────────┘
     ↓
┌─────────────────┐
│ Log Request     │
│ End             │
└─────────────────┘
```

**Flow Steps**:
1. **Client Request**: The system receives an HTTP request and creates a `RequestTraceContext`.
2. **Request Start**: `log_request_start` logs the request initiation with method, URI, and client details.
3. **Access Log**: `AccessLogEntry` captures request metadata and is written to the access log via `AsyncFileWriter`.
4. **Metrics Collection**: `PerformanceMetrics` collects processing times and other metrics during request handling.
5. **Error Log**: Errors are logged to a separate file using `log_error_enhanced` with optional trace context.

## Component Details

### Logging Configuration

**Structure**:
```rust
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub access_log: Option<PathBuf>,
    pub error_log: Option<PathBuf>,
    pub level: Level,
    pub enabled: bool,
    pub structured_format: bool,
    pub include_request_id: bool,
    pub include_performance_metrics: bool,
}
```

**Key Features**:
- Configures log file paths for access and error logs
- Sets log level (e.g., `INFO`, `ERROR`) and enables/disables logging
- Toggles JSON formatting and inclusion of request IDs/performance metrics

### Asynchronous File Writer

**Structure**:
```rust
pub struct AsyncFileWriter {
    sender: Sender<LogMessage>,
    task_handle: Option<TaskHandle>,
}

enum LogMessage {
    Content(String),
    Shutdown(oneshot::Sender<()>),
}
```

**Key Operations**:
- `new(path)`: Initializes the writer with a file path, creating parent directories if needed
- `writer_task`: Asynchronously writes buffered log messages with 64KB buffer and 100ms flush interval
- `Drop`: Ensures graceful shutdown by flushing remaining logs
- Supports both Tokio and standard thread runtimes

### Access Log Entry

**Structure**:
```rust
pub struct AccessLogEntry {
    pub timestamp: String,
    pub request_id: Option<String>,
    pub method: String,
    pub uri: String,
    pub query_string: Option<String>,
    pub status: u16,
    pub upstream: String,
    pub elapsed_ms: u64,
    pub http_version: String,
    pub user_agent: String,
    pub referer: String,
    pub cache_status: String,
    pub client_ip: Option<String>,
    pub host_header: Option<String>,
    pub bytes_sent: Option<u64>,
    pub bytes_received: Option<u64>,
    pub ssl_protocol: Option<String>,
    pub ssl_cipher: Option<String>,
    pub request_size: Option<u64>,
    pub response_time_upstream_ms: Option<u64>,
    pub custom_headers: HashMap<String, String>,
}
```

**Key Operations**:
- `to_json()`: Serializes the entry to JSON with optional fields
- `to_formatted_string()`: Formats the entry as a space-separated string
- Used by `log_access_enhanced` for flexible logging

### Request Trace Context

**Structure**:
```rust
pub struct RequestTraceContext {
    pub request_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub trace_id: String,
    pub start_time: std::time::Instant,
    pub custom_attributes: HashMap<String, String>,
}
```

**Key Operations**:
- `new()`: Generates unique IDs for request, span, and trace
- `with_parent()`: Links to a parent span for distributed tracing
- `elapsed()`: Calculates request duration
- `add_attribute()`: Adds custom metadata for logging

### Performance Metrics

**Structure**:
```rust
pub struct PerformanceMetrics {
    pub request_processing_time_ms: u64,
    pub upstream_connect_time_ms: Option<u64>,
    pub upstream_response_time_ms: Option<u64>,
    pub cache_lookup_time_ms: Option<u64>,
    pub ssl_handshake_time_ms: Option<u64>,
    pub dns_lookup_time_ms: Option<u64>,
    pub total_bytes_sent: u64,
    pub total_bytes_received: u64,
    pub connection_reused: bool,
    pub compression_ratio: Option<f64>,
}
```

**Key Operations**:
- `new()`: Initializes metrics with default values
- `to_json()`: Serializes metrics to JSON with optional fields
- Used by `log_performance_metrics` for detailed performance logging

## Logging Functions

### Access Logging
```rust
pub fn log_access_enhanced(entry: &AccessLogEntry, structured_format: bool) {
    if structured_format {
        tracing::info!(target: "stryvo::logging", "{}", entry.to_json().to_string());
    } else {
        tracing::info!(target: "stryvo::logging", "{}", entry.to_formatted_string());
    }
}
```

### Performance Metrics Logging
```rust
pub fn log_performance_metrics(
    trace_context: &RequestTraceContext,
    metrics: &PerformanceMetrics,
    structured_format: bool,
) {
    if structured_format {
        let log_entry = json!({
            "type": "performance_metrics",
            "timestamp": Local::now().format("%Y-%m-%d %H:%M:%S%.3f %z").to_string(),
            "request_id": trace_context.request_id,
            "trace_id": trace_context.trace_id,
            "span_id": trace_context.span_id,
            "metrics": metrics.to_json(),
            "custom_attributes": trace_context.custom_attributes
        });
        tracing::info!(target: "stryvo::performance", "{}", log_entry.to_string());
    } else {
        tracing::info!(
            target: "stryvo::performance",
            "request_id={} trace_id={} processing_time_ms={} bytes_sent={} bytes_received={} connection_reused={}",
            trace_context.request_id,
            trace_context.trace_id,
            metrics.request_processing_time_ms,
            metrics.total_bytes_sent,
            metrics.total_bytes_received,
            metrics.connection_reused
        );
    }
}
```

### Error Logging
```rust
pub fn log_error_enhanced(
    trace_context: Option<&RequestTraceContext>,
    method: &str,
    uri: &str,
    error: &str,
    error_code: Option<&str>,
    structured_format: bool,
) {
    if structured_format {
        let mut log_entry = json!({
            "type": "error",
            "timestamp": Local::now().format("%Y-%m-%d %H:%M:%S%.3f %z").to_string(),
            "method": method,
            "uri": uri,
            "error": error
        });
        if let Some(ctx) = trace_context {
            log_entry["request_id"] = json!(ctx.request_id);
            log_entry["trace_id"] = json!(ctx.trace_id);
            log_entry["span_id"] = json!(ctx.span_id);
        }
        if let Some(code) = error_code {
            log_entry["error_code"] = json!(code);
        }
        tracing::error!(target: "stryvo::error", "{}", log_entry.to_string());
    } else {
        if let Some(ctx) = trace_context {
            tracing::error!(
                target: "stryvo::error",
                "request_id={} trace_id={} method={} uri={} error={} error_code={}",
                ctx.request_id,
                ctx.trace_id,
                method,
                uri,
                error,
                error_code.unwrap_or("unknown")
            );
        } else {
            tracing::error!(
                target: "stryvo::error",
                "method={} uri={} error={} error_code={}",
                method,
                uri,
                error,
                error_code.unwrap_or("unknown")
            );
        }
    }
}
```

## Configuration

### Example Configuration
```yaml
logging:
  enabled: true
  level: "info"
  access_log: "/var/log/stryvo/access.log"
  error_log: "/var/log/stryvo/error.log"
  structured_format: true
  include_request_id: true
  include_performance_metrics: true
```

**Notes**:
- `access_log` and `error_log` specify file paths (optional)
- `level` sets the minimum log level (e.g., `info`, `error`)
- `structured_format` enables JSON output
- `include_request_id` and `include_performance_metrics` toggle additional metadata

## Log Formats

### JSON Access Log Example
```json
{
  "timestamp": "2025-09-08 12:22:45.123 +0300",
  "method": "GET",
  "uri": "/api/users/123",
  "status": 200,
  "upstream": "backend-1:8080",
  "elapsed_ms": 45,
  "http_version": "HTTP/1.1",
  "user_agent": "Mozilla/5.0...",
  "referer": "https://example.com",
  "cache_status": "HIT",
  "request_id": "req_abc123def456",
  "client_ip": "192.168.1.100",
  "host_header": "api.example.com",
  "bytes_sent": 1024,
  "bytes_received": 256,
  "ssl_protocol": "TLSv1.3",
  "ssl_cipher": "TLS_AES_256_GCM_SHA384",
  "request_size": 256,
  "response_time_upstream_ms": 35,
  "custom_headers": {
    "x-forwarded-for": "203.0.113.1"
  }
}
```

### Text Access Log Example
```
timestamp=2025-09-08 12:22:45.123 +0300 method=GET uri=/api/users/123 status=200 upstream=backend-1:8080 elapsed_ms=45 http_version=HTTP/1.1 user_agent=Mozilla/5.0... referer=https://example.com cache=HIT request_id=req_abc123def456 client_ip=192.168.1.100 host_header=api.example.com bytes_sent=1024 bytes_received=256 ssl_protocol=TLSv1.3 ssl_cipher=TLS_AES_256_GCM_SHA384 request_size=256 response_time_upstream_ms=35 custom_headers=[x-forwarded-for=203.0.113.1]
```

### JSON Performance Metrics Example
```json
{
  "type": "performance_metrics",
  "timestamp": "2025-09-08 12:22:45.123 +0300",
  "request_id": "req_abc123def456",
  "trace_id": "trace_xyz789",
  "span_id": "span_123",
  "metrics": {
    "request_processing_time_ms": 45,
    "upstream_connect_time_ms": 5,
    "upstream_response_time_ms": 35,
    "cache_lookup_time_ms": 2,
    "total_bytes_sent": 1024,
    "total_bytes_received": 256,
    "connection_reused": true,
    "compression_ratio": 0.65
  },
  "custom_attributes": {}
}
```

## Performance Optimization

### Buffered Writing
- **Buffer Size**: 64KB buffer with 32KB or 100ms flush triggers
- **Batch Writes**: Multiple log entries per I/O operation
- **Channel Capacity**: 4096-entry channel to prevent blocking

### Async Processing
- Non-blocking writes using `AsyncFileWriter`
- Supports Tokio for async environments and standard threads for compatibility
- Graceful shutdown ensures no log loss

### Memory Efficiency
- Efficient JSON serialization for structured logs
- Conditional field inclusion to reduce overhead
- Minimal memory usage for text logs

## Troubleshooting

### Common Issues
1. **Log Files Not Created**:
   - **Cause**: Insufficient permissions or invalid paths
   - **Solution**: Verify write access and ensure parent directories exist
2. **Missing Log Entries**:
   - **Cause**: Buffer not flushed before shutdown or channel overflow
   - **Solution**: Check shutdown logic and monitor channel capacity
3. **High Disk Usage**:
   - **Cause**: High log volume or infrequent rotation
   - **Solution**: Implement log rotation (e.g., via `logrotate`)

### Debugging Tips
- Enable debug logs for writer task events (e.g., buffer flushes, errors)
- Monitor channel buffer usage and flush frequency
- Check error logs for write or flush failures

## Best Practices

### Configuration
- Use JSON format (`structured_format: true`) for production to enable parsing
- Enable `include_request_id` for traceability
- Set appropriate log levels to balance detail and performance

### Operations
- Monitor log file sizes and implement rotation
- Use log aggregation tools (e.g., ELK, Splunk) for analysis
- Set up alerts for error log spikes

### Security
- Avoid logging sensitive data (e.g., passwords, tokens)
- Restrict log file access to authorized users
- Maintain logs for audit and security analysis

## Limitations
- No external tracing integration (e.g., Jaeger) in the provided code
- Limited configuration options compared to advanced setups
- No built-in log rotation or size management

This documentation reflects the current implementation of Stryvo's logging system as of the provided code. For further details on integration or extension, refer to the source code in `src/logging.rs`.