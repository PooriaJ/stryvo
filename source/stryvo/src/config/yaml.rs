//! Configuration sourced from a YAML file

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use pingora::upstreams::peer::HttpPeer;
use serde::{Deserialize, Serialize};

use super::internal::UpstreamOptions;
use crate::acme::config::AcmeConfig;

/// Configuration used for YAML formatted files
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct Yaml {
    /// System-wide configuration values
    #[serde(default)]
    pub system: System,

    /// Configuration for each Basic Proxy instance
    #[serde(default = "Vec::new")]
    pub basic_proxy: Vec<ProxyConfig>,

    /// Configuration for each Virtual Host Proxy instance
    #[serde(default = "Vec::new")]
    pub vhost_proxy: Vec<VHostProxyConfig>,

    /// Configuration for each File Server instance
    #[serde(default = "Vec::new")]
    pub file_server: Vec<FileServerConfig>,

    /// ACME configuration for automatic SSL certificate management
    #[serde(default)]
    pub acme: AcmeConfig,
}

impl Yaml {
    pub fn from_path<P>(path: &P) -> Self
    where
        P: AsRef<Path> + core::fmt::Debug + ?Sized,
    {
        tracing::info!("Loading YAML from {path:?}");
        let f = std::fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("Failed to load file at {path:?}"));
        let t = serde_yaml::from_str(&f).expect("failed to deserialize YAML");
        tracing::info!("YAML file contents: {t:?}");
        t
    }
}

//
// System Config
//

/// System level configuration options
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct System {
    #[serde(default = "System::default_threads_per_service")]
    pub threads_per_service: usize,
    
    /// Whether the server should run as a background daemon process
    #[serde(default)]
    pub daemonize: bool,
    
    /// Path to the PID file used for tracking the daemon process
    pub pid_file: Option<PathBuf>,
    
    /// Path to the socket used for zero-downtime upgrades
    pub upgrade_socket: Option<PathBuf>,
    
    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl Default for System {
    fn default() -> Self {
        System {
            threads_per_service: Self::default_threads_per_service(),
            daemonize: false,
            pid_file: None,
            upgrade_socket: None,
            logging: LoggingConfig::default(),
        }
    }
}

impl System {
    fn default_threads_per_service() -> usize {
        8
    }
}

/// Logging configuration
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct LoggingConfig {
    /// Path for HTTP access logs
    pub access_log: Option<String>,
    
    /// Path for server error logs
    pub error_log: Option<String>,
    
    /// Logging verbosity level
    #[serde(default = "LoggingConfig::default_level")]
    pub level: String,
    
    /// Master switch to enable/disable all logging
    #[serde(default = "LoggingConfig::default_enabled")]
    pub enabled: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig {
            access_log: Some("stryvo-access.log".to_string()),
            error_log: Some("stryvo-error.log".to_string()),
            level: Self::default_level(),
            enabled: Self::default_enabled(),
        }
    }
}

impl LoggingConfig {
    fn default_level() -> String {
        "INFO".to_string()
    }
    
    fn default_enabled() -> bool {
        true
    }
}

/// Add Path Control Modifiers
///
/// Note that we use `BTreeMap` and NOT `HashMap`, as we want to maintain the
/// ordering from the configuration file.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub struct PathControl {
    #[serde(default = "Vec::new")]
    pub request_filters: Vec<BTreeMap<String, String>>,
    #[serde(default = "Vec::new")]
    pub upstream_request_filters: Vec<BTreeMap<String, String>>,
    #[serde(default = "Vec::new")]
    pub upstream_response_filters: Vec<BTreeMap<String, String>>,
}

/// Rate limiting configuration
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub struct RateLimiting {
    #[serde(default = "Vec::new")]
    pub rules: Vec<RateLimitRule>,
}

/// Individual rate limiting rule
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct RateLimitRule {
    pub kind: String,
    pub pattern: Option<String>,
    pub max_buckets: Option<u32>,
    pub tokens_per_bucket: u32,
    pub refill_qty: u32,
    pub refill_rate_ms: u64,
}

/// Caching configuration
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct CacheConfig {
    /// Whether caching is enabled
    #[serde(default)]
    pub cache_enabled: bool,
    
    /// Time-to-live for cached responses in seconds
    #[serde(default = "CacheConfig::default_cache_ttl")]
    pub cache_ttl: u64,
    
    /// Number of internal cache divisions for performance
    #[serde(default = "CacheConfig::default_shard_count")]
    pub shard_count: u32,
    
    /// Directory where cached responses are stored
    pub cache_dir: Option<PathBuf>,
    
    /// Maximum size of cacheable response (bytes)
    #[serde(default = "CacheConfig::default_max_file_size")]
    pub max_file_size: usize,
    
    /// Whether to send cache control headers to browsers
    #[serde(default)]
    pub browser_cache_enabled: bool,
    
    /// TTL for browser caching when enabled (seconds)
    #[serde(default = "CacheConfig::default_browser_cache_ttl")]
    pub browser_cache_ttl: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            cache_enabled: false,
            cache_ttl: Self::default_cache_ttl(),
            shard_count: Self::default_shard_count(),
            cache_dir: None,
            max_file_size: Self::default_max_file_size(),
            browser_cache_enabled: false,
            browser_cache_ttl: Self::default_browser_cache_ttl(),
        }
    }
}

impl CacheConfig {
    fn default_cache_ttl() -> u64 {
        300
    }
    
    fn default_shard_count() -> u32 {
        32
    }
    
    fn default_max_file_size() -> usize {
        1048576 // 1MB
    }
    
    fn default_browser_cache_ttl() -> u64 {
        3600 // 1 hour
    }
}

//
// Basic Proxy Configuration
//

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct ProxyConfig {
    /// Name of the Service. Used for logging.
    pub name: String,

    /// Listeners - or "downstream" interfaces we listen to
    #[serde(default = "Vec::new")]
    pub listeners: Vec<ListenerConfig>,

    /// Connectors - our "upstream" servers
    #[serde(default = "Vec::new")]
    pub connectors: Vec<ConnectorConfig>,

    /// Load balancing configuration
    #[serde(default)]
    pub load_balance: Option<LoadBalanceConfig>,

    /// Path Control, for modifying and filtering requests
    #[serde(default)]
    pub path_control: PathControl,

    /// Rate limiting configuration
    #[serde(default)]
    pub rate_limiting: RateLimiting,

    /// Caching configuration
    #[serde(default)]
    pub cache: CacheConfig,
}

/// Load balancing configuration
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct LoadBalanceConfig {
    /// Selection algorithm
    #[serde(default = "LoadBalanceConfig::default_selection")]
    pub selection: String,
    
    /// Selection key for consistent hashing
    pub key: Option<String>,
    
    /// Discovery method
    #[serde(default = "LoadBalanceConfig::default_discovery")]
    pub discovery: String,
    
    /// Health check configuration
    #[serde(default = "LoadBalanceConfig::default_health_check")]
    pub health_check: String,
}

impl Default for LoadBalanceConfig {
    fn default() -> Self {
        LoadBalanceConfig {
            selection: Self::default_selection(),
            key: None,
            discovery: Self::default_discovery(),
            health_check: Self::default_health_check(),
        }
    }
}

impl LoadBalanceConfig {
    fn default_selection() -> String {
        "RoundRobin".to_string()
    }
    
    fn default_discovery() -> String {
        "Static".to_string()
    }
    
    fn default_health_check() -> String {
        "None".to_string()
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct ConnectorConfig {
    /// Proxy Address, e.g. `IP:port`
    pub proxy_addr: String,
    /// TLS SNI, if TLS should be used
    pub tls_sni: Option<String>,
    /// Protocol to use (h1, h2, h2-or-h1)
    pub proto: Option<String>,
}

impl From<ConnectorConfig> for HttpPeer {
    fn from(val: ConnectorConfig) -> Self {
        use pingora::protocols::ALPN;
        
        let (tls, sni) = if let Some(sni) = val.tls_sni {
            (true, sni)
        } else {
            (false, String::new())
        };
        
        let alpn = match val.proto.as_deref() {
            Some("h1-only") => ALPN::H1,
            Some("h2-only") => ALPN::H2,
            Some("h1-or-h2") | Some("h2-or-h1") => ALPN::H2H1,
            None => if tls { ALPN::H2H1 } else { ALPN::H1 },
            _ => ALPN::H1, // Default fallback
        };
        
        let mut peer = HttpPeer::new(&val.proxy_addr, tls, sni);
        peer.options.alpn = alpn;
        peer
    }
}

//
// Virtual Host Proxy Configuration
//

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct VHostProxyConfig {
    /// Name of the Service. Used for logging.
    pub name: String,

    /// Listeners - or "downstream" interfaces we listen to
    #[serde(default = "Vec::new")]
    pub listeners: Vec<ListenerConfig>,

    /// Domain configurations
    #[serde(default)]
    pub domains: BTreeMap<String, DomainConfig>,

    /// Default domain configuration
    pub default_domain: DomainConfig,

    /// Path Control, for modifying and filtering requests
    #[serde(default)]
    pub path_control: PathControl,

    /// Rate limiting configuration
    #[serde(default)]
    pub rate_limiting: RateLimiting,

    /// Caching configuration
    #[serde(default)]
    pub cache: CacheConfig,
}

/// Configuration for a single domain in a virtual host setup
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct DomainConfig {
    /// Upstream servers for this domain
    #[serde(default = "Vec::new")]
    pub upstreams: Vec<ConnectorConfig>,

    /// SSL/TLS configuration for this domain
    pub ssl_config: Option<TlsConfig>,

    /// Load balancing configuration for this domain
    #[serde(default)]
    pub load_balance: Option<LoadBalanceConfig>,
}

//
// File Server Configuration
//

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct FileServerConfig {
    /// Name of the Service. Used for logging.
    pub name: String,

    /// Listeners - or "downstream" interfaces we listen to
    #[serde(default = "Vec::new")]
    pub listeners: Vec<ListenerConfig>,

    /// Base path for file serving
    pub base_path: Option<PathBuf>,
}

//
// Common Configuration Types
//

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// Enable automatic SSL certificate management via ACME
    #[serde(default)]
    pub auto_ssl: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct ListenerConfig {
    /// Address to bind to (e.g., "0.0.0.0:8080")
    pub addr: String,
    /// TLS configuration (legacy mode with specific cert/key)
    pub tls: Option<TlsConfig>,
    /// SSL flag for SNI-based certificate selection
    #[serde(default)]
    pub ssl: bool,
    /// Whether to offer HTTP/2
    #[serde(default)]
    pub offer_h2: bool,
}

//
// Conversion implementations
//

impl From<Yaml> for super::internal::Config {
    fn from(yaml: Yaml) -> Self {
        let mut config = super::internal::Config::default();
        
        // Apply system configuration
        config.threads_per_service = yaml.system.threads_per_service;
        config.daemonize = yaml.system.daemonize;
        config.pid_file = yaml.system.pid_file;
        config.upgrade_socket = yaml.system.upgrade_socket;
        config.logging = yaml.system.logging.into();
        config.acme = yaml.acme;
        
        // Convert basic proxies
        config.basic_proxies = yaml.basic_proxy.into_iter().map(Into::into).collect();
        
        // Convert vhost proxies
        config.vhost_proxies = yaml.vhost_proxy.into_iter().map(Into::into).collect();
        
        // Convert file servers
        config.file_servers = yaml.file_server.into_iter().map(Into::into).collect();
        
        config
    }
}

impl From<LoggingConfig> for crate::logging::LoggingConfig {
    fn from(yaml_logging: LoggingConfig) -> Self {
        use tracing::Level;
        
        let level = match yaml_logging.level.to_uppercase().as_str() {
            "TRACE" => Level::TRACE,
            "DEBUG" => Level::DEBUG,
            "INFO" => Level::INFO,
            "WARN" => Level::WARN,
            "ERROR" => Level::ERROR,
            _ => Level::INFO, // Default fallback
        };
        
        crate::logging::LoggingConfig {
            access_log: yaml_logging.access_log.map(PathBuf::from),
            error_log: yaml_logging.error_log.map(PathBuf::from),
            level,
            enabled: yaml_logging.enabled,
        }
    }
}

impl From<ProxyConfig> for super::internal::ProxyConfig {
    fn from(yaml_proxy: ProxyConfig) -> Self {
        let upstream_options = yaml_proxy.load_balance
            .map(|lb| lb.into())
            .unwrap_or_default();
            
        Self {
            name: yaml_proxy.name,
            listeners: yaml_proxy.listeners.into_iter().map(Into::into).collect(),
            upstreams: yaml_proxy.connectors.into_iter().map(Into::into).collect(),
            path_control: yaml_proxy.path_control.into(),
            upstream_options,
            rate_limiting: yaml_proxy.rate_limiting.into(),
            cache_enabled: yaml_proxy.cache.cache_enabled,
            cache_ttl: yaml_proxy.cache.cache_ttl,
            browser_cache_enabled: yaml_proxy.cache.browser_cache_enabled,
            browser_cache_ttl: yaml_proxy.cache.browser_cache_ttl,
            cache_dir: yaml_proxy.cache.cache_dir,
            max_file_size: Some(yaml_proxy.cache.max_file_size),
            shard_count: Some(yaml_proxy.cache.shard_count as usize),
        }
    }
}

/// Validate domain name format (including wildcard patterns)
fn validate_domain_name(domain_name: &str) -> Result<(), String> {
    if domain_name.is_empty() {
        return Err("Domain name cannot be empty".to_string());
    }

    if domain_name.contains('*') {
        // Validate wildcard pattern
        if !is_valid_wildcard_pattern(domain_name) {
            return Err(format!(
                "Invalid wildcard domain pattern '{}'. Wildcards must be complete labels, cannot be in TLD position, and pattern must have at least one non-wildcard part.",
                domain_name
            ));
        }
    } else {
        // Validate regular domain name
        if !is_valid_domain_name(domain_name) {
            return Err(format!(
                "Invalid domain name '{}'. Domain names must follow standard DNS naming conventions.",
                domain_name
            ));
        }
    }

    Ok(())
}

/// Validate wildcard pattern format (same logic as in VHostRouter)
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
        if *part != "*" && !is_valid_domain_label(part) {
            return false;
        }
    }

    true
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
    labels.iter().all(|label| is_valid_domain_label(label))
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

impl From<VHostProxyConfig> for super::internal::VHostConfig {
    fn from(yaml_vhost: VHostProxyConfig) -> Self {
        use std::collections::HashMap;
        
        let mut domains = HashMap::new();
        for (domain_name, domain_config) in yaml_vhost.domains {
            // Validate domain name
            if let Err(validation_error) = validate_domain_name(&domain_name) {
                tracing::warn!("Invalid domain name '{}': {}", domain_name, validation_error);
                continue; // Skip invalid domains
            }
            
            domains.insert(domain_name, domain_config.into());
        }
        
        Self {
            name: yaml_vhost.name,
            listeners: yaml_vhost.listeners.into_iter().map(Into::into).collect(),
            domains,
            default_domain: yaml_vhost.default_domain.into(),
            path_control: yaml_vhost.path_control.into(),
            rate_limiting: yaml_vhost.rate_limiting.into(),
            cache_enabled: yaml_vhost.cache.cache_enabled,
            cache_ttl: yaml_vhost.cache.cache_ttl,
            browser_cache_enabled: yaml_vhost.cache.browser_cache_enabled,
            browser_cache_ttl: yaml_vhost.cache.browser_cache_ttl,
            cache_dir: yaml_vhost.cache.cache_dir,
            max_file_size: Some(yaml_vhost.cache.max_file_size),
            shard_count: Some(yaml_vhost.cache.shard_count as usize),
        }
    }
}

impl From<DomainConfig> for super::internal::DomainConfig {
    fn from(yaml_domain: DomainConfig) -> Self {
        let upstream_options = yaml_domain.load_balance
            .map(|lb| lb.into())
            .unwrap_or_default();
            
        Self {
            upstreams: yaml_domain.upstreams.into_iter().map(Into::into).collect(),
            ssl_config: yaml_domain.ssl_config.map(Into::into),
            upstream_options,
        }
    }
}

impl From<FileServerConfig> for super::internal::FileServerConfig {
    fn from(yaml_file_server: FileServerConfig) -> Self {
        Self {
            name: yaml_file_server.name,
            listeners: yaml_file_server.listeners.into_iter().map(Into::into).collect(),
            base_path: yaml_file_server.base_path,
        }
    }
}

impl From<PathControl> for super::internal::PathControl {
    fn from(yaml_path_control: PathControl) -> Self {
        Self {
            request_filters: yaml_path_control.request_filters,
            upstream_request_filters: yaml_path_control.upstream_request_filters,
            upstream_response_filters: yaml_path_control.upstream_response_filters,
        }
    }
}

impl From<TlsConfig> for super::internal::TlsConfig {
    fn from(yaml_tls: TlsConfig) -> Self {
        Self {
            cert_path: yaml_tls.cert_path,
            key_path: yaml_tls.key_path,
            auto_ssl: yaml_tls.auto_ssl,
        }
    }
}

impl From<RateLimiting> for super::internal::RateLimitingConfig {
    fn from(yaml_rate_limiting: RateLimiting) -> Self {
        use crate::proxy::rate_limiting::{
            multi::{MultiRaterConfig, MultiRequestKeyKind},
            single::{SingleInstanceConfig, SingleRequestKeyKind},
            AllRateConfig, RegexShim,
        };
        
        let mut rules = vec![];
        
        for rule in yaml_rate_limiting.rules {
            let rate_config = match rule.kind.as_str() {
                "source-ip" => {
                    if let Some(max_buckets) = rule.max_buckets {
                        AllRateConfig::Multi {
                            kind: MultiRequestKeyKind::SourceIp,
                            config: MultiRaterConfig {
                                threads: 8, // Default threads, should be configurable
                                max_buckets: max_buckets as usize,
                                max_tokens_per_bucket: rule.tokens_per_bucket as usize,
                                refill_interval_millis: rule.refill_rate_ms as usize,
                                refill_qty: rule.refill_qty as usize,
                            },
                        }
                    } else {
                        continue; // Skip invalid rules
                    }
                }
                "specific-uri" => {
                    if let (Some(pattern), Some(max_buckets)) = (&rule.pattern, rule.max_buckets) {
                        if let Ok(regex_pattern) = RegexShim::new(pattern) {
                            AllRateConfig::Multi {
                                kind: MultiRequestKeyKind::Uri {
                                    pattern: regex_pattern,
                                },
                                config: MultiRaterConfig {
                                    threads: 8, // Default threads, should be configurable
                                    max_buckets: max_buckets as usize,
                                    max_tokens_per_bucket: rule.tokens_per_bucket as usize,
                                    refill_interval_millis: rule.refill_rate_ms as usize,
                                    refill_qty: rule.refill_qty as usize,
                                },
                            }
                        } else {
                            continue; // Skip invalid regex patterns
                        }
                    } else {
                        continue; // Skip invalid rules
                    }
                }
                "any-matching-uri" => {
                    if let Some(pattern) = &rule.pattern {
                        if let Ok(regex_pattern) = RegexShim::new(pattern) {
                            AllRateConfig::Single {
                                kind: SingleRequestKeyKind::UriGroup {
                                    pattern: regex_pattern,
                                },
                                config: SingleInstanceConfig {
                                    max_tokens_per_bucket: rule.tokens_per_bucket as usize,
                                    refill_interval_millis: rule.refill_rate_ms as usize,
                                    refill_qty: rule.refill_qty as usize,
                                },
                            }
                        } else {
                            continue; // Skip invalid regex patterns
                        }
                    } else {
                        continue; // Skip invalid rules
                    }
                }
                _ => continue, // Skip unknown rule types
            };
            
            rules.push(rate_config);
        }
        
        Self { rules }
    }
}

impl From<LoadBalanceConfig> for UpstreamOptions {
    fn from(yaml_lb: LoadBalanceConfig) -> Self {
        use super::internal::{SelectionKind, HealthCheckKind, DiscoveryKind};
        use crate::proxy::request_selector::{
            null_selector, uri_path_selector, source_addr_and_uri_path_selector,
        };
        
        let selection = match yaml_lb.selection.as_str() {
            "RoundRobin" => SelectionKind::RoundRobin,
            "Random" => SelectionKind::Random,
            "FNV" | "Fnv" => SelectionKind::Fnv,
            "Ketama" => SelectionKind::Ketama,
            _ => SelectionKind::RoundRobin, // Default fallback
        };
        
        let selector = match (selection.clone(), yaml_lb.key.as_deref()) {
            (SelectionKind::Fnv | SelectionKind::Ketama, Some("UriPath")) => uri_path_selector,
            (SelectionKind::Fnv | SelectionKind::Ketama, Some("SourceAddrAndUriPath")) => source_addr_and_uri_path_selector,
            _ => null_selector,
        };
        
        let health_checks = match yaml_lb.health_check.as_str() {
            "None" => HealthCheckKind::None,
            _ => HealthCheckKind::None, // Default fallback
        };
        
        let discovery = match yaml_lb.discovery.as_str() {
            "Static" => DiscoveryKind::Static,
            _ => DiscoveryKind::Static, // Default fallback
        };
        
        Self {
            selection,
            selector,
            health_checks,
            discovery,
        }
    }
}

impl From<ListenerConfig> for super::internal::ListenerConfig {
    fn from(yaml_listener: ListenerConfig) -> Self {
        // Determine SSL mode: either from explicit TLS config or SSL flag
        let ssl_enabled = yaml_listener.tls.is_some() || yaml_listener.ssl;
        
        // Validate HTTP/2 requires SSL
        if yaml_listener.offer_h2 && !ssl_enabled {
            tracing::warn!("HTTP/2 requires SSL, but no SSL configuration provided for listener {}", yaml_listener.addr);
        }
        
        Self {
            source: super::internal::ListenerKind::Tcp {
                addr: yaml_listener.addr,
                tls: yaml_listener.tls.map(Into::into),
                ssl: ssl_enabled,
                offer_h2: yaml_listener.offer_h2,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn test_yaml_deserialization() {
        let yaml_content = r#"
system:
  threads-per-service: 8
  daemonize: false
  logging:
    access-log: "stryvo-access.log"
    error-log: "stryvo-error.log"
    level: "INFO"
    enabled: true

basic-proxy:
  - name: "Example1"
    listeners:
      - addr: "0.0.0.0:8080"
        offer-h2: false
      - addr: "0.0.0.0:4443"
        tls:
          cert_path: "./assets/test.crt"
          key_path: "./assets/test.key"
        offer-h2: true
    connectors:
      - proxy-addr: "91.107.223.4:443"
        tls-sni: "onevariable.com"
        proto: "h2-or-h1"
    cache:
      cache-enabled: true
      cache-ttl: 10
      shard-count: 32
      cache-dir: "/tmp/test/stryvo-cache"
      max-file-size: 1048576
      browser-cache-enabled: false
      browser-cache-ttl: 3600
    path-control:
      upstream-request-filters:
        - kind: "remove-header-key-regex"
          pattern: ".*(secret|SECRET).*"
        - kind: "upsert-header"
          key: "x-proxy-friend"
          value: "stryvo"
      upstream-response-filters:
        - kind: "remove-header-key-regex"
          pattern: ".*ETag.*"
        - kind: "upsert-header"
          key: "x-with-love-from"
          value: "stryvo"

file-server:
  - name: "Example3"
    listeners:
      - addr: "0.0.0.0:9000"
        offer-h2: false
      - addr: "0.0.0.0:9443"
        tls:
          cert_path: "./assets/test.crt"
          key_path: "./assets/test.key"
        offer-h2: true
    base-path: "."
"#;

        let parsed: Yaml = serde_yaml::from_str(yaml_content).expect("Failed to parse YAML");
        
        assert_eq!(parsed.system.threads_per_service, 8);
        assert!(!parsed.system.daemonize);
        assert_eq!(parsed.basic_proxy.len(), 1);
        assert_eq!(parsed.file_server.len(), 1);
        
        let proxy = &parsed.basic_proxy[0];
        assert_eq!(proxy.name, "Example1");
        assert_eq!(proxy.listeners.len(), 2);
        assert_eq!(proxy.connectors.len(), 1);
        
        let file_server = &parsed.file_server[0];
        assert_eq!(file_server.name, "Example3");
        assert_eq!(file_server.listeners.len(), 2);
    }
}