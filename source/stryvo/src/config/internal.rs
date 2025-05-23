use std::{collections::BTreeMap, path::PathBuf};

use pingora::{
    server::configuration::{Opt as PingoraOpt, ServerConf as PingoraServerConf},
    upstreams::peer::HttpPeer,
};
use tracing::warn;

use crate::proxy::{
    rate_limiting::AllRateConfig,
    request_selector::{null_selector, RequestSelector},
};

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
    pub file_servers: Vec<FileServerConfig>,
    pub logging: crate::logging::LoggingConfig,
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

#[derive(Debug, PartialEq, Clone)]
pub struct TlsConfig {
    pub(crate) cert_path: PathBuf,
    pub(crate) key_path: PathBuf,
}

#[derive(Debug, PartialEq, Clone)]
pub struct ListenerConfig {
    pub(crate) source: ListenerKind,
}

#[derive(Debug, PartialEq, Clone)]
pub enum ListenerKind {
    Tcp {
        addr: String,
        tls: Option<TlsConfig>,
        offer_h2: bool,
    },
    Uds(PathBuf),
}

#[derive(Debug, PartialEq, Clone)]
pub struct UpstreamOptions {
    pub(crate) selection: SelectionKind,
    pub(crate) selector: RequestSelector,
    pub(crate) health_checks: HealthCheckKind,
    pub(crate) discovery: DiscoveryKind,
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
            file_servers: vec![],
            daemonize: false,
            pid_file: None,
            upgrade: false,
            upgrade_socket: None,
            logging: crate::logging::LoggingConfig::default(),
        }
    }
}