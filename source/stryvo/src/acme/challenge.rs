use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use thiserror::Error;

/// HTTP-01 challenge server for ACME domain validation
#[derive(Debug)]
pub struct HttpChallengeServer {
    /// Challenge tokens and their corresponding key authorizations
    pub(crate) challenges: Arc<RwLock<HashMap<String, String>>>,
    /// Server handle for graceful shutdown
    server_handle: Option<tokio::task::JoinHandle<()>>,
    /// Port the challenge server is running on
    port: u16,
    /// Bind address for the challenge server
    bind_addr: String,
}

/// Errors that can occur during challenge operations
#[derive(Error, Debug)]
pub enum ChallengeError {
    #[error("Failed to start HTTP challenge server: {reason}")]
    ServerStartError { reason: String },
    
    #[error("Challenge server is not running")]
    ServerNotRunning,
    
    #[error("Failed to bind to address {addr}:{port}: {reason}")]
    BindError { addr: String, port: u16, reason: String },
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl HttpChallengeServer {
    /// Create a new HTTP challenge server
    pub fn new(bind_addr: String, port: u16) -> Self {
        Self {
            challenges: Arc::new(RwLock::new(HashMap::new())),
            server_handle: None,
            port,
            bind_addr,
        }
    }
    
    /// Start the HTTP challenge server
    pub async fn start(&mut self) -> Result<(), ChallengeError> {
        if self.server_handle.is_some() {
            return Ok(()); // Already running
        }
        
        let challenges = Arc::clone(&self.challenges);
        let bind_addr = self.bind_addr.clone();
        let port = self.port;
        
        let server_handle = tokio::spawn(async move {
            if let Err(e) = Self::run_server(challenges, bind_addr, port).await {
                tracing::error!("HTTP challenge server error: {}", e);
            }
        });
        
        self.server_handle = Some(server_handle);
        
        // Give the server a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        
        tracing::info!("HTTP challenge server started on {}:{}", self.bind_addr, self.port);
        Ok(())
    }
    
    /// Add a challenge token and key authorization
    pub async fn add_challenge(&self, token: String, key_auth: String) {
        let mut challenges = self.challenges.write().await;
        challenges.insert(token, key_auth);
        tracing::debug!("Added HTTP challenge token");
    }
    
    /// Remove a challenge token
    pub async fn remove_challenge(&self, token: &str) {
        let mut challenges = self.challenges.write().await;
        challenges.remove(token);
        tracing::debug!("Removed HTTP challenge token");
    }
    
    /// Run the HTTP server for challenges
    async fn run_server(
        challenges: Arc<RwLock<HashMap<String, String>>>,
        bind_addr: String,
        port: u16,
    ) -> Result<(), ChallengeError> {
        use tokio::net::TcpListener;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        
        let addr = format!("{}:{}", bind_addr, port);
        let listener = TcpListener::bind(&addr).await
            .map_err(|e| ChallengeError::BindError {
                addr: bind_addr,
                port,
                reason: e.to_string(),
            })?;
        
        tracing::info!("HTTP challenge server listening on {}", addr);
        
        loop {
            match listener.accept().await {
                Ok((mut stream, _)) => {
                    let challenges = Arc::clone(&challenges);
                    
                    tokio::spawn(async move {
                        let mut buffer = [0; 1024];
                        
                        match stream.read(&mut buffer).await {
                            Ok(n) => {
                                let request = String::from_utf8_lossy(&buffer[..n]);
                                
                                if let Some(response) = Self::handle_request(&request, &challenges).await {
                                    if let Err(e) = stream.write_all(response.as_bytes()).await {
                                        tracing::warn!("Failed to write HTTP response: {}", e);
                                    }
                                } else {
                                    let not_found = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                                    if let Err(e) = stream.write_all(not_found.as_bytes()).await {
                                        tracing::warn!("Failed to write 404 response: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to read from HTTP challenge connection: {}", e);
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("Failed to accept HTTP challenge connection: {}", e);
                }
            }
        }
    }
    
    /// Handle an HTTP request for ACME challenge
    async fn handle_request(
        request: &str,
        challenges: &Arc<RwLock<HashMap<String, String>>>,
    ) -> Option<String> {
        // Parse the request line to extract the path
        let lines: Vec<&str> = request.lines().collect();
        if lines.is_empty() {
            return None;
        }
        
        let request_line = lines[0];
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            return None;
        }
        
        let method = parts[0];
        let path = parts[1];
        
        // Only handle GET requests to /.well-known/acme-challenge/
        if method != "GET" || !path.starts_with("/.well-known/acme-challenge/") {
            return None;
        }
        
        // Extract the token from the path
        let token = &path[28..]; // Remove "/.well-known/acme-challenge/"
        
        // Look up the key authorization for this token
        let challenges = challenges.read().await;
        if let Some(key_auth) = challenges.get(token) {
            Some(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                key_auth.len(),
                key_auth
            ))
        } else {
            None
        }
    }
}

impl Drop for HttpChallengeServer {
    fn drop(&mut self) {
        if let Some(handle) = self.server_handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_challenge_server_creation() {
        let server = HttpChallengeServer::new("127.0.0.1".to_string(), 8080);
        assert_eq!(server.port, 8080);
        assert_eq!(server.bind_addr, "127.0.0.1");
        assert!(server.server_handle.is_none());
    }
    
    #[tokio::test]
    async fn test_add_remove_challenge() {
        let server = HttpChallengeServer::new("127.0.0.1".to_string(), 8080);
        
        server.add_challenge("test_token".to_string(), "test_key_auth".to_string()).await;
        
        let challenges = server.challenges.read().await;
        assert_eq!(challenges.get("test_token"), Some(&"test_key_auth".to_string()));
        drop(challenges);
        
        server.remove_challenge("test_token").await;
        
        let challenges = server.challenges.read().await;
        assert!(challenges.get("test_token").is_none());
    }
    
    #[test]
    fn test_handle_request_parsing() {
        let request = "GET /.well-known/acme-challenge/test_token HTTP/1.1\r\nHost: example.com\r\n\r\n";
        
        let lines: Vec<&str> = request.lines().collect();
        let request_line = lines[0];
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        
        assert_eq!(parts[0], "GET");
        assert_eq!(parts[1], "/.well-known/acme-challenge/test_token");
        
        let token = &parts[1][28..];
        assert_eq!(token, "test_token");
    }
}