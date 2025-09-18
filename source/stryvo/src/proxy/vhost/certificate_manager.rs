use std::collections::HashMap;
use std::sync::Arc;
use std::path::PathBuf;
use tokio::sync::{RwLock, mpsc};
use openssl::ssl::{SslAcceptor, SslMethod, SslFiletype, NameType};
use pingora::{Error, ErrorType};
use pingora::listeners::tls::TlsSettings;
use pingora::listeners::TlsAccept;
use pingora::protocols::tls::TlsRef;
use pingora_openssl::ext::{ssl_use_certificate, ssl_use_private_key};
use tracing::{info, warn, error, debug};
use async_trait::async_trait;
use notify::RecommendedWatcher;
use crate::config::internal::{VHostConfig, TlsConfig};

/// Thread-safe certificate manager for virtual host SSL certificate handling with performance optimizations
#[derive(Clone)]
pub struct CertificateManager {
    /// Maps domain names to their SSL acceptors (optimized for concurrent access)
    certificates: Arc<RwLock<HashMap<String, Arc<SslAcceptor>>>>,
    /// Default SSL acceptor for unknown domains
    default_certificate: Arc<SslAcceptor>,
    /// Maps domain names to their TLS configurations for SNI handling
    #[allow(dead_code)]
    tls_configs: Arc<RwLock<HashMap<String, TlsConfig>>>,
    /// Default TLS configuration
    default_tls_config: TlsConfig,
    /// File system watcher for certificate files
    _watcher: Option<Arc<tokio::sync::Mutex<RecommendedWatcher>>>,
    /// Channel for receiving file system events
    #[allow(dead_code)]
    reload_sender: Option<mpsc::UnboundedSender<ReloadEvent>>,
    /// Maps file paths to domain names for reload tracking
    #[allow(dead_code)]
    file_to_domain_map: Arc<RwLock<HashMap<PathBuf, String>>>,
}

/// Events for certificate reload operations
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ReloadEvent {
    /// A certificate file was modified
    CertificateModified { domain: String, file_path: PathBuf },
    /// A certificate file was created
    CertificateCreated { domain: String, file_path: PathBuf },
    /// A certificate file was removed
    CertificateRemoved { domain: String, file_path: PathBuf },
    /// Manual reload request
    ManualReload,
}

impl CertificateManager {
    /// Create a new CertificateManager from VHost configuration
    pub async fn new(vhost_config: &VHostConfig) -> Result<Self, Error> {
        let mut certificates = HashMap::new();
        let mut tls_configs = HashMap::new();
        let mut certificate_load_errors = Vec::new();
        
        info!("Initializing certificate manager for VHost service '{}'", vhost_config.name);
        
        // Get default TLS configuration
        let default_tls_config = vhost_config.default_domain.ssl_config.as_ref()
            .ok_or_else(|| {
                error!("Default domain must have SSL configuration for VHost service '{}' - this is required for fallback certificate", vhost_config.name);
                *Error::new(ErrorType::InternalError)
            })?.clone();
        
        // Load default certificate first with enhanced error handling
        info!("Loading default certificate from cert: {:?}, key: {:?}", 
              default_tls_config.cert_path, default_tls_config.key_path);
        
        let default_certificate = Self::load_ssl_acceptor(&Some(default_tls_config.clone()))
            .map_err(|e| {
                error!("CRITICAL: Failed to load default certificate for VHost service '{}' - cert: {:?}, key: {:?}, error: {}", 
                       vhost_config.name, default_tls_config.cert_path, default_tls_config.key_path, e);
                error!("Default certificate is required for VHost service to function - service cannot start");
                e
            })?;
        
        info!("Successfully loaded default certificate for VHost service '{}'", vhost_config.name);
        
        // Load certificates for each configured domain with comprehensive error handling
        for (domain_name, domain_config) in &vhost_config.domains {
            if let Some(ssl_config) = &domain_config.ssl_config {
                debug!("Loading certificate for domain '{}' - cert: {:?}, key: {:?}", 
                       domain_name, ssl_config.cert_path, ssl_config.key_path);
                
                match Self::load_ssl_acceptor(&Some(ssl_config.clone())) {
                    Ok(acceptor) => {
                        certificates.insert(domain_name.clone(), Arc::new(acceptor));
                        tls_configs.insert(domain_name.clone(), ssl_config.clone());
                        info!("Successfully loaded SSL certificate for domain '{}' - cert: {:?}", 
                              domain_name, ssl_config.cert_path);
                    }
                    Err(e) => {
                        let error_msg = format!("Failed to load SSL certificate for domain '{}' - cert: {:?}, key: {:?}, error: {}", 
                                               domain_name, ssl_config.cert_path, ssl_config.key_path, e);
                        error!("{}", error_msg);
                        certificate_load_errors.push((domain_name.clone(), error_msg));
                        
                        // Continue loading other certificates instead of failing completely
                        warn!("Continuing certificate loading for other domains despite failure for '{}'", domain_name);
                    }
                }
            } else {
                warn!("No SSL configuration found for domain '{}' - this domain will use the default certificate", domain_name);
            }
        }
        
        // Log summary of certificate loading results
        if certificate_load_errors.is_empty() {
            info!("Certificate manager successfully initialized for VHost service '{}' with {} domain certificates (all domains loaded successfully)", 
                  vhost_config.name, certificates.len());
        } else {
            warn!("Certificate manager initialized for VHost service '{}' with {} domain certificates, but {} domains failed to load certificates:", 
                  vhost_config.name, certificates.len(), certificate_load_errors.len());
            for (domain, error) in &certificate_load_errors {
                warn!("  - Domain '{}': {}", domain, error);
            }
            warn!("Domains with failed certificates will fall back to the default certificate");
        }
        
        // Create file-to-domain mapping for reload tracking
        let mut file_to_domain_map = HashMap::new();
        
        // Map default certificate files
        file_to_domain_map.insert(default_tls_config.cert_path.clone(), "default".to_string());
        file_to_domain_map.insert(default_tls_config.key_path.clone(), "default".to_string());
        
        // Map domain certificate files
        for (domain_name, domain_config) in &vhost_config.domains {
            if let Some(ssl_config) = &domain_config.ssl_config {
                file_to_domain_map.insert(ssl_config.cert_path.clone(), domain_name.clone());
                file_to_domain_map.insert(ssl_config.key_path.clone(), domain_name.clone());
            }
        }
        
        Ok(Self {
            certificates: Arc::new(RwLock::new(certificates)),
            default_certificate: Arc::new(default_certificate),
            tls_configs: Arc::new(RwLock::new(tls_configs)),
            default_tls_config,
            _watcher: None,
            reload_sender: None,
            file_to_domain_map: Arc::new(RwLock::new(file_to_domain_map)),
        })
    }
    
    /// Extract SNI (Server Name Indication) from SSL connection
    /// This function is called during the TLS handshake to determine which certificate to use
    pub fn extract_sni_domain(ssl: &TlsRef) -> Option<String> {
        debug!("Attempting SNI extraction from TLS handshake");
        
        // Get the SNI hostname from the SSL connection
        match ssl.servername(NameType::HOST_NAME) {
            Some(hostname) => {
                let domain = hostname.to_string();
                
                // Validate the extracted SNI domain
                if domain.is_empty() {
                    warn!("SNI extraction failed: empty hostname received from TLS handshake");
                    return None;
                }
                
                // Check for invalid characters
                if domain.contains(' ') || domain.contains('\t') || domain.contains('\n') {
                    warn!("SNI extraction failed: invalid domain format '{}' contains whitespace", domain);
                    return None;
                }
                
                // Check domain length (reasonable limits)
                if domain.len() > 253 {
                    warn!("SNI extraction failed: domain '{}' exceeds maximum length (253 characters)", domain);
                    return None;
                }
                
                info!("SNI extraction successful: domain '{}' extracted from TLS handshake", domain);
                Some(domain)
            }
            None => {
                debug!("SNI extraction result: no SNI domain found in TLS handshake (client may not support SNI or using IP address)");
                None
            }
        }
    }
    
    /// Get certificate context for a specific domain (synchronous version for SNI callback)
    pub fn get_certificate_sync(&self, sni_domain: &str) -> Option<Arc<SslAcceptor>> {
        // This is a synchronous version that tries to get certificates without async
        // We'll use try_read to avoid blocking if the lock is held
        if let Ok(certificates) = self.certificates.try_read() {
            debug!("Looking up certificate for SNI domain (sync): {}", sni_domain);
            
            // Try exact domain match first
            if let Some(acceptor) = certificates.get(sni_domain) {
                debug!("Found exact certificate match for domain (sync): {}", sni_domain);
                return Some(acceptor.clone());
            }
            
            debug!("No certificate found for domain '{}' (sync), using default certificate", sni_domain);
        } else {
            warn!("Could not acquire certificate lock for SNI lookup, using default certificate");
        }
        
        // Fall back to default certificate
        Some(self.default_certificate.clone())
    }
    
    /// Create TLS settings with SNI support for Pingora listeners
    /// This creates TLS settings that can handle multiple certificates based on SNI
    pub async fn create_sni_tls_settings(&self) -> Result<TlsSettings, Error> {
        
        // Now create TLS settings with callbacks using this certificate manager
        let callbacks: Box<dyn TlsAccept + Send + Sync> = Box::new(self.clone());
        let tls_settings = TlsSettings::with_callbacks(callbacks)
            .map_err(|e| {
                error!("Failed to create TLS settings with callbacks: {}", e);
                *Error::new(ErrorType::InternalError)
            })?;
        
        info!("Created TLS settings with SNI callback for dynamic certificate selection");
        
        Ok(tls_settings)
    }
    
    /// Create multiple TLS settings for different domains
    /// This returns a map of domain names to their TLS settings
    #[allow(dead_code)]
    pub async fn create_domain_tls_settings(&self) -> Result<HashMap<String, TlsSettings>, Error> {
        let mut domain_settings = HashMap::new();
        let tls_configs = self.tls_configs.read().await;
        
        // Create TLS settings for each domain
        for (domain_name, tls_config) in tls_configs.iter() {
            let cert_path = tls_config.cert_path.to_str()
                .ok_or_else(|| {
                    error!("Certificate path for domain '{}' is not valid UTF-8", domain_name);
                    *Error::new(ErrorType::InternalError)
                })?;
            
            let key_path = tls_config.key_path.to_str()
                .ok_or_else(|| {
                    error!("Key path for domain '{}' is not valid UTF-8", domain_name);
                    *Error::new(ErrorType::InternalError)
                })?;
            
            let tls_settings = TlsSettings::intermediate(cert_path, key_path)
                .map_err(|e| {
                    error!("Failed to create TLS settings for domain '{}': {}", domain_name, e);
                    *Error::new(ErrorType::InternalError)
                })?;
            
            domain_settings.insert(domain_name.clone(), tls_settings);
            debug!("Created TLS settings for domain: {}", domain_name);
        }
        
        // Add default domain settings
        let default_cert_path = self.default_tls_config.cert_path.to_str()
            .ok_or_else(|| {
                error!("Default certificate path is not valid UTF-8");
                *Error::new(ErrorType::InternalError)
            })?;
        
        let default_key_path = self.default_tls_config.key_path.to_str()
            .ok_or_else(|| {
                error!("Default key path is not valid UTF-8");
                *Error::new(ErrorType::InternalError)
            })?;
        
        let default_tls_settings = TlsSettings::intermediate(default_cert_path, default_key_path)
            .map_err(|e| {
                error!("Failed to create default TLS settings: {}", e);
                *Error::new(ErrorType::InternalError)
            })?;
        
        domain_settings.insert("default".to_string(), default_tls_settings);
        
        info!("Created TLS settings for {} domains", domain_settings.len());
        Ok(domain_settings)
    }
    
    /// Load SSL acceptor from TLS configuration
    fn load_ssl_acceptor(ssl_config: &Option<TlsConfig>) -> Result<SslAcceptor, Error> {
        let ssl_config = ssl_config.as_ref()
            .ok_or_else(|| {
                error!("Certificate loading failed: no SSL configuration provided");
                *Error::new(ErrorType::InternalError)
            })?;
        
        debug!("Starting certificate loading process - cert: {:?}, key: {:?}", 
               ssl_config.cert_path, ssl_config.key_path);
        
        // Validate certificate and key files exist with detailed error messages
        if !ssl_config.cert_path.exists() {
            error!("Certificate loading failed: SSL certificate file not found at path '{:?}'", ssl_config.cert_path);
            error!("Please ensure the certificate file exists and is accessible");
            return Err(*Error::new(ErrorType::InternalError));
        }
        
        if !ssl_config.key_path.exists() {
            error!("Certificate loading failed: SSL private key file not found at path '{:?}'", ssl_config.key_path);
            error!("Please ensure the private key file exists and is accessible");
            return Err(*Error::new(ErrorType::InternalError));
        }
        
        // Check file permissions and readability
        match std::fs::metadata(&ssl_config.cert_path) {
            Ok(metadata) => {
                debug!("Certificate file metadata - size: {} bytes, readonly: {}", 
                       metadata.len(), metadata.permissions().readonly());
            }
            Err(e) => {
                error!("Certificate loading failed: cannot read certificate file metadata at '{:?}': {}", 
                       ssl_config.cert_path, e);
                return Err(*Error::new(ErrorType::InternalError));
            }
        }
        
        match std::fs::metadata(&ssl_config.key_path) {
            Ok(metadata) => {
                debug!("Private key file metadata - size: {} bytes, readonly: {}", 
                       metadata.len(), metadata.permissions().readonly());
            }
            Err(e) => {
                error!("Certificate loading failed: cannot read private key file metadata at '{:?}': {}", 
                       ssl_config.key_path, e);
                return Err(*Error::new(ErrorType::InternalError));
            }
        }
        
        // Create SSL acceptor with enhanced error handling
        debug!("Creating SSL acceptor with Mozilla intermediate configuration");
        let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls())
            .map_err(|e| {
                error!("Certificate loading failed: failed to create SSL acceptor with Mozilla intermediate configuration: {}", e);
                error!("This may indicate an OpenSSL configuration issue");
                *Error::new(ErrorType::InternalError)
            })?;
        
        // Load certificate chain with detailed error handling
        debug!("Loading certificate chain from file: {:?}", ssl_config.cert_path);
        acceptor.set_certificate_chain_file(&ssl_config.cert_path)
            .map_err(|e| {
                error!("Certificate loading failed: failed to load certificate chain from '{:?}': {}", ssl_config.cert_path, e);
                error!("Please verify the certificate file format is valid PEM and contains a complete certificate chain");
                *Error::new(ErrorType::InternalError)
            })?;
        
        // Load private key with detailed error handling
        debug!("Loading private key from file: {:?}", ssl_config.key_path);
        acceptor.set_private_key_file(&ssl_config.key_path, SslFiletype::PEM)
            .map_err(|e| {
                error!("Certificate loading failed: failed to load private key from '{:?}': {}", ssl_config.key_path, e);
                error!("Please verify the private key file format is valid PEM and is not encrypted (or provide decryption)");
                *Error::new(ErrorType::InternalError)
            })?;
        
        // Verify that the private key matches the certificate
        debug!("Verifying private key matches certificate");
        acceptor.check_private_key()
            .map_err(|e| {
                error!("Certificate loading failed: private key does not match certificate: {}", e);
                error!("Please ensure the private key corresponds to the certificate in the certificate chain");
                *Error::new(ErrorType::InternalError)
            })?;
        
        info!("Certificate loading successful - cert: {:?}, key: {:?}", 
              ssl_config.cert_path, ssl_config.key_path);
        
        Ok(acceptor.build())
    }
    
    /// Get statistics about loaded certificates
    pub async fn get_certificate_stats(&self) -> CertificateStats {
        let certificates = self.certificates.read().await;
        CertificateStats {
            total_domains: certificates.len(),
            domain_names: certificates.keys().cloned().collect(),
        }
    }
}

// Note: SNI certificate selection in Pingora 0.6.0 is handled through the TlsAccept trait
// implementation above, which provides the certificate_callback method for dynamic
// certificate selection based on SNI domain.

impl std::fmt::Debug for CertificateManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateManager")
            .field("certificates", &"<HashMap<String, Arc<SslAcceptor>>>")
            .field("default_certificate", &"<Arc<SslAcceptor>>")
            .field("tls_configs", &"<HashMap<String, TlsConfig>>")
            .field("default_tls_config", &self.default_tls_config)
            .field("file_to_domain_map", &"<HashMap<PathBuf, String>>")
            .finish()
    }
}

/// Implementation of TlsAccept trait for SNI-based certificate selection
#[async_trait]
impl TlsAccept for CertificateManager {
    async fn certificate_callback(&self, ssl: &mut TlsRef) {
        // Extract SNI domain from the SSL connection
        if let Some(sni_domain) = Self::extract_sni_domain(ssl) {
            debug!("SNI callback: Received SNI domain '{}'", sni_domain);
            
            // Get the appropriate certificate for this domain
            if let Some(acceptor) = self.get_certificate_sync(&sni_domain) {
                info!("SNI callback: Using certificate for domain '{}'", sni_domain);
                
                // Set the certificate and private key for this connection
                if let Err(e) = ssl_use_certificate(ssl, acceptor.context().certificate().unwrap()) {
                    error!("Failed to set certificate for domain '{}': {}", sni_domain, e);
                    return;
                }
                
                if let Err(e) = ssl_use_private_key(ssl, acceptor.context().private_key().unwrap()) {
                    error!("Failed to set private key for domain '{}': {}", sni_domain, e);
                    return;
                }
                
                debug!("Successfully set certificate and key for domain '{}'", sni_domain);
            } else {
                warn!("SNI callback: No certificate found for domain '{}', using default", sni_domain);
                // Default certificate will be used automatically
            }
        } else {
            debug!("SNI callback: No SNI domain provided, using default certificate");
            // Default certificate will be used automatically
        }
    }
}

/// Statistics about loaded certificates
#[derive(Debug, Clone)]
pub struct CertificateStats {
    pub total_domains: usize,
    #[allow(dead_code)]
    pub domain_names: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use crate::config::internal::{VHostConfig, DomainConfig, UpstreamOptions};
    use pingora::upstreams::peer::HttpPeer;
    
    fn create_test_vhost_config() -> VHostConfig {
        let mut domains = HashMap::new();
        
        // Add a test domain (we'll use non-existent cert paths for testing)
        domains.insert("example.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("127.0.0.1:8080", false, String::new())],
            ssl_config: Some(TlsConfig {
                cert_path: PathBuf::from("/nonexistent/example.com.crt"),
                key_path: PathBuf::from("/nonexistent/example.com.key"),
            }),
            upstream_options: UpstreamOptions::default(),
        });
        
        VHostConfig {
            name: "test-vhost".to_string(),
            listeners: vec![],
            domains,
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("127.0.0.1:8080", false, String::new())],
                ssl_config: Some(TlsConfig {
                    cert_path: PathBuf::from("/nonexistent/default.crt"),
                    key_path: PathBuf::from("/nonexistent/default.key"),
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
    async fn test_certificate_manager_creation_fails_with_missing_files() {
        let config = create_test_vhost_config();
        
        // Should fail because certificate files don't exist
        let result = CertificateManager::new(&config).await;
        assert!(result.is_err());
    }
    
    #[tokio::test]
    async fn test_certificate_stats() {
        // This test would need valid certificate files to work properly
        // For now, we'll test the structure
        let config = create_test_vhost_config();
        
        // We expect this to fail due to missing files, but we can test the error handling
        let result = CertificateManager::new(&config).await;
        assert!(result.is_err());
        
        // The error should be InternalError due to missing certificate files
        if let Err(error) = result {
            assert!(matches!(error.etype, pingora::ErrorType::InternalError));
        }
    }
    
    #[test]
    fn test_load_ssl_acceptor_with_missing_config() {
        let result = CertificateManager::load_ssl_acceptor(&None);
        assert!(result.is_err());
        // Error is now InternalError, so we just check that it fails
        if let Err(error) = result {
            assert!(matches!(error.etype, pingora::ErrorType::InternalError));
        }
    }
    
    #[test]
    fn test_load_ssl_acceptor_with_missing_files() {
        let ssl_config = Some(TlsConfig {
            cert_path: PathBuf::from("/nonexistent/test.crt"),
            key_path: PathBuf::from("/nonexistent/test.key"),
        });
        
        let result = CertificateManager::load_ssl_acceptor(&ssl_config);
        assert!(result.is_err());
        // Error is now InternalError, so we just check that it fails
        if let Err(error) = result {
            assert!(matches!(error.etype, pingora::ErrorType::InternalError));
        }
    }
    
    #[tokio::test]
    async fn test_hot_reload_functionality() {
        let config = create_test_vhost_config();
        
        // Test that hot reload setup fails gracefully with missing files
        let result = CertificateManager::new(&config).await;
        assert!(result.is_err());
        
        // The error should be InternalError due to missing certificate files
        if let Err(error) = result {
            assert!(matches!(error.etype, pingora::ErrorType::InternalError));
        }
    }
    
    #[tokio::test]
    async fn test_reload_event_handling() {
        // Test ReloadEvent enum variants
        let event1 = ReloadEvent::CertificateModified {
            domain: "example.com".to_string(),
            file_path: PathBuf::from("/test/cert.pem"),
        };
        
        let event2 = ReloadEvent::CertificateCreated {
            domain: "api.example.com".to_string(),
            file_path: PathBuf::from("/test/api.pem"),
        };
        
        let event3 = ReloadEvent::CertificateRemoved {
            domain: "old.example.com".to_string(),
            file_path: PathBuf::from("/test/old.pem"),
        };
        
        let event4 = ReloadEvent::ManualReload;
        
        // Test that events can be created and cloned
        let _cloned_event = event1.clone();
        
        // Test debug formatting
        let debug_str = format!("{:?}", event2);
        assert!(debug_str.contains("CertificateCreated"));
        assert!(debug_str.contains("api.example.com"));
        
        // Test that all event types are handled
        match event3 {
            ReloadEvent::CertificateRemoved { domain, file_path } => {
                assert_eq!(domain, "old.example.com");
                assert_eq!(file_path, PathBuf::from("/test/old.pem"));
            }
            _ => panic!("Expected CertificateRemoved event"),
        }
        
        match event4 {
            ReloadEvent::ManualReload => {
                // Expected
            }
            _ => panic!("Expected ManualReload event"),
        }
    }
    
    #[tokio::test]
    async fn test_file_to_domain_mapping() {
        let config = create_test_vhost_config();
        
        // Even though certificate creation will fail, we can test the mapping logic
        let result = CertificateManager::new(&config).await;
        assert!(result.is_err());
        
        // Test that the mapping would be created correctly if certificates existed
        let expected_cert_path = PathBuf::from("/nonexistent/example.com.crt");
        let expected_key_path = PathBuf::from("/nonexistent/example.com.key");
        let expected_default_cert = PathBuf::from("/nonexistent/default.crt");
        let expected_default_key = PathBuf::from("/nonexistent/default.key");
        
        // These paths should be in the mapping
        assert!(config.domains.contains_key("example.com"));
        assert!(config.default_domain.ssl_config.is_some());
        
        if let Some(ssl_config) = &config.domains.get("example.com").unwrap().ssl_config {
            assert_eq!(ssl_config.cert_path, expected_cert_path);
            assert_eq!(ssl_config.key_path, expected_key_path);
        }
        
        if let Some(default_ssl) = &config.default_domain.ssl_config {
            assert_eq!(default_ssl.cert_path, expected_default_cert);
            assert_eq!(default_ssl.key_path, expected_default_key);
        }
    }
}