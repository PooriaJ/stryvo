// Provides asynchronous logging for the Stryvo proxy, supporting access and error logs
// with configurable file outputs and tracing integration. Manages buffered writes and
// graceful shutdown for efficient and reliable log handling.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Local;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::oneshot;
use tokio::task;
use tracing::Level;
use tracing_subscriber::fmt::{self, MakeWriter};
use tracing_subscriber::prelude::*;

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

/// Log access information
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
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f %z").to_string();
    // Use a single pre-formatted message for optimal performance
    tracing::info!(
        target: "stryvo::logging",
        "timestamp={} method={} uri={} status={} upstream={} elapsed_ms={} http_version={} user_agent={} referer={} cache={}",
        timestamp,
        method,
        uri,
        status,
        upstream,
        elapsed.as_millis(),
        http_version,
        user_agent,
        referer,
        cache_status
    );
}

/// Log error information
pub fn log_error(method: &str, uri: &str, error: &str) {
    tracing::error!(
        method = %method,
        uri = %uri,
        error = %error,
        "Error log"
    );
}