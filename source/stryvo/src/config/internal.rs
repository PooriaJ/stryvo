use std::{collections::{BTreeMap, HashMap}, path::PathBuf};

use pingora::{
    server::configuration::{Opt as PingoraOpt, ServerConf as PingoraServerConf},
    upstreams::peer::HttpPeer,
};
use tracing::warn;

use crate::proxy::{
    rate_limiting::AllRateConfig,
    request_selector::{null_selector, RequestSelector},
};
use crate::acme::config::AcmeConfig;

/// stryvo's internal configuration
#[derive(Debug, Clone)]
pub struct Config {
    pub validate_configs: bool,
    pub threads_per_service: usize,
    pub daemonize: bool,
    pub pid_file: Option<PathBuf>,
    pub upgrade_socket: Option<PathBuf>,
    pub upgrade: bool,
    pub basic_proxies: Vec<ProxyConfig>,
    pub vhost_proxies: Vec<VHostConfig>,
    pub file_servers: Vec<FileServerConfig>,
    pub logging: crate::logging::LoggingConfig,
    pub acme: AcmeConfig,
}

impl Config {
    /// Get the [`Opt`][PingoraOpt] field for Pingora
    pub fn pingora_opt(&self) -> PingoraOpt {
        // TODO
        PingoraOpt {
            upgrade: self.upgrade,
            daemon: self.daemonize,
            nocapture: false,
            test: self.validate_configs,
            conf: None,
        }
    }

    /// Get the [`ServerConf`][PingoraServerConf] field for Pingora
    pub fn pingora_server_conf(&self) -> PingoraServerConf {
        PingoraServerConf {
            daemon: self.daemonize,
            error_log: None,
            // TODO: These are bad assumptions - non-developers will not have "target"
            // files, and we shouldn't necessarily use utf-8 strings with fixed separators
            // here.
            pid_file: self
                .pid_file
                .as_ref()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("/tmp/stryvo.pidfile"))
                .to_string_lossy()
                .into(),
            upgrade_sock: self
                .upgrade_socket
                .as_ref()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("/tmp/stryvo-upgrade.sock"))
                .to_string_lossy()
                .into(),
            user: None,
            group: None,
            threads: self.threads_per_service,
            work_stealing: true,
            ca_file: None,
            ..PingoraServerConf::default()
        }
    }

    pub fn validate(&self) {
        // This is currently mostly ad-hoc checks, we should potentially be a bit
        // more systematic about this.
        
        // Validate vhost proxy configurations
        for vhost_config in &self.vhost_proxies {
            if let Err(e) = vhost_config.validate() {
                panic!("VHost configuration validation failed: {}", e);
            }
        }
        
        if self.daemonize {
            if let Some(pf) = self.pid_file.as_ref() {
                // NOTE: currently due to https://github.com/cloudflare/pingora/issues/331,
                // we are not able to use relative paths.
                assert!(pf.is_absolute(), "pid file path must be absolute, see https://github.com/cloudflare/pingora/issues/331");
            } else {
                panic!("Daemonize commanded but no pid file set!");
            }
        } else if let Some(pf) = self.pid_file.as_ref() {
            if !pf.is_absolute() {
                warn!("pid file path must be absolute. Currently: {:?}, see https://github.com/cloudflare/pingora/issues/331", pf);
            }
        }
        if self.upgrade {
            assert!(
                cfg!(target_os = "linux"),
                "Upgrade is only supported on linux!"
            );
            if let Some(us) = self.upgrade_socket.as_ref() {
                // NOTE: currently due to https://github.com/cloudflare/pingora/issues/331,
                // we are not able to use relative paths.
                assert!(us.is_absolute(), "upgrade socket path must be absolute, see https://github.com/cloudflare/pingora/issues/331");
            } else {
                panic!("Upgrade commanded but upgrade socket path not set!");
            }
        } else if let Some(us) = self.upgrade_socket.as_ref() {
            if !us.is_absolute() {
                warn!("upgrade socket path must be absolute. Currently: {:?}, see https://github.com/cloudflare/pingora/issues/331", us);
            }
        }
    }
}

impl VHostConfig {
    /// Validate the virtual host configuration
    pub fn validate(&self) -> Result<(), String> {
        // Validate service name is not empty
        if self.name.is_empty() {
            return Err("VHost service name cannot be empty".to_string());
        }

        // Validate at least one listener is configured
        if self.listeners.is_empty() {
            return Err(format!("VHost service '{}' must have at least one listener", self.name));
        }

        // Validate domain configurations
        for (domain_name, domain_config) in &self.domains {
            if let Err(e) = self.validate_domain(domain_name, domain_config) {
                return Err(format!("Invalid domain '{}' in service '{}': {}", domain_name, self.name, e));
            }
        }

        // Validate default domain configuration
        if let Err(e) = self.validate_domain("default", &self.default_domain) {
            return Err(format!("Invalid default domain in service '{}': {}", self.name, e));
        }

        // Validate cache configuration
        if self.cache_enabled && self.cache_ttl == 0 {
            return Err(format!("VHost service '{}' has caching enabled but cache_ttl is 0", self.name));
        }

        if self.browser_cache_enabled && self.browser_cache_ttl == 0 {
            return Err(format!("VHost service '{}' has browser caching enabled but browser_cache_ttl is 0", self.name));
        }

        Ok(())
    }

    /// Validate a single domain configuration
    fn validate_domain(&self, domain_name: &str, domain_config: &DomainConfig) -> Result<(), String> {
        // Validate domain name format (basic validation)
        if domain_name != "default" && domain_name.is_empty() {
            return Err("Domain name cannot be empty".to_string());
        }

        // Validate at least one upstream is configured
        if domain_config.upstreams.is_empty() {
            return Err("Domain must have at least one upstream server".to_string());
        }

        // Validate SSL configuration if present
        if let Some(ssl_config) = &domain_config.ssl_config {
            if ssl_config.auto_ssl {
                // For auto SSL, we don't need to validate cert/key paths as they will be managed automatically
                tracing::debug!("Auto SSL enabled for domain: {}", domain_name);
            } else {
                // For manual SSL, validate cert/key paths
                if ssl_config.cert_path.as_os_str().is_empty() {
                    return Err("SSL certificate path cannot be empty when auto_ssl is disabled".to_string());
                }
                if ssl_config.key_path.as_os_str().is_empty() {
                    return Err("SSL key path cannot be empty when auto_ssl is disabled".to_string());
                }
                
                // Check if certificate files exist (basic validation)
                if !ssl_config.cert_path.exists() {
                    return Err(format!("SSL certificate file does not exist: {:?}", ssl_config.cert_path));
                }
                if !ssl_config.key_path.exists() {
                    return Err(format!("SSL key file does not exist: {:?}", ssl_config.key_path));
                }
            }
        }

        Ok(())
    }
}

///
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RateLimitingConfig {
    pub(crate) rules: Vec<AllRateConfig>,
}

/// Add Path Control Modifiers
///
/// Note that we use `BTreeMap` and NOT `HashMap`, as we want to maintain the
/// ordering from the configuration file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PathControl {
    pub(crate) request_filters: Vec<BTreeMap<String, String>>,
    pub(crate) upstream_request_filters: Vec<BTreeMap<String, String>>,
    pub(crate) upstream_response_filters: Vec<BTreeMap<String, String>>,
}

//
// File Server Configuration
//
#[derive(Debug, Clone)]
pub struct FileServerConfig {
    pub(crate) name: String,
    pub(crate) listeners: Vec<ListenerConfig>,
    pub(crate) base_path: Option<PathBuf>,
}

//
// Basic Proxy Configuration
//

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub(crate) name: String,
    pub(crate) listeners: Vec<ListenerConfig>,
    pub(crate) upstream_options: UpstreamOptions,
    pub(crate) upstreams: Vec<HttpPeer>,
    pub(crate) path_control: PathControl,
    pub(crate) rate_limiting: RateLimitingConfig,
    /// Whether caching is enabled for this proxy service
    pub(crate) cache_enabled: bool,
    /// Time-to-live for cached responses in seconds
    pub(crate) cache_ttl: u64,
    /// Whether browser caching is enabled (via Cache-Control headers)
    pub(crate) browser_cache_enabled: bool,
    /// Time-to-live for browser cache in seconds (max-age value)
    pub(crate) browser_cache_ttl: u64,
    /// Directory for cache storage
    pub(crate) cache_dir: Option<PathBuf>,
    /// Maximum file size for cached objects
    pub(crate) max_file_size: Option<usize>,
    /// Number of cache shards
    pub(crate) shard_count: Option<usize>,
}

//
// Virtual Host Configuration
//

/// Configuration for virtual host proxy service
#[derive(Debug, Clone)]
pub struct VHostConfig {
    pub name: String,
    pub listeners: Vec<ListenerConfig>,
    pub domains: HashMap<String, DomainConfig>,
    pub default_domain: DomainConfig,
    #[allow(dead_code)]
    pub path_control: PathControl,
    #[allow(dead_code)]
    pub rate_limiting: RateLimitingConfig,
    /// Whether caching is enabled for this vhost proxy service
    pub cache_enabled: bool,
    /// Time-to-live for cached responses in seconds
    pub cache_ttl: u64,
    /// Whether browser caching is enabled (via Cache-Control headers)
    pub browser_cache_enabled: bool,
    /// Time-to-live for browser cache in seconds (max-age value)
    pub browser_cache_ttl: u64,
    /// Directory for cache storage
    pub cache_dir: Option<PathBuf>,
    /// Maximum file size for cached objects
    pub max_file_size: Option<usize>,
    /// Number of cache shards
    pub shard_count: Option<usize>,
}

/// Configuration for a single domain in a virtual host setup
#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub upstreams: Vec<HttpPeer>,
    pub ssl_config: Option<TlsConfig>,
    #[allow(dead_code)]
    pub upstream_options: UpstreamOptions,
}

#[derive(Debug, PartialEq, Clone)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// Enable automatic SSL certificate management via ACME
    pub auto_ssl: bool,
}

#[derive(Debug, PartialEq, Clone)]
pub struct ListenerConfig {
    pub source: ListenerKind,
}

#[derive(Debug, PartialEq, Clone)]
pub enum ListenerKind {
    Tcp {
        addr: String,
        tls: Option<TlsConfig>,
        ssl: bool,  // New SSL flag for SNI-based certificate selection
        offer_h2: bool,
    },
    Uds(PathBuf),
}

#[derive(Debug, Clone)]
pub struct UpstreamOptions {
    pub(crate) selection: SelectionKind,
    pub(crate) selector: RequestSelector,
    pub(crate) health_checks: HealthCheckKind,
    pub(crate) discovery: DiscoveryKind,
}

impl PartialEq for UpstreamOptions {
    fn eq(&self, other: &Self) -> bool {
        self.selection == other.selection
            && self.health_checks == other.health_checks
            && self.discovery == other.discovery
        // Note: We intentionally skip comparing selector because it's a function pointer
        // and function pointer comparisons are unreliable in Rust
    }
}

impl Default for UpstreamOptions {
    fn default() -> Self {
        Self {
            selection: SelectionKind::RoundRobin,
            selector: null_selector,
            health_checks: HealthCheckKind::None,
            discovery: DiscoveryKind::Static,
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum SelectionKind {
    RoundRobin,
    Random,
    Fnv,
    Ketama,
}

#[derive(Debug, PartialEq, Clone)]
pub enum HealthCheckKind {
    None,
}

#[derive(Debug, PartialEq, Clone)]
pub enum DiscoveryKind {
    Static,
}

//
// Boilerplate trait impls
//

impl Default for Config {
    fn default() -> Self {
        Self {
            validate_configs: false,
            threads_per_service: 8,
            basic_proxies: vec![],
            vhost_proxies: vec![],
            file_servers: vec![],
            daemonize: false,
            pid_file: None,
            upgrade: false,
            upgrade_socket: None,
            logging: crate::logging::LoggingConfig::default(),
            acme: AcmeConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_vhost_config_validation_empty_name() {
        let config = VHostConfig {
            name: String::new(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "0.0.0.0:8080".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains: HashMap::new(),
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("127.0.0.1:8080", false, String::new())],
                ssl_config: None,
                upstream_options: UpstreamOptions::default(),
            },
            path_control: PathControl::default(),
            rate_limiting: RateLimitingConfig::default(),
            cache_enabled: false,
            cache_ttl: 300,
            browser_cache_enabled: false,
            browser_cache_ttl: 3600,
            cache_dir: None,
            max_file_size: None,
            shard_count: None,
        };

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("VHost service name cannot be empty"));
    }

    #[test]
    fn test_vhost_config_validation_no_listeners() {
        let config = VHostConfig {
            name: "TestService".to_string(),
            listeners: vec![],
            domains: HashMap::new(),
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("127.0.0.1:8080", false, String::new())],
                ssl_config: None,
                upstream_options: UpstreamOptions::default(),
            },
            path_control: PathControl::default(),
            rate_limiting: RateLimitingConfig::default(),
            cache_enabled: false,
            cache_ttl: 300,
            browser_cache_enabled: false,
            browser_cache_ttl: 3600,
            cache_dir: None,
            max_file_size: None,
            shard_count: None,
        };

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must have at least one listener"));
    }

    #[test]
    fn test_domain_config_validation_no_upstreams() {
        let config = VHostConfig {
            name: "TestService".to_string(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "0.0.0.0:8080".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains: HashMap::new(),
            default_domain: DomainConfig {
                upstreams: vec![], // Empty upstreams should fail validation
                ssl_config: None,
                upstream_options: UpstreamOptions::default(),
            },
            path_control: PathControl::default(),
            rate_limiting: RateLimitingConfig::default(),
            cache_enabled: false,
            cache_ttl: 300,
            browser_cache_enabled: false,
            browser_cache_ttl: 3600,
            cache_dir: None,
            max_file_size: None,
            shard_count: None,
        };

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must have at least one upstream server"));
    }

    #[test]
    fn test_vhost_config_validation_success() {
        let config = VHostConfig {
            name: "TestService".to_string(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "0.0.0.0:8080".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains: HashMap::new(),
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("127.0.0.1:8080", false, String::new())],
                ssl_config: None,
                upstream_options: UpstreamOptions::default(),
            },
            path_control: PathControl::default(),
            rate_limiting: RateLimitingConfig::default(),
            cache_enabled: false,
            cache_ttl: 300,
            browser_cache_enabled: false,
            browser_cache_ttl: 3600,
            cache_dir: None,
            max_file_size: None,
            shard_count: None,
        };

        let result = config.validate();
        assert!(result.is_ok());
    }
}