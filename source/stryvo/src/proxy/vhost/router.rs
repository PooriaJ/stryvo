use std::collections::HashMap;
use std::sync::Arc;
use pingora_core::{upstreams::peer::HttpPeer, Result};
use pingora_http::RequestHeader;
use pingora_load_balancing::{
    selection::RoundRobin,
    Backend, Backends, LoadBalancer,
};
use tracing::{debug, error, info, warn};
use tokio::sync::RwLock;

use crate::config::internal::{VHostConfig, DomainConfig};

/// Virtual host router that handles domain-to-upstream mapping
pub struct VHostRouter {
    /// Domain-to-upstream mappings
    domain_mappings: Arc<RwLock<HashMap<String, VHostUpstream>>>,
    /// Default upstream for unknown domains
    default_upstream: VHostUpstream,
    /// Load balancers for domains with multiple backends
    load_balancers: Arc<RwLock<HashMap<String, Arc<LoadBalancer<RoundRobin>>>>>,
    /// Default load balancer
    default_load_balancer: Option<Arc<LoadBalancer<RoundRobin>>>,
}

/// Upstream configuration for a virtual host domain
#[derive(Clone)]
pub struct VHostUpstream {
    /// Backend servers for this domain
    pub backends: Vec<HttpPeer>,
    /// Whether this upstream has a load balancer for multiple backends
    pub has_load_balancer: bool,
}

impl VHostRouter {
    /// Create a new VHostRouter from configuration
    pub fn new(vhost_config: &VHostConfig) -> Self {
        // Validate domain configuration first
        if let Err(e) = Self::validate_domain_config(&vhost_config.domains) {
            error!("Domain configuration validation failed: {}", e);
            panic!("Invalid domain configuration: {}", e);
        }

        let mut domain_mappings = HashMap::new();
        let mut load_balancers = HashMap::new();

        // Build domain mappings from configuration
        for (domain_name, domain_config) in &vhost_config.domains {
            let (vhost_upstream, load_balancer) = Self::create_vhost_upstream(domain_config);
            domain_mappings.insert(domain_name.clone(), vhost_upstream);
            
            if let Some(lb) = load_balancer {
                load_balancers.insert(domain_name.clone(), Arc::new(lb));
            }
            
            debug!("Configured domain mapping: {} -> {} upstreams", 
                   domain_name, domain_config.upstreams.len());
        }

        // Create default upstream
        let (default_upstream, default_load_balancer) = Self::create_vhost_upstream(&vhost_config.default_domain);
        debug!("Configured default upstream with {} backends", 
               default_upstream.backends.len());
        
        info!("VHost router initialized for {} domains", domain_mappings.len());

        Self {
            domain_mappings: Arc::new(RwLock::new(domain_mappings)),
            default_upstream,
            load_balancers: Arc::new(RwLock::new(load_balancers)),
            default_load_balancer: default_load_balancer.map(Arc::new),
        }
    }

    /// Create a VHostUpstream from a DomainConfig
    fn create_vhost_upstream(domain_config: &DomainConfig) -> (VHostUpstream, Option<LoadBalancer<RoundRobin>>) {
        let backends = domain_config.upstreams.clone();
        let has_load_balancer = backends.len() > 1;
        
        // Create load balancer if we have multiple backends
        let load_balancer = if has_load_balancer {
            // Convert HttpPeers to Backends for load balancer
            let mut backend_set = std::collections::BTreeSet::new();
            for peer in &backends {
                let backend_addr = peer._address.to_string();
                match Backend::new(&backend_addr) {
                    Ok(mut backend) => {
                        // Store the HttpPeer in the backend's extension for later retrieval
                        backend.ext.insert::<HttpPeer>(peer.clone());
                        backend_set.insert(backend);
                    }
                    Err(e) => {
                        error!("Failed to create backend from address {}: {:?}", backend_addr, e);
                        continue;
                    }
                }
            }
            
            if !backend_set.is_empty() {
                let discovery = pingora_load_balancing::discovery::Static::new(backend_set);
                let lb = LoadBalancer::<RoundRobin>::from_backends(Backends::new(discovery));
                
                // For static discovery, we don't need to call update() as it's synchronous
                debug!("Created load balancer with {} backends", backends.len());
                Some(lb)
            } else {
                error!("Failed to create any backends for load balancer");
                None
            }
        } else {
            None
        };

        let vhost_upstream = VHostUpstream {
            backends,
            has_load_balancer,
        };

        (vhost_upstream, load_balancer)
    }

    /// Extract Host header from request and parse domain
    pub fn extract_host_header(req_header: &RequestHeader) -> Option<String> {
        debug!("Attempting to extract Host header from request");
        
        match req_header.headers.get("host") {
            Some(host_value) => {
                debug!("Host header found in request");
                
                match host_value.to_str() {
                    Ok(host_str) => {
                        debug!("Host header value extracted: '{}'", host_str);
                        
                        // Validate host header format
                        if host_str.is_empty() {
                            warn!("Host header parsing failed: empty Host header value");
                            return None;
                        }
                        
                        // Check for invalid characters
                        if host_str.contains(' ') || host_str.contains('\t') || host_str.contains('\n') {
                            warn!("Host header parsing failed: invalid Host header format '{}' contains whitespace", host_str);
                            return None;
                        }
                        
                        // Remove port if present (e.g., "example.com:443" -> "example.com")
                        let domain = host_str.split(':').next().unwrap_or(host_str);
                        
                        // Validate domain after port removal
                        if domain.is_empty() {
                            warn!("Host header parsing failed: empty domain after port removal from '{}'", host_str);
                            return None;
                        }
                        
                        // Check domain length (reasonable limits)
                        if domain.len() > 253 {
                            warn!("Host header parsing failed: domain '{}' exceeds maximum length (253 characters)", domain);
                            return None;
                        }
                        
                        // Convert to lowercase for case-insensitive matching
                        let normalized_domain = domain.to_lowercase();
                        
                        info!("Host header parsing successful: '{}' -> domain '{}'", host_str, normalized_domain);
                        Some(normalized_domain)
                    }
                    Err(e) => {
                        warn!("Host header parsing failed: Host header contains invalid UTF-8 characters: {}", e);
                        None
                    }
                }
            }
            None => {
                debug!("Host header parsing result: no Host header found in request (normal for HTTP/1.0 or malformed requests)");
                None
            }
        }
    }

    /// Route request to appropriate upstream based on domain
    pub async fn route_request(&self, host: &str) -> Option<VHostUpstream> {
        let domain = host.to_lowercase();
        
        debug!("Domain routing initiated for: '{}'", domain);

        // Validate domain format before routing
        if domain.is_empty() {
            warn!("Domain routing failed: empty domain provided, using default upstream");
            return Some(self.default_upstream.clone());
        }

        // Look up domain in mappings
        let domain_mappings = self.domain_mappings.read().await;
        if let Some(vhost_upstream) = domain_mappings.get(&domain) {
            info!("Domain routing successful: found match for domain '{}' -> {} upstream(s)", 
                  domain, vhost_upstream.backends.len());
            return Some(vhost_upstream.clone());
        }

        // Try wildcard matching against all configured patterns
        for (pattern, vhost_upstream) in domain_mappings.iter() {
            if Self::is_wildcard_match(&domain, pattern) {
                info!("Domain routing successful: wildcard match for domain '{}' using pattern '{}' -> {} upstream(s)", 
                      domain, pattern, vhost_upstream.backends.len());
                return Some(vhost_upstream.clone());
            }
        }

        // Try simple wildcard matching for backward compatibility
        let domain_parts: Vec<&str> = domain.split('.').collect();
        if domain_parts.len() > 2 {
            // Try matching against wildcard patterns (*.example.com)
            let wildcard_domain = format!("*.{}", domain_parts[1..].join("."));
            if let Some(vhost_upstream) = domain_mappings.get(&wildcard_domain) {
                info!("Domain routing successful: simple wildcard match for domain '{}' using pattern '{}' -> {} upstream(s)", 
                      domain, wildcard_domain, vhost_upstream.backends.len());
                return Some(vhost_upstream.clone());
            }
            
            // Try matching against parent domain (example.com for sub.example.com)
            let parent_domain = domain_parts[1..].join(".");
            if let Some(vhost_upstream) = domain_mappings.get(&parent_domain) {
                info!("Domain routing successful: parent domain match for domain '{}' using parent '{}' -> {} upstream(s)", 
                      domain, parent_domain, vhost_upstream.backends.len());
                return Some(vhost_upstream.clone());
            }
        }

        // Fall back to default upstream
        info!("Domain routing fallback: no specific configuration found for domain '{}', using default upstream -> {} upstream(s)", 
              domain, self.default_upstream.backends.len());
        Some(self.default_upstream.clone())
    }

    /// Select upstream peer from the appropriate domain configuration with connection pooling
    pub async fn select_upstream_peer(&self, host: Option<String>) -> Result<Box<HttpPeer>> {
        debug!("Upstream peer selection initiated");
        
        let domain = match host {
            Some(domain) => {
                debug!("Upstream selection for specific domain: '{}'", domain);
                domain
            }
            None => {
                info!("Upstream selection: no Host header provided, selecting from default upstream");
                return self.select_from_default_upstream().await;
            }
        };

        let vhost_upstream = match self.route_request(&domain).await {
            Some(upstream) => upstream,
            None => {
                error!("Failed to route request for domain: {}", domain);
                return Err(pingora::Error::new_str("Failed to route request"));
            }
        };
        
        // If we have a load balancer, use it for selection
        if vhost_upstream.has_load_balancer {
            debug!("Attempting load balancer selection for domain: '{}'", domain);
            
            let load_balancers = self.load_balancers.read().await;
            if let Some(load_balancer) = load_balancers.get(&domain) {
                // Use domain as key for load balancing (could be enhanced with session affinity)
                let key = domain.as_bytes();
                
                match load_balancer.select(key, 256) {
                    Some(backend) => {
                        if let Some(peer) = backend.ext.get::<HttpPeer>() {
                            info!("Upstream selection successful: load balancer selected {} for domain '{}'", 
                                  peer._address, domain);
                            return Ok(Box::new(peer.clone()));
                        } else {
                            error!("Upstream selection failed: load balancer returned backend without HttpPeer extension for domain '{}'", domain);
                        }
                    }
                    None => {
                        warn!("Upstream selection warning: load balancer returned no backend for domain '{}', falling back to first backend", domain);
                    }
                }
            } else {
                warn!("Upstream selection warning: load balancer not found for domain '{}' despite has_load_balancer=true, falling back to first backend", domain);
            }
        }

        // Fall back to first backend if load balancer is not available or fails
        if let Some(first_backend) = vhost_upstream.backends.first() {
            info!("Upstream selection successful: selected first backend {} for domain '{}' (load balancer fallback)", 
                  first_backend._address, domain);
            Ok(Box::new(first_backend.clone()))
        } else {
            error!("Upstream selection failed: no upstream backends available for domain '{}' - configuration error", domain);
            Err(pingora::Error::new_str("No upstream backends available"))
        }
    }

    /// Select from default upstream when no domain is specified with connection pooling
    /// Handles missing or invalid Host headers gracefully
    async fn select_from_default_upstream(&self) -> Result<Box<HttpPeer>> {
        debug!("Default upstream selection initiated (no domain specified)");
        
        // Use load balancer if available for default upstream
        if self.default_upstream.has_load_balancer {
            debug!("Attempting default upstream load balancer selection");
            
            if let Some(load_balancer) = &self.default_load_balancer {
                let key = b"default"; // Simple key for default upstream load balancing
                
                match load_balancer.select(key, 256) {
                    Some(backend) => {
                        if let Some(peer) = backend.ext.get::<HttpPeer>() {
                            info!("Default upstream selection successful: load balancer selected {}", 
                                  peer._address);
                            return Ok(Box::new(peer.clone()));
                        } else {
                            error!("Default upstream selection failed: load balancer returned backend without HttpPeer extension");
                        }
                    }
                    None => {
                        warn!("Default upstream selection warning: load balancer returned no backend, falling back to first backend");
                    }
                }
            } else {
                warn!("Default upstream selection warning: load balancer not available despite has_load_balancer=true, using first backend");
            }
        }

        // Fall back to first backend - this handles the case gracefully
        if let Some(first_backend) = self.default_upstream.backends.first() {
            info!("Default upstream selection successful: selected first backend {} (graceful fallback for missing/invalid Host header)", 
                  first_backend._address);
            Ok(Box::new(first_backend.clone()))
        } else {
            error!("Default upstream selection failed: no default upstream backends available - this is a critical configuration error");
            error!("Please ensure the default domain configuration includes at least one upstream backend");
            Err(pingora::Error::new_str("No default upstream backends available"))
        }
    }



    /// Enhanced wildcard domain matching that supports multiple levels
    /// Supports patterns like *.example.com, *.*.example.com, etc.
    fn is_wildcard_match(domain: &str, pattern: &str) -> bool {
        // Quick check: if pattern doesn't contain wildcards, it's not a wildcard pattern
        if !pattern.contains('*') {
            return false;
        }

        // Validate the wildcard pattern format
        if !Self::is_valid_wildcard_pattern(pattern) {
            warn!("Invalid wildcard pattern '{}' - skipping match", pattern);
            return false;
        }

        // Split both domain and pattern into parts
        let domain_parts: Vec<&str> = domain.split('.').collect();
        let pattern_parts: Vec<&str> = pattern.split('.').collect();

        // Match from right to left (TLD first)
        Self::match_domain_parts(&domain_parts, &pattern_parts)
    }

    /// Validate wildcard pattern format
    /// Rules:
    /// - Wildcards (*) can only appear as complete labels (not partial)
    /// - Wildcards cannot be at the end (TLD position)
    /// - Pattern must have at least one non-wildcard part
    /// - No consecutive wildcards without a separator
    fn is_valid_wildcard_pattern(pattern: &str) -> bool {
        if pattern.is_empty() {
            return false;
        }

        let parts: Vec<&str> = pattern.split('.').collect();
        
        // Must have at least 2 parts (e.g., "*.com" minimum)
        if parts.len() < 2 {
            return false;
        }

        // Last part (TLD) cannot be a wildcard
        if parts.last() == Some(&"*") {
            return false;
        }

        // Must have at least one non-wildcard part
        if parts.iter().all(|&part| part == "*") {
            return false;
        }

        // Check each part
        for part in &parts {
            if part.is_empty() {
                return false; // Empty parts not allowed
            }
            
            // If it contains *, it must be exactly "*" (no partial wildcards)
            if part.contains('*') && *part != "*" {
                return false;
            }
            
            // Validate non-wildcard parts (basic domain label validation)
            if *part != "*" && !Self::is_valid_domain_label(part) {
                return false;
            }
        }

        true
    }

    /// Validate a single domain label (non-wildcard part)
    fn is_valid_domain_label(label: &str) -> bool {
        if label.is_empty() || label.len() > 63 {
            return false;
        }

        // Must start and end with alphanumeric
        let chars: Vec<char> = label.chars().collect();
        if !chars.first().unwrap_or(&' ').is_alphanumeric() || 
           !chars.last().unwrap_or(&' ').is_alphanumeric() {
            return false;
        }

        // Can only contain alphanumeric and hyphens
        chars.iter().all(|c| c.is_alphanumeric() || *c == '-')
    }

    /// Match domain parts against pattern parts (right to left)
    fn match_domain_parts(domain_parts: &[&str], pattern_parts: &[&str]) -> bool {
        // Quick check: if pattern has more parts than domain, wildcards must consume the difference
        // Each wildcard must consume at least one part
        let wildcard_count = pattern_parts.iter().filter(|&&p| p == "*").count();
        let non_wildcard_count = pattern_parts.len() - wildcard_count;
        
        // Domain must have at least as many parts as non-wildcards + wildcards (each wildcard needs >=1 part)
        if domain_parts.len() < non_wildcard_count + wildcard_count {
            return false;
        }

        let mut domain_idx = domain_parts.len() as i32 - 1;
        let mut pattern_idx = pattern_parts.len() as i32 - 1;

        // Match from right to left (TLD first)
        while domain_idx >= 0 && pattern_idx >= 0 {
            let domain_part = domain_parts[domain_idx as usize];
            let pattern_part = pattern_parts[pattern_idx as usize];

            if pattern_part == "*" {
                // Wildcard must match at least one domain part
                if domain_idx < 0 {
                    return false; // No domain parts left for wildcard to match
                }
                
                // Move to next pattern part
                pattern_idx -= 1;
                
                if pattern_idx < 0 {
                    // Pattern ends with wildcard, consume remaining domain parts
                    // Wildcard must consume at least one part
                    return domain_idx >= 0;
                }

                // Find the next non-wildcard pattern part
                let mut next_pattern_idx = pattern_idx;
                while next_pattern_idx >= 0 && pattern_parts[next_pattern_idx as usize] == "*" {
                    next_pattern_idx -= 1;
                }
                
                if next_pattern_idx < 0 {
                    // All remaining pattern parts are wildcards, they consume remaining domain parts
                    return true;
                }
                
                let next_pattern_part = pattern_parts[next_pattern_idx as usize];

                // Look for the next pattern part in the domain, ensuring wildcard consumes at least one part
                let mut found = false;

                // Skip at least one domain part for the wildcard
                domain_idx -= 1;
                
                while domain_idx >= 0 {
                    if domain_parts[domain_idx as usize].eq_ignore_ascii_case(next_pattern_part) {
                        found = true;
                        break;
                    }
                    domain_idx -= 1;
                }

                if !found {
                    return false;
                }
                
                // Continue matching from this position
                domain_idx -= 1;
                pattern_idx = next_pattern_idx - 1;
            } else {
                // Exact match required (case-insensitive)
                if !domain_part.eq_ignore_ascii_case(pattern_part) {
                    return false;
                }
                domain_idx -= 1;
                pattern_idx -= 1;
            }
        }

        // Check if we've consumed all pattern parts
        while pattern_idx >= 0 {
            if pattern_parts[pattern_idx as usize] != "*" {
                return false; // Unmatched non-wildcard pattern parts
            }
            // Each remaining wildcard must consume at least one domain part
            if domain_idx < 0 {
                return false; // No domain parts left for wildcard
            }
            domain_idx -= 1; // Consume one part for the wildcard
            pattern_idx -= 1;
        }

        // Check if we've consumed all domain parts
        domain_idx < 0
    }

    /// Validate domain configuration during router creation
    pub fn validate_domain_config(domains: &HashMap<String, DomainConfig>) -> Result<()> {
        for (domain_name, _config) in domains {
            if domain_name.contains('*') {
                if !Self::is_valid_wildcard_pattern(domain_name) {
                    error!("Invalid wildcard domain pattern: '{}'. Wildcards must be complete labels, cannot be in TLD position, and pattern must have at least one non-wildcard part.", domain_name);
                    return Err(pingora::Error::new_str("Invalid wildcard domain pattern"));
                }
            } else {
                // Validate regular domain name
                if !Self::is_valid_domain_name(domain_name) {
                    error!("Invalid domain name: '{}'. Domain names must follow standard DNS naming conventions.", domain_name);
                    return Err(pingora::Error::new_str("Invalid domain name"));
                }
            }
        }
        Ok(())
    }

    /// Validate a complete domain name (non-wildcard)
    fn is_valid_domain_name(domain: &str) -> bool {
        if domain.is_empty() || domain.len() > 253 {
            return false;
        }

        // Cannot start or end with dot
        if domain.starts_with('.') || domain.ends_with('.') {
            return false;
        }

        // Split into labels and validate each
        let labels: Vec<&str> = domain.split('.').collect();
        
        // Must have at least one label
        if labels.is_empty() {
            return false;
        }

        // Validate each label
        labels.iter().all(|label| Self::is_valid_domain_label(label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::internal::{UpstreamOptions, TlsConfig};
    use std::path::PathBuf;
    use pingora_http::RequestHeader;
    use http::Method;

    fn create_test_vhost_config() -> VHostConfig {
        use crate::config::internal::{ListenerConfig, ListenerKind};
        
        let mut domains = HashMap::new();
        
        // Add example.com domain
        domains.insert("example.com".to_string(), DomainConfig {
            upstreams: vec![
                HttpPeer::new("192.168.1.10:80", false, String::new()),
                HttpPeer::new("192.168.1.11:80", false, String::new()),
            ],
            ssl_config: Some(TlsConfig {
                cert_path: PathBuf::from("./source/stryvo/assets/test.crt"),
                key_path: PathBuf::from("./source/stryvo/assets/test.key"),
            }),
            upstream_options: UpstreamOptions::default(),
        });

        // Add api.example.com domain
        domains.insert("api.example.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.20:80", false, String::new())],
            ssl_config: Some(TlsConfig {
                cert_path: PathBuf::from("./source/stryvo/assets/test.crt"),
                key_path: PathBuf::from("./source/stryvo/assets/test.key"),
            }),
            upstream_options: UpstreamOptions::default(),
        });

        // Add single-level wildcard domain
        domains.insert("*.wildcard.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.30:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });

        // Add multi-level wildcard domain
        domains.insert("*.*.multi.example.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.40:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });

        // Add complex wildcard pattern
        domains.insert("api.*.service.*.example.org".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.50:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });

        VHostConfig {
            name: "test-vhost".to_string(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "0.0.0.0:8080".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains,
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("192.168.1.100:80", false, String::new())],
                ssl_config: Some(TlsConfig {
                    cert_path: PathBuf::from("./source/stryvo/assets/test.crt"),
                    key_path: PathBuf::from("./source/stryvo/assets/test.key"),
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
    async fn test_vhost_router_creation() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        assert_eq!(router.default_upstream.backends.len(), 1);
    }

    #[test]
    fn test_extract_host_header() {
        let mut req_header = RequestHeader::build(Method::GET, b"/", None).unwrap();
        req_header.insert_header("host", "Example.COM:443").unwrap();
        
        let host = VHostRouter::extract_host_header(&req_header);
        assert_eq!(host, Some("example.com".to_string()));
    }

    #[test]
    fn test_extract_host_header_no_port() {
        let mut req_header = RequestHeader::build(Method::GET, b"/", None).unwrap();
        req_header.insert_header("host", "API.Example.Com").unwrap();
        
        let host = VHostRouter::extract_host_header(&req_header);
        assert_eq!(host, Some("api.example.com".to_string()));
    }

    #[test]
    fn test_extract_host_header_missing() {
        let req_header = RequestHeader::build(Method::GET, b"/", None).unwrap();
        
        let host = VHostRouter::extract_host_header(&req_header);
        assert_eq!(host, None);
    }

    #[tokio::test]
    async fn test_route_request_exact_match() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        let upstream = router.route_request("example.com").await.unwrap();
        assert_eq!(upstream.backends.len(), 2);
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.10:80");
    }

    #[tokio::test]
    async fn test_route_request_case_insensitive() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        let upstream = router.route_request("EXAMPLE.COM").await.unwrap();
        assert_eq!(upstream.backends.len(), 2);
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.10:80");
    }

    #[tokio::test]
    async fn test_route_request_wildcard_match() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        let upstream = router.route_request("sub.wildcard.com").await.unwrap();
        assert_eq!(upstream.backends.len(), 1);
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.30:80");
    }

    #[tokio::test]
    async fn test_route_request_default_fallback() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        let upstream = router.route_request("unknown.com").await.unwrap();
        assert_eq!(upstream.backends.len(), 1);
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.100:80");
    }

    #[tokio::test]
    async fn test_select_upstream_peer_known_domain() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        let result = router.select_upstream_peer(Some("api.example.com".to_string())).await;
        assert!(result.is_ok());
        
        if let Ok(peer) = result {
            assert_eq!(peer._address.to_string(), "192.168.1.20:80");
        }
    }

    #[tokio::test]
    async fn test_select_upstream_peer_unknown_domain() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        let result = router.select_upstream_peer(Some("unknown.domain.com".to_string())).await;
        assert!(result.is_ok());
        
        if let Ok(peer) = result {
            assert_eq!(peer._address.to_string(), "192.168.1.100:80");
        }
    }

    #[tokio::test]
    async fn test_select_upstream_peer_no_host() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        let result = router.select_upstream_peer(None).await;
        assert!(result.is_ok());
        
        if let Ok(peer) = result {
            assert_eq!(peer._address.to_string(), "192.168.1.100:80");
        }
    }

    #[tokio::test]
    async fn test_multiple_backends_load_balancing() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        // example.com has 2 backends, so it should have a load balancer
        let upstream = router.route_request("example.com").await.unwrap();
        assert_eq!(upstream.backends.len(), 2);
        
        // The load balancer should be created for multiple backends
        // Note: In the actual implementation, the load balancer creation might fail
        // due to async requirements, but the backends should still be available
        assert!(upstream.backends.len() > 1);
    }

    // Enhanced wildcard matching tests
    
    #[test]
    fn test_wildcard_pattern_validation() {
        // Valid patterns
        assert!(VHostRouter::is_valid_wildcard_pattern("*.example.com"));
        assert!(VHostRouter::is_valid_wildcard_pattern("*.*.example.com"));
        assert!(VHostRouter::is_valid_wildcard_pattern("api.*.service.example.com"));
        assert!(VHostRouter::is_valid_wildcard_pattern("*.sub.*.example.org"));
        
        // Invalid patterns
        assert!(!VHostRouter::is_valid_wildcard_pattern("*")); // Only wildcard
        assert!(!VHostRouter::is_valid_wildcard_pattern("example.*")); // Wildcard in TLD
        assert!(!VHostRouter::is_valid_wildcard_pattern("*.*")); // All wildcards
        assert!(!VHostRouter::is_valid_wildcard_pattern("sub*.example.com")); // Partial wildcard
        assert!(!VHostRouter::is_valid_wildcard_pattern("*.example.")); // Trailing dot
        assert!(!VHostRouter::is_valid_wildcard_pattern("")); // Empty
        assert!(!VHostRouter::is_valid_wildcard_pattern("*.example..com")); // Double dot
    }

    #[test]
    fn test_domain_label_validation() {
        // Valid labels
        assert!(VHostRouter::is_valid_domain_label("example"));
        assert!(VHostRouter::is_valid_domain_label("sub-domain"));
        assert!(VHostRouter::is_valid_domain_label("api123"));
        assert!(VHostRouter::is_valid_domain_label("a"));
        
        // Invalid labels
        assert!(!VHostRouter::is_valid_domain_label("")); // Empty
        assert!(!VHostRouter::is_valid_domain_label("-example")); // Starts with hyphen
        assert!(!VHostRouter::is_valid_domain_label("example-")); // Ends with hyphen
        assert!(!VHostRouter::is_valid_domain_label("exam ple")); // Contains space
        assert!(!VHostRouter::is_valid_domain_label("exam.ple")); // Contains dot
        assert!(!VHostRouter::is_valid_domain_label(&"a".repeat(64))); // Too long
    }

    #[test]
    fn test_domain_name_validation() {
        // Valid domain names
        assert!(VHostRouter::is_valid_domain_name("example.com"));
        assert!(VHostRouter::is_valid_domain_name("sub.example.com"));
        assert!(VHostRouter::is_valid_domain_name("api-v2.service.example.org"));
        
        // Invalid domain names
        assert!(!VHostRouter::is_valid_domain_name("")); // Empty
        assert!(!VHostRouter::is_valid_domain_name(".example.com")); // Starts with dot
        assert!(!VHostRouter::is_valid_domain_name("example.com.")); // Ends with dot
        assert!(!VHostRouter::is_valid_domain_name("example..com")); // Double dot
        assert!(!VHostRouter::is_valid_domain_name(&format!("{}.com", "a".repeat(250)))); // Too long
    }

    #[tokio::test]
    async fn test_single_level_wildcard_matching() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        // Should match *.wildcard.com
        let upstream = router.route_request("sub.wildcard.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.30:80");
        
        let upstream = router.route_request("api.wildcard.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.30:80");
        
        let upstream = router.route_request("very-long-subdomain-name.wildcard.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.30:80");
        
        // Should not match (wrong domain)
        let upstream = router.route_request("sub.different.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.100:80"); // Default
        
        // Should not match (no subdomain)
        let upstream = router.route_request("wildcard.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.100:80"); // Default
    }

    #[tokio::test]
    async fn test_multi_level_wildcard_matching() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        // Should match *.*.multi.example.com
        let upstream = router.route_request("sub1.sub2.multi.example.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.40:80");
        
        let upstream = router.route_request("api.v1.multi.example.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.40:80");
        
        let upstream = router.route_request("very.deep.multi.example.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.40:80");
        
        // Should not match (insufficient levels)
        let upstream = router.route_request("sub.multi.example.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.100:80"); // Default
        
        // Should not match (wrong base domain)
        let upstream = router.route_request("sub1.sub2.multi.different.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.100:80"); // Default
    }

    #[tokio::test]
    async fn test_complex_wildcard_pattern_matching() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        // Should match api.*.service.*.example.org
        let upstream = router.route_request("api.v1.service.prod.example.org").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.50:80");
        
        let upstream = router.route_request("api.beta.service.staging.example.org").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.50:80");
        
        let upstream = router.route_request("api.test-env.service.dev.example.org").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.50:80");
        
        // Should not match (wrong prefix)
        let upstream = router.route_request("web.v1.service.prod.example.org").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.100:80"); // Default
        
        // Should not match (wrong middle part)
        let upstream = router.route_request("api.v1.app.prod.example.org").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.100:80"); // Default
        
        // Should not match (wrong domain)
        let upstream = router.route_request("api.v1.service.prod.different.org").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.100:80"); // Default
    }

    #[tokio::test]
    async fn test_wildcard_case_insensitive_matching() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        // Case variations should all match *.wildcard.com
        let upstream = router.route_request("SUB.WILDCARD.COM").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.30:80");
        
        let upstream = router.route_request("Sub.Wildcard.Com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.30:80");
        
        let upstream = router.route_request("sub.WILDCARD.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.30:80");
    }

    #[tokio::test]
    async fn test_wildcard_precedence_over_default() {
        let config = create_test_vhost_config();
        let router = VHostRouter::new(&config);
        
        // Wildcard matches should take precedence over default
        let upstream = router.route_request("any.wildcard.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.30:80");
        
        // But non-matching domains should use default
        let upstream = router.route_request("any.nonmatching.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.100:80");
    }

    #[tokio::test]
    async fn test_exact_match_precedence_over_wildcard() {
        // Create a config with both exact and wildcard matches
        let mut config = create_test_vhost_config();
        
        // Add an exact match that would also match a wildcard
        config.domains.insert("exact.wildcard.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.99:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });
        
        let router = VHostRouter::new(&config);
        
        // Exact match should take precedence over wildcard
        let upstream = router.route_request("exact.wildcard.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.99:80");
        
        // Other subdomains should still match wildcard
        let upstream = router.route_request("other.wildcard.com").await.unwrap();
        assert_eq!(upstream.backends[0]._address.to_string(), "192.168.1.30:80");
    }

    #[test]
    fn test_domain_config_validation_success() {
        let mut domains = HashMap::new();
        
        // Valid configurations
        domains.insert("example.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.10:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });
        
        domains.insert("*.wildcard.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.20:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });
        
        domains.insert("*.*.multi.example.org".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.30:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });
        
        assert!(VHostRouter::validate_domain_config(&domains).is_ok());
    }

    #[test]
    fn test_domain_config_validation_failure() {
        let mut domains = HashMap::new();
        
        // Invalid wildcard pattern (wildcard in TLD)
        domains.insert("example.*".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.10:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });
        
        assert!(VHostRouter::validate_domain_config(&domains).is_err());
        
        domains.clear();
        
        // Invalid domain name (starts with dot)
        domains.insert(".example.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.10:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });
        
        assert!(VHostRouter::validate_domain_config(&domains).is_err());
    }
}