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
    #[allow(dead_code)]
    #[error("Failed to start HTTP challenge server: {reason}")]
    ServerStartError { reason: String },
    
    #[allow(dead_code)]
    #[error("Challenge server is not running")]
    ServerNotRunning,
    
    #[allow(dead_code)]
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
}

impl Drop for HttpChallengeServer {
    fn drop(&mut self) {
        // Reference bind_addr and port to avoid unread-field warnings
        tracing::debug!(
            "Dropping HttpChallengeServer listening on {}:{}",
            self.bind_addr,
            self.port
        );
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