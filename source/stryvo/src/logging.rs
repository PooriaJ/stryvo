// Provides asynchronous logging for the Stryvo proxy, supporting access and error logs
// with configurable file outputs and tracing integration. Manages buffered writes and
// graceful shutdown for efficient and reliable log handling.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::collections::HashMap;

use chrono::Local;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::oneshot;
use tokio::task;
use tracing::Level;
use tracing_subscriber::fmt::{self, MakeWriter};
use tracing_subscriber::prelude::*;
use uuid::Uuid;

/// Configuration for the logging system
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub access_log: Option<PathBuf>,
    pub error_log: Option<PathBuf>,
    pub level: Level,
    pub enabled: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            access_log: None,
            error_log: None,
            level: Level::INFO,
            enabled: false,
        }
    }
}

/// Message sent to the writer task
#[derive(Debug)]
enum LogMessage {
    Content(String),
    Shutdown(oneshot::Sender<()>),
}

/// Enum to hold either a Tokio join handle or a std::thread join handle
enum TaskHandle {
    Tokio(task::JoinHandle<()>),
    Std(std::thread::JoinHandle<()>),
}

/// Asynchronous file writer for logging
pub struct AsyncFileWriter {
    sender: Sender<LogMessage>,
    task_handle: Option<TaskHandle>,
}

impl AsyncFileWriter {
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file_path = path.as_ref().to_path_buf();
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        let file = File::create(&file_path)?;
        let (sender, receiver) = mpsc::channel(4096);

        let task_handle = if tokio::runtime::Handle::try_current().is_ok() {
            let file = tokio::fs::File::from_std(file);
            let handle = task::spawn(Self::writer_task(receiver, file, file_path));
            TaskHandle::Tokio(handle)
        } else {
            let handle = std::thread::spawn(move || {
                Self::writer_task_sync(receiver, file, file_path);
            });
            TaskHandle::Std(handle)
        };

        Ok(Self {
            sender,
            task_handle: Some(task_handle),
        })
    }

    async fn writer_task(
        mut receiver: Receiver<LogMessage>, 
        mut file: tokio::fs::File, 
        path: PathBuf
    ) {
        let mut buffer = Vec::with_capacity(64 * 1024);
        let mut last_flush = std::time::Instant::now();

        while let Some(msg) = receiver.recv().await {
            match msg {
                LogMessage::Content(content) => {
                    buffer.extend_from_slice(content.as_bytes());
                    buffer.push(b'\n');
                    
                    let now = std::time::Instant::now();
                    if buffer.len() > 32 * 1024
                        || now.duration_since(last_flush) > Duration::from_millis(100)
                    {
                        if let Err(e) = file.write_all(&buffer).await {
                            tracing::error!("Error writing to {}: {}", path.display(), e);
                        }
                        if let Err(e) = file.flush().await {
                            tracing::error!("Error flushing {}: {}", path.display(), e);
                        }
                        buffer.clear();
                        last_flush = now;
                    }
                }
                LogMessage::Shutdown(response) => {
                    // Flush remaining buffer contents before shutdown
                    if !buffer.is_empty() {
                        if let Err(e) = file.write_all(&buffer).await {
                            tracing::error!("Error flushing on shutdown {}: {}", path.display(), e);
                        }
                        if let Err(e) = file.flush().await {
                            tracing::error!("Error flushing on shutdown {}: {}", path.display(), e);
                        }
                    }
                    let _ = response.send(());
                    break;
                }
            }
        }
    }

    fn writer_task_sync(
        mut receiver: Receiver<LogMessage>, 
        mut file: File, 
        path: PathBuf
    ) {
        let mut buffer = Vec::with_capacity(64 * 1024);
        let mut last_flush = std::time::Instant::now();

        while let Some(msg) = receiver.blocking_recv() {
            match msg {
                LogMessage::Content(content) => {
                    buffer.extend_from_slice(content.as_bytes());
                    buffer.push(b'\n');
                    
                    let now = std::time::Instant::now();
                    if buffer.len() > 32 * 1024
                        || now.duration_since(last_flush) > Duration::from_millis(100)
                    {
                        if let Err(e) = file.write_all(&buffer) {
                            tracing::error!("Error writing to {}: {}", path.display(), e);
                        }
                        if let Err(e) = file.flush() {
                            tracing::error!("Error flushing {}: {}", path.display(), e);
                        }
                        buffer.clear();
                        last_flush = now;
                    }
                }
                LogMessage::Shutdown(response) => {
                    // Flush remaining buffer contents before shutdown
                    if !buffer.is_empty() {
                        if let Err(e) = file.write_all(&buffer) {
                            tracing::error!("Error flushing on shutdown {}: {}", path.display(), e);
                        }
                        if let Err(e) = file.flush() {
                            tracing::error!("Error flushing on shutdown {}: {}", path.display(), e);
                        }
                    }
                    let _ = response.send(());
                    break;
                }
            }
        }
    }
}

impl Drop for AsyncFileWriter {
    fn drop(&mut self) {
        if let Some(task_handle) = self.task_handle.take() {
            // Send shutdown message with oneshot channel for confirmation
            let (tx, rx) = oneshot::channel();
            if self.sender.try_send(LogMessage::Shutdown(tx)).is_ok() {
                // Use a timeout to avoid blocking indefinitely
                let timeout = Duration::from_millis(500);
                
                match task_handle {
                    TaskHandle::Tokio(handle) => {
                        // For Tokio tasks, we can use a timeout
                        let _ = tokio::runtime::Handle::try_current().map(|h| {
                            h.block_on(async {
                                tokio::select! {
                                    _ = rx => tracing::debug!("Writer task completed gracefully"),
                                    _ = tokio::time::sleep(timeout) => {
                                        tracing::debug!("Timeout waiting for writer task, aborting");
                                        handle.abort();
                                    }
                                }
                            })
                        });
                    }
                    TaskHandle::Std(_handle) => {
                        // For std threads, use a simple timeout
                        let start = std::time::Instant::now();
                        let mut rx = rx;  // Make rx mutable
                        while start.elapsed() < timeout {
                            if rx.try_recv().is_ok() {
                                tracing::debug!("Writer thread completed gracefully");
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        // We don't join the thread to avoid blocking
                    }
                }
            }
        }
    }
}

impl MakeWriter<'_> for AsyncFileWriter {
    type Writer = AsyncLogWriter;

    fn make_writer(&self) -> Self::Writer {
        AsyncLogWriter {
            sender: self.sender.clone(),
        }
    }
}

/// Writer implementation for tracing integration
pub struct AsyncLogWriter {
    sender: Sender<LogMessage>,
}

impl Write for AsyncLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let message = String::from_utf8_lossy(buf).to_string();
        if self
            .sender
            .try_send(LogMessage::Content(message))
            .is_err()
        {
            tracing::warn!("Log buffer full, message dropped");
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Log buffer full"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Initialize the logging system
pub fn init_logging(config: &LoggingConfig) -> io::Result<()> {
    // Console layer: exclude access logs from stryvo::logging at INFO level
    let console_layer = fmt::layer().with_writer(std::io::stdout).with_filter(
        tracing_subscriber::filter::Targets::new()
            .with_target("stryvo::logging", Level::WARN) // Exclude INFO access logs
            .with_default(config.level),
    );

    if !config.enabled {
        tracing_subscriber::registry().with(console_layer).init();
        return Ok(());
    }

    let access_writer = config
        .access_log
        .as_ref()
        .map(AsyncFileWriter::new)
        .transpose()?;
    let error_writer = config
        .error_log
        .as_ref()
        .map(AsyncFileWriter::new)
        .transpose()?;

    // Access layer: minimal format, only access logs
    let access_layer = access_writer.map(|w| {
        fmt::layer()
            .with_ansi(false)        // Disable ANSI colors
            .without_time()          // Remove timestamp prefix
            .with_target(false)      // Remove target
            .with_level(false)       // Remove log level
            .with_file(false)        // Remove file path
            .with_line_number(false) // Remove line numbers
            .with_thread_ids(false)  // Remove thread IDs
            .with_thread_names(false)// Remove thread names
            .with_writer(w)
            .with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("stryvo::logging", Level::INFO),
            )
    });

    // Error layer: plain text, only errors
    let error_layer = error_writer.map(|w| {
        fmt::layer()
            .with_ansi(false) // Disable ANSI colors for file
            .with_writer(w)
            .with_filter(tracing_subscriber::filter::LevelFilter::ERROR)
    });

    tracing_subscriber::registry()
        .with(console_layer) // Console for non-access logs
        .with(access_layer) // Access logs to file
        .with(error_layer) // Error logs to file
        .init();

    Ok(())
}

/// Enhanced access log entry with additional metadata
#[derive(Debug, Clone)]
pub struct AccessLogEntry {
    pub timestamp: String,
    pub request_id: String,
    pub method: String,
    pub uri: String,
    pub query_string: String,
    pub status: u16,
    pub upstream: String,
    pub elapsed_ms: u64,
    pub http_version: String,
    pub user_agent: String,
    pub referer: String,
    pub cache_status: String,
    pub client_ip: String,
    pub host_header: String,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub ssl_protocol: String,
    pub ssl_cipher: String,
    pub request_size: u64,
    pub response_time_upstream_ms: u64,
    pub custom_headers: Option<HashMap<String, String>>,
}

impl AccessLogEntry {
    pub fn to_json(&self) -> Value {
        let mut json_obj = json!({
            "timestamp": self.timestamp,
            "request_id": self.request_id,
            "method": self.method,
            "uri": self.uri,
            "query_string": self.query_string,
            "status": self.status,
            "upstream": self.upstream,
            "elapsed_ms": self.elapsed_ms,
            "http_version": self.http_version,
            "user_agent": self.user_agent,
            "referer": self.referer,
            "cache_status": self.cache_status,
            "client_ip": self.client_ip,
            "host_header": self.host_header,
            "bytes_sent": self.bytes_sent,
            "bytes_received": self.bytes_received,
            "ssl_protocol": self.ssl_protocol,
            "ssl_cipher": self.ssl_cipher,
            "request_size": self.request_size,
            "response_time_upstream_ms": self.response_time_upstream_ms
        });

        if let Some(custom_headers) = &self.custom_headers {
            if !custom_headers.is_empty() {
                json_obj["custom_headers"] = json!(custom_headers);
            }
        }

        json_obj
    }

    pub fn to_formatted_string(&self) -> String {
        let mut parts = vec![
            format!("timestamp={}", self.timestamp),
            format!("request_id={}", self.request_id),
            format!("method={}", self.method),
            format!("uri={}", self.uri),
            format!("query_string={}", self.query_string),
            format!("status={}", self.status),
            format!("upstream={}", self.upstream),
            format!("elapsed_ms={}", self.elapsed_ms),
            format!("http_version={}", self.http_version),
            format!("user_agent={}", self.user_agent),
            format!("referer={}", self.referer),
            format!("cache={}", self.cache_status),
            format!("client_ip={}", self.client_ip),
            format!("host_header={}", self.host_header),
            format!("bytes_sent={}", self.bytes_sent),
            format!("bytes_received={}", self.bytes_received),
            format!("ssl_protocol={}", self.ssl_protocol),
            format!("ssl_cipher={}", self.ssl_cipher),
            format!("request_size={}", self.request_size),
            format!("response_time_upstream_ms={}", self.response_time_upstream_ms),
        ];

        if let Some(custom_headers) = &self.custom_headers {
            if !custom_headers.is_empty() {
                let headers_str = custom_headers.iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<_>>()
                    .join(",");
                parts.push(format!("custom_headers=[{}]", headers_str));
            }
        }

        parts.join(" ")
    }
}

/// Generate a unique request ID
pub fn generate_request_id() -> String {
    Uuid::new_v4().to_string()
}

/// Extract query string from URI
pub fn extract_query_string(uri: &str) -> (String, Option<String>) {
    if let Some(query_start) = uri.find('?') {
        let (path, query_with_question) = uri.split_at(query_start);
        let query = &query_with_question[1..]; // Remove the '?' character
        if query.is_empty() {
            (path.to_string(), None)
        } else {
            (path.to_string(), Some(query.to_string()))
        }
    } else {
        (uri.to_string(), None)
    }
}

/// Extract client IP from various headers and connection info
pub fn extract_client_ip(
    headers: &std::collections::HashMap<String, String>,
    remote_addr: Option<&str>,
) -> Option<String> {
    // Check X-Forwarded-For header first (most common proxy header)
    if let Some(xff) = headers.get("x-forwarded-for") {
        // Take the first IP in the chain (original client)
        if let Some(first_ip) = xff.split(',').next() {
            let ip = first_ip.trim();
            if !ip.is_empty() && ip != "unknown" {
                return Some(ip.to_string());
            }
        }
    }
    
    // Check X-Real-IP header (nginx proxy)
    if let Some(real_ip) = headers.get("x-real-ip") {
        let ip = real_ip.trim();
        if !ip.is_empty() && ip != "unknown" {
            return Some(ip.to_string());
        }
    }
    
    // Check CF-Connecting-IP header (Cloudflare)
    if let Some(cf_ip) = headers.get("cf-connecting-ip") {
        let ip = cf_ip.trim();
        if !ip.is_empty() && ip != "unknown" {
            return Some(ip.to_string());
        }
    }
    
    // Check X-Client-IP header
    if let Some(client_ip) = headers.get("x-client-ip") {
        let ip = client_ip.trim();
        if !ip.is_empty() && ip != "unknown" {
            return Some(ip.to_string());
        }
    }
    
    // Fall back to remote address from connection
    remote_addr.map(|addr| {
        // Remove port if present (e.g., "192.168.1.1:12345" -> "192.168.1.1")
        if let Some(colon_pos) = addr.rfind(':') {
            addr[..colon_pos].to_string()
        } else {
            addr.to_string()
        }
    })
}

/// Calculate request size including headers and body
pub fn calculate_request_size(
    method: &str,
    uri: &str,
    headers: &std::collections::HashMap<String, String>,
    body_size: Option<u64>,
) -> u64 {
    // Start with request line size: "METHOD URI HTTP/1.1\r\n"
    let request_line_size = method.len() + 1 + uri.len() + 1 + 8 + 2; // +1 for spaces, +8 for "HTTP/1.1", +2 for \r\n
    
    // Calculate headers size
    let headers_size: usize = headers.iter()
        .map(|(name, value)| name.len() + 2 + value.len() + 2) // +2 for ": ", +2 for \r\n
        .sum();
    
    // Add final \r\n after headers
    let headers_end_size = 2;
    
    // Total size
    let total_size = request_line_size + headers_size + headers_end_size;
    
    // Add body size if available
    total_size as u64 + body_size.unwrap_or(0)
}

/// Extract SSL/TLS protocol and cipher information
pub fn extract_ssl_info(
    ssl_context: Option<&str>, // This would come from the TLS connection context
) -> (Option<String>, Option<String>) {
    if let Some(ssl_info) = ssl_context {
        // Parse SSL info string (format depends on implementation)
        // Example: "TLSv1.3:TLS_AES_256_GCM_SHA384"
        if let Some(colon_pos) = ssl_info.find(':') {
            let protocol = ssl_info[..colon_pos].to_string();
            let cipher = ssl_info[colon_pos + 1..].to_string();
            (Some(protocol), Some(cipher))
        } else {
            // If no cipher info, just return protocol
            (Some(ssl_info.to_string()), None)
        }
    } else {
        (None, None)
    }
}

/// Create a comprehensive access log entry with all fields populated
pub fn create_comprehensive_access_entry(
    method: &str,
    uri: &str,
    status: u16,
    upstream: &str,
    elapsed: Duration,
    http_version: &str,
    user_agent: &str,
    referer: &str,
    cache_status: &str,
    headers: &std::collections::HashMap<String, String>,
    remote_addr: Option<&str>,
    bytes_sent: Option<u64>,
    bytes_received: Option<u64>,
    ssl_context: Option<&str>,
    body_size: Option<u64>,
    upstream_response_time: Option<Duration>,
    custom_headers: Option<HashMap<String, String>>,
) -> AccessLogEntry {
    let request_id = generate_request_id();
    let (uri_path, query_string) = extract_query_string(uri);
    let client_ip = extract_client_ip(headers, remote_addr);
    let host_header = headers.get("host").cloned();
    let request_size = Some(calculate_request_size(method, uri, headers, body_size));
    let (ssl_protocol, ssl_cipher) = extract_ssl_info(ssl_context);
    let response_time_upstream_ms = upstream_response_time.map(|d| d.as_millis() as u64);
    
    AccessLogEntry {
        timestamp: Local::now().format("%Y-%m-%d %H:%M:%S%.3f %z").to_string(),
        request_id,
        method: method.to_string(),
        uri: uri_path,
        query_string: query_string.unwrap_or_else(|| "-".to_string()),
        status,
        upstream: upstream.to_string(),
        elapsed_ms: elapsed.as_millis() as u64,
        http_version: http_version.to_string(),
        user_agent: user_agent.to_string(),
        referer: referer.to_string(),
        cache_status: cache_status.to_string(),
        client_ip: client_ip.unwrap_or_else(|| "-".to_string()),
        host_header: host_header.unwrap_or_else(|| "-".to_string()),
        bytes_sent: bytes_sent.unwrap_or(0),
        bytes_received: bytes_received.unwrap_or(0),
        ssl_protocol: ssl_protocol.unwrap_or_else(|| "-".to_string()),
        ssl_cipher: ssl_cipher.unwrap_or_else(|| "-".to_string()),
        request_size: request_size.unwrap_or(0),
        response_time_upstream_ms: response_time_upstream_ms.unwrap_or(0),
        custom_headers,
    }
}

/// Log access information with enhanced capabilities
pub fn log_access_enhanced(entry: &AccessLogEntry, structured_format: bool) {
    // Debug: Log to stderr to verify function is being called
    eprintln!("DEBUG: log_access_enhanced called - method={} uri={} status={}", 
              entry.method, entry.uri, entry.status);
    
    if structured_format {
        tracing::info!(
            target: "stryvo::logging",
            "{}",
            entry.to_json().to_string()
        );
    } else {
        tracing::info!(
            target: "stryvo::logging",
            "{}",
            entry.to_formatted_string()
        );
    }
}

/// Comprehensive access logging with all optional fields
pub fn log_access_comprehensive(
    method: &str,
    uri: &str,
    status: u16,
    upstream: &str,
    elapsed: Duration,
    http_version: &str,
    user_agent: &str,
    referer: &str,
    cache_status: &str,
    request_id: Option<&str>,
    client_ip: Option<&str>,
    host_header: Option<&str>,
    bytes_sent: Option<u64>,
    bytes_received: Option<u64>,
    ssl_protocol: Option<&str>,
    ssl_cipher: Option<&str>,
    request_size: Option<u64>,
    response_time_upstream_ms: Option<u64>,
    custom_headers: Option<HashMap<String, String>>,
    structured_format: bool,
) {
    // Extract query string from URI
    let (uri_path, query_string) = extract_query_string(uri);
    
    let entry = AccessLogEntry {
        timestamp: Local::now().format("%Y-%m-%d %H:%M:%S%.3f %z").to_string(),
        request_id: request_id.map(|s| s.to_string()).unwrap_or_else(|| generate_request_id()),
        method: method.to_string(),
        uri: uri_path,
        query_string: query_string.unwrap_or_else(|| "-".to_string()),
        status,
        upstream: upstream.to_string(),
        elapsed_ms: elapsed.as_millis() as u64,
        http_version: http_version.to_string(),
        user_agent: user_agent.to_string(),
        referer: referer.to_string(),
        cache_status: cache_status.to_string(),
        client_ip: client_ip.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string()),
        host_header: host_header.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string()),
        bytes_sent: bytes_sent.unwrap_or(0),
        bytes_received: bytes_received.unwrap_or(0),
        ssl_protocol: ssl_protocol.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string()),
        ssl_cipher: ssl_cipher.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string()),
        request_size: request_size.unwrap_or(0),
        response_time_upstream_ms: response_time_upstream_ms.unwrap_or(0),
        custom_headers,
    };
    
    log_access_enhanced(&entry, structured_format);
}

/// Log access information (legacy function for backward compatibility)
pub fn log_access(
    method: &str,
    uri: &str,
    status: u16,
    upstream: &str,
    elapsed: Duration,
    http_version: &str,
    user_agent: &str,
    referer: &str,
    cache_status: &str,
) {
    log_access_comprehensive(
        method,
        uri,
        status,
        upstream,
        elapsed,
        http_version,
        user_agent,
        referer,
        cache_status,
        None, // request_id
        None, // client_ip
        None, // host_header
        None, // bytes_sent
        None, // bytes_received
        None, // ssl_protocol
        None, // ssl_cipher
        None, // request_size
        None, // response_time_upstream_ms
        None, // custom_headers
        false, // structured_format
    );
}

/// Request tracing context
#[derive(Debug, Clone)]
pub struct RequestTraceContext {
    pub request_id: String,
    pub span_id: String,
    pub trace_id: String,
    pub custom_attributes: HashMap<String, String>,
}



/// Performance metrics for logging
#[derive(Debug, Clone)]
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

impl PerformanceMetrics {
    pub fn to_json(&self) -> Value {
        let mut json_obj = json!({
            "request_processing_time_ms": self.request_processing_time_ms,
            "total_bytes_sent": self.total_bytes_sent,
            "total_bytes_received": self.total_bytes_received,
            "connection_reused": self.connection_reused
        });

        if let Some(upstream_connect_time) = self.upstream_connect_time_ms {
            json_obj["upstream_connect_time_ms"] = json!(upstream_connect_time);
        }
        if let Some(upstream_response_time) = self.upstream_response_time_ms {
            json_obj["upstream_response_time_ms"] = json!(upstream_response_time);
        }
        if let Some(cache_lookup_time) = self.cache_lookup_time_ms {
            json_obj["cache_lookup_time_ms"] = json!(cache_lookup_time);
        }
        if let Some(ssl_handshake_time) = self.ssl_handshake_time_ms {
            json_obj["ssl_handshake_time_ms"] = json!(ssl_handshake_time);
        }
        if let Some(dns_lookup_time) = self.dns_lookup_time_ms {
            json_obj["dns_lookup_time_ms"] = json!(dns_lookup_time);
        }
        if let Some(compression_ratio) = self.compression_ratio {
            json_obj["compression_ratio"] = json!(compression_ratio);
        }

        json_obj
    }
}

/// Log performance metrics
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
        
        tracing::info!(
            target: "stryvo::performance",
            "{}",
            log_entry.to_string()
        );
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

/// Log error information with enhanced context following standardized format


/// Log error information with comprehensive standardized format
/// Format: 2025-09-15T11:54:50.649855Z ERROR pingora_proxy: Fail to proxy: Upstream H2Error context: while creating new stream cause: connection closed because of a broken pipe, status: 502, tries: 1, retry: false, GET /, Host: test.local:9443
pub fn log_error_comprehensive(
    trace_context: Option<&RequestTraceContext>,
    method: &str,
    uri: &str,
    error: &str,
    error_code: Option<&str>,
    status: Option<u16>,
    host_header: Option<&str>,
    tries: Option<u32>,
    retry: Option<bool>,
    service_identifier: &str,
    structured_format: bool,
) {
    // Use UTC timestamp with Z suffix to match the standardized format
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    
    if structured_format {
        let mut log_entry = json!({
            "timestamp": timestamp,
            "level": "ERROR",
            "service": service_identifier,
            "error": error,
            "method": method,
            "uri": uri
        });
        
        if let Some(ctx) = trace_context {
            log_entry["request_id"] = json!(ctx.request_id);
            log_entry["trace_id"] = json!(ctx.trace_id);
            log_entry["span_id"] = json!(ctx.span_id);
        }
        
        if let Some(code) = error_code {
            log_entry["error_code"] = json!(code);
        }
        
        if let Some(s) = status {
            log_entry["status"] = json!(s);
        }
        
        if let Some(host) = host_header {
            log_entry["host"] = json!(host);
        }
        
        if let Some(t) = tries {
            log_entry["tries"] = json!(t);
        }
        
        if let Some(r) = retry {
            log_entry["retry"] = json!(r);
        }
        
        tracing::error!(
            target: "stryvo::error",
            "{}",
            log_entry.to_string()
        );
    } else {
        // Format: 2025-09-15T11:54:50.649855Z ERROR pingora_proxy: Fail to proxy: Upstream H2Error context: while creating new stream cause: connection closed because of a broken pipe, status: 502, tries: 1, retry: false, GET /, Host: test.local:9443
        let mut log_parts = vec![
            timestamp,
            "ERROR".to_string(),
            format!("{}:", service_identifier),
            error.to_string(),
        ];
        
        // Add status if provided
        if let Some(s) = status {
            log_parts.push(format!("status: {}", s));
        }
        
        // Add tries if provided
        if let Some(t) = tries {
            log_parts.push(format!("tries: {}", t));
        }
        
        // Add retry if provided
        if let Some(r) = retry {
            log_parts.push(format!("retry: {}", r));
        }
        
        // Add method and URI
        log_parts.push(format!("{} {}", method, uri));
        
        // Add host if provided
        if let Some(host) = host_header {
            log_parts.push(format!("Host: {}", host));
        }
        
        // Add trace context if available
        if let Some(ctx) = trace_context {
            log_parts.push(format!("request_id: {}", ctx.request_id));
            log_parts.push(format!("trace_id: {}", ctx.trace_id));
        }
        
        let log_message = log_parts.join(", ");
        
        tracing::error!(
            target: "stryvo::error",
            "{}",
            log_message
        );
    }
}

/// Log proxy error with standardized format matching the example
/// Example: 2025-09-15T11:54:50.649855Z ERROR pingora_proxy: Fail to proxy: Upstream H2Error context: while creating new stream cause: connection closed because of a broken pipe, status: 502, tries: 1, retry: false, GET /, Host: test.local:9443
pub fn log_proxy_error(
    method: &str,
    uri: &str,
    host: Option<&str>,
    error_message: &str,
    status: Option<u16>,
    tries: Option<u32>,
    retry: Option<bool>,
    trace_context: Option<&RequestTraceContext>,
    structured_format: bool,
) {
    log_error_comprehensive(
        trace_context,
        method,
        uri,
        error_message,
        None, // error_code
        status,
        host,
        tries,
        retry,
        "pingora_proxy",
        structured_format,
    );
}

