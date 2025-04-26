//! Configuration sourced from an Nginx-like file format
//!
//! This module implements a parser for a simplified Nginx-like configuration format
//! that is more familiar to users of Nginx while maintaining compatibility with
//! stryvo's internal configuration model.

use std::collections::{BTreeMap, HashMap};
use std::fs::read_to_string;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use miette::{Diagnostic, SourceSpan};
use pingora::protocols::ALPN;
use pingora::upstreams::peer::HttpPeer;
use thiserror::Error;

use super::internal::{
    Config, DiscoveryKind, FileServerConfig, HealthCheckKind, ListenerConfig, ListenerKind,
    PathControl, ProxyConfig, RateLimitingConfig, SelectionKind, TlsConfig, UpstreamOptions,
};
use crate::proxy::rate_limiting::{
    multi::{MultiRaterConfig, MultiRequestKeyKind},
    single::{SingleInstanceConfig, SingleRequestKeyKind},
    AllRateConfig, RegexShim,
};
use crate::proxy::request_selector::{source_addr_and_uri_path_selector, uri_path_selector};

#[derive(Debug, Error, Diagnostic)]
#[error("Nginx configuration error at line {line}: {message}")]
pub struct NginxParseError {
    message: String,
    line: usize,
    #[source_code]
    source_code: String,
    #[label("here")]
    span: SourceSpan,
}

/// Represents an Nginx-like configuration
pub struct Nginx {
    config: Config,
}

impl Nginx {
    /// Load configuration from a file path
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, NginxParseError> {
        let content = read_to_string(&path).map_err(|e| NginxParseError {
            message: format!("IO error: {}", e),
            line: 0,
            source_code: String::new(),
            span: (0, 0).into(),
        })?;
        Self::from_string(&content)
    }

    /// Parse configuration from a string
    pub fn from_string(content: &str) -> Result<Self, NginxParseError> {
        let mut parser = NginxParser::new(content);
        let config = parser.parse()?;
        Ok(Self { config })
    }
}

impl From<Nginx> for Config {
    fn from(nginx: Nginx) -> Self {
        nginx.config
    }
}

/// Parser for Nginx-like configuration syntax
struct NginxParser<'a> {
    content: &'a str,
    lines: Vec<(usize, &'a str)>, // (line_number, line_content)
    current_line: usize,
}

impl<'a> NginxParser<'a> {
    fn new(content: &'a str) -> Self {
        let lines: Vec<(usize, &'a str)> = content
            .lines()
            .enumerate()
            .map(|(i, line)| (i + 1, line))
            .filter(|(_, line)| !line.trim().is_empty() && !line.trim().starts_with('#'))
            .collect();

        Self {
            content,
            lines,
            current_line: 0,
        }
    }

    /// Returns the next non-empty line after stripping inline comments.
    fn next_clean_line(&mut self) -> Option<(usize, &'a str)> {
        while let Some((line_num, raw_line)) = self.next_line() {
            let cleaned = raw_line.split('#').next().unwrap().trim();
            if cleaned.is_empty() {
                continue;
            } else {
                return Some((line_num, cleaned));
            }
        }
        None
    }

    fn next_line(&mut self) -> Option<(usize, &'a str)> {
        if self.current_line < self.lines.len() {
            let line = self.lines[self.current_line];
            self.current_line += 1;
            Some(line)
        } else {
            None
        }
    }

    fn parse(&mut self) -> Result<Config, NginxParseError> {
        let mut config = Config::default();
        let mut basic_proxies = Vec::new();
        let mut file_servers = Vec::new();
        let mut upstreams_map = HashMap::new();

        while let Some((line_num, line)) = self.next_clean_line() {
            let line = line.trim_end_matches(';');
            if line.starts_with("system") {
                self.expect_open_brace(line, line_num)?;
                self.parse_system_block(&mut config)?;
            } else if line.starts_with("http") {
                self.expect_open_brace(line, line_num)?;
                self.parse_http_block(&mut basic_proxies, &mut file_servers, &mut upstreams_map)?;
            } else if line.starts_with("upstream") {
                let name = self.parse_upstream_name(line, line_num)?;
                self.expect_open_brace(line, line_num)?;
                let (upstreams, options) = self.parse_upstream_block(line_num)?;
                upstreams_map.insert(name.to_string(), (upstreams, options));
            }
        }

        config.basic_proxies = basic_proxies;
        config.file_servers = file_servers;
        Ok(config)
    }

    fn expect_open_brace(&self, line: &str, line_num: usize) -> Result<(), NginxParseError> {
        if !line.ends_with("{") {
            Err(self.error("Expected '{'", line_num, line.len().saturating_sub(1), 1))
        } else {
            Ok(())
        }
    }

    fn error(&self, message: &str, line_num: usize, offset: usize, len: usize) -> NginxParseError {
        NginxParseError {
            message: message.to_string(),
            line: line_num,
            source_code: self.content.to_string(),
            span: (self.line_offset(line_num) + offset, len).into(),
        }
    }

    fn line_offset(&self, line_num: usize) -> usize {
        self.content
            .lines()
            .take(line_num - 1)
            .map(|l| l.len() + 1)
            .sum()
    }

    fn parse_system_block(&mut self, config: &mut Config) -> Result<(), NginxParseError> {
        while let Some((line_num, line)) = self.next_clean_line() {
            let line = line.trim_end_matches(';');
            if line == "}" {
                break;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts[0] {
                "threads_per_service" => {
                    config.threads_per_service =
                        self.parse_value(&parts, 1, line_num, "threads_per_service")?;
                }
                "daemonize" => {
                    config.daemonize = self.parse_value(&parts, 1, line_num, "daemonize")?;
                }
                "pid_file" => {
                    let path = self.parse_quoted_value(&parts, 1, line_num, "pid_file")?;
                    config.pid_file = Some(PathBuf::from(path));
                }
                "upgrade_socket" => {
                    let path = self.parse_quoted_value(&parts, 1, line_num, "upgrade_socket")?;
                    config.upgrade_socket = Some(PathBuf::from(path));
                }
                _ => return Err(self.error("Unknown system directive", line_num, 0, line.len())),
            }
        }
        Ok(())
    }

    fn parse_http_block(
        &mut self,
        basic_proxies: &mut Vec<ProxyConfig>,
        file_servers: &mut Vec<FileServerConfig>,
        upstreams_map: &mut HashMap<String, (Vec<HttpPeer>, UpstreamOptions)>,
    ) -> Result<(), NginxParseError> {
        while let Some((line_num, line)) = self.next_clean_line() {
            let line = line.trim_end_matches(';');
            if line == "}" {
                break;
            }
            if line.starts_with("server") {
                self.expect_open_brace(line, line_num)?;
                self.parse_server_block(basic_proxies, file_servers, upstreams_map)?;
            }
        }
        Ok(())
    }

    fn parse_server_block(
        &mut self,
        basic_proxies: &mut Vec<ProxyConfig>,
        file_servers: &mut Vec<FileServerConfig>,
        upstreams_map: &HashMap<String, (Vec<HttpPeer>, UpstreamOptions)>,
    ) -> Result<(), NginxParseError> {
        let mut name = format!("Server{}", basic_proxies.len() + file_servers.len());
        let mut listeners = Vec::new();
        let mut upstreams = Vec::new();
        let mut upstream_options = UpstreamOptions::default();
        let mut path_control = PathControl::default();
        let mut rate_limiting = RateLimitingConfig::default();
        let mut is_file_server = false;
        let mut base_path = None;
        // Optional server-level proxy_pass directive (if provided)
        let mut proxy_pass: Option<String> = None;
        // Cache settings
        let mut cache_enabled = false;
        let mut cache_ttl = 300;
        let mut browser_cache_enabled = false;
        let mut browser_cache_ttl = 3600;
        let mut cache_dir = None;
        let mut max_file_size = None;
        let mut shard_count = None;
    
        while let Some((line_num, line)) = self.next_clean_line() {
            let line = line.trim_end_matches(';');
            if line == "}" {
                break;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts[0] {
                "server_name" => {
                    name = self.parse_value(&parts, 1, line_num, "server_name")?;
                }
                "listen" => {
                    listeners.push(self.parse_listener(line, line_num)?);
                }
                "upstream" => {
                    let is_single_line = line.contains('{') && line.ends_with('}');
                    if is_single_line {
                        // Single-line upstream block: upstream backend { server <addr>; }
                        let inner = line[line.find('{').unwrap() + 1..line.rfind('}').unwrap()].trim();
                        let inner_parts: Vec<&str> = inner.split_whitespace().collect();
                        if inner_parts.len() < 2 || inner_parts[0] != "server" {
                            return Err(self.error(
                                "Invalid single-line upstream directive",
                                line_num,
                                0,
                                line.len(),
                            ));
                        }
                        let addr = inner_parts[1].trim_end_matches(';').to_string();
                        let peer = HttpPeer::new(addr, false, String::new());
                        upstreams.push(peer);
                    } else {
                        // Multi-line upstream block
                        self.expect_open_brace(line, line_num)?;
                        let (new_upstreams, new_options) = self.parse_upstream_block(line_num)?;
                        upstreams.extend(new_upstreams);
                        upstream_options = new_options;
                    }
                }
                "proxy_pass" => {
                    // Server-level proxy_pass directive.
                    let url: String = self.parse_value(&parts, 1, line_num, "proxy_pass")?;
                    if url.starts_with("http://") || url.starts_with("https://") {
                        proxy_pass = Some(url);
                    } else if let Some((ups, opts)) = upstreams_map.get(&url) {
                        upstreams.extend(ups.clone());
                        upstream_options = opts.clone();
                        proxy_pass = Some(url);
                    } else {
                        return Err(self.error(
                            "Unknown upstream reference",
                            line_num,
                            0,
                            line.len(),
                        ));
                    }
                }
                "location" => {
                    self.parse_location_block(
                        line,
                        line_num,
                        &mut path_control,
                        &mut rate_limiting,
                    )?;
                }
                "root" => {
                    let path = self.parse_quoted_value(&parts, 1, line_num, "root")?;
                    base_path = Some(PathBuf::from(path));
                    is_file_server = true;
                }
                "cache" => {
                    self.expect_open_brace(line, line_num)?;
                    self.parse_cache_block(
                        line_num,
                        &mut cache_enabled,
                        &mut cache_ttl,
                        &mut browser_cache_enabled,
                        &mut browser_cache_ttl,
                        &mut cache_dir,
                        &mut max_file_size,
                        &mut shard_count,
                    )?;
                }
                _ => return Err(self.error("Unknown server directive", line_num, 0, line.len())),
            }
        }
    
        // If no upstream provided at server level, try to "promote" a proxy_pass from location filters.
        if upstreams.is_empty() && proxy_pass.is_none() {
            for filter in &path_control.upstream_request_filters {
                if let Some(kind) = filter.get("kind") {
                    if kind == "proxy_pass" {
                        if let Some(val) = filter.get("value") {
                            if let Some((ups, opts)) = upstreams_map.get(val) {
                                upstreams.extend(ups.clone());
                                upstream_options = opts.clone();
                                proxy_pass = Some(val.clone());
                                break;
                            }
                        }
                    }
                }
            }
        }
        // Remove any proxy_pass filters from location-level filters so that the proxy module does not see them.
        path_control.upstream_request_filters.retain(|filter| {
            filter
                .get("kind")
                .map(|s| s != "proxy_pass")
                .unwrap_or(true)
        });
    
        if listeners.is_empty() {
            return Err(self.error(
                "Server must have at least one listener",
                self.current_line,
                0,
                0,
            ));
        }
    
        if is_file_server {
            file_servers.push(FileServerConfig {
                name,
                listeners,
                base_path,
            });
        } else {
            if upstreams.is_empty() && proxy_pass.is_none() {
                return Err(self.error(
                    "Proxy server must have at least one upstream",
                    self.current_line,
                    0,
                    0,
                ));
            }
            basic_proxies.push(ProxyConfig {
                name,
                listeners,
                upstream_options,
                upstreams,
                path_control,
                rate_limiting,
                cache_enabled,
                cache_ttl,
                browser_cache_enabled,
                browser_cache_ttl,
                cache_dir,
                max_file_size,
                shard_count,
            });
        }
        Ok(())
    }

    fn parse_cache_block(
        &mut self,
        _line_num: usize,
        cache_enabled: &mut bool,
        cache_ttl: &mut u64,
        browser_cache_enabled: &mut bool,
        browser_cache_ttl: &mut u64,
        cache_dir: &mut Option<PathBuf>,
        max_file_size: &mut Option<usize>,
        shard_count: &mut Option<usize>,
    ) -> Result<(), NginxParseError> {
        while let Some((sub_line_num, line)) = self.next_clean_line() {
            let line = line.trim_end_matches(';');
            if line == "}" {
                break;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                return Err(self.error("Empty directive in cache block", sub_line_num, 0, line.len()));
            }
            match parts[0] {
                "cache_enabled" => {
                    *cache_enabled = self.parse_value(&parts, 1, sub_line_num, "cache_enabled")?;
                }
                "cache_ttl" => {
                    let val: i64 = self.parse_value(&parts, 1, sub_line_num, "cache_ttl")?;
                    if val >= 0 {
                        *cache_ttl = val as u64;
                    } else {
                        return Err(self.error(
                            "cache_ttl must be non-negative",
                            sub_line_num,
                            0,
                            line.len(),
                        ));
                    }
                }
                "browser_cache_enabled" => {
                    *browser_cache_enabled =
                        self.parse_value(&parts, 1, sub_line_num, "browser_cache_enabled")?;
                }
                "browser_cache_ttl" => {
                    let val: i64 = self.parse_value(&parts, 1, sub_line_num, "browser_cache_ttl")?;
                    if val >= 0 {
                        *browser_cache_ttl = val as u64;
                    } else {
                        return Err(self.error(
                            "browser_cache_ttl must be non-negative",
                            sub_line_num,
                            0,
                            line.len(),
                        ));
                    }
                }
                "cache_dir" => {
                    let path = self.parse_quoted_value(&parts, 1, sub_line_num, "cache_dir")?;
                    *cache_dir = Some(PathBuf::from(path));
                }
                "max_file_size" => {
                    let val: i64 = self.parse_value(&parts, 1, sub_line_num, "max_file_size")?;
                    if val >= 0 {
                        *max_file_size = Some(val as usize);
                    } else {
                        return Err(self.error(
                            "max_file_size must be non-negative",
                            sub_line_num,
                            0,
                            line.len(),
                        ));
                    }
                }
                "shard_count" => {
                    let val: i64 = self.parse_value(&parts, 1, sub_line_num, "shard_count")?;
                    if val > 0 {
                        *shard_count = Some(val as usize);
                    } else {
                        return Err(self.error(
                            "shard_count must be positive",
                            sub_line_num,
                            0,
                            line.len(),
                        ));
                    }
                }
                _ => {
                    return Err(self.error(
                        "Unknown cache directive",
                        sub_line_num,
                        0,
                        line.len(),
                    ))
                }
            }
        }
        Ok(())
    }

    fn parse_listener(
        &self,
        line: &str,
        line_num: usize,
    ) -> Result<ListenerConfig, NginxParseError> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(self.error("Invalid listen directive", line_num, 0, line.len()));
        }
        let addr = parts[1].to_string();
        let mut tls = None;
        let mut offer_h2 = false;
        for i in 2..parts.len() {
            match parts[i] {
                "ssl" => {
                    let mut cert_path = None;
                    let mut key_path = None;
                    for j in i + 1..parts.len() {
                        if parts[j].starts_with("cert=") {
                            cert_path = Some(PathBuf::from(
                                parts[j].trim_start_matches("cert=").trim_matches('"'),
                            ));
                        } else if parts[j].starts_with("key=") {
                            key_path = Some(PathBuf::from(
                                parts[j].trim_start_matches("key=").trim_matches('"'),
                            ));
                        }
                    }
                    if let (Some(cert), Some(key)) = (cert_path, key_path) {
                        tls = Some(TlsConfig {
                            cert_path: cert,
                            key_path: key,
                        });
                    } else {
                        return Err(self.error(
                            "SSL requires both cert and key",
                            line_num,
                            0,
                            line.len(),
                        ));
                    }
                }
                "http2" => offer_h2 = true,
                _ => {}
            }
        }
        Ok(ListenerConfig {
            source: ListenerKind::Tcp {
                addr,
                tls,
                offer_h2,
            },
        })
    }

    fn parse_upstream_name(
        &self,
        line: &'a str,
        line_num: usize,
    ) -> Result<&'a str, NginxParseError> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(self.error(
                "Invalid upstream directive: missing name",
                line_num,
                0,
                line.len(),
            ));
        }
        Ok(parts[1].trim_end_matches('{'))
    }

    fn parse_upstream_block(
        &mut self,
        _line_num: usize,
    ) -> Result<(Vec<HttpPeer>, UpstreamOptions), NginxParseError> {
        let mut upstreams = Vec::new();
        let mut options = UpstreamOptions::default();
        while let Some((line_num, line)) = self.next_clean_line() {
            let line = line.trim_end_matches(';');
            if line == "}" {
                break;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                return Err(self.error(
                    "Empty directive in upstream block",
                    line_num,
                    0,
                    line.len(),
                ));
            }
            match parts[0] {
                "server" => {
                    if parts.len() < 2 {
                        return Err(self.error(
                            "Invalid server directive in upstream",
                            line_num,
                            0,
                            line.len(),
                        ));
                    }
                    let addr = parts[1].to_string();
                    let mut tls_sni = None;
                    let mut proto = "h1-only".to_string();
                    for i in 2..parts.len() {
                        if parts[i].starts_with("tls_sni=") {
                            tls_sni = Some(
                                parts[i]
                                    .trim_start_matches("tls_sni=")
                                    .trim_matches('"')
                                    .to_string(),
                            );
                        } else if parts[i].starts_with("proto=") {
                            proto = parts[i]
                                .trim_start_matches("proto=")
                                .trim_matches('"')
                                .to_string();
                        }
                    }
                    let (tls, sni) = if let Some(sni_value) = tls_sni {
                        (true, sni_value)
                    } else {
                        (false, String::new())
                    };
                    let mut peer = HttpPeer::new(addr, tls, sni);
                    if tls {
                        match proto.as_str() {
                            "h1-only" => peer.options.alpn = ALPN::H1,
                            "h2-only" => peer.options.alpn = ALPN::H2,
                            "h2-or-h1" => peer.options.alpn = ALPN::H2H1,
                            _ => {
                                return Err(self.error(
                                    "Invalid proto value",
                                    line_num,
                                    0,
                                    line.len(),
                                ))
                            }
                        }
                    }
                    upstreams.push(peer);
                }
                "hash" => {
                    options.selection = SelectionKind::Ketama;
                    options.selector = uri_path_selector;
                    if parts.len() > 1 && parts[1] == "consistent" {
                        options.discovery = DiscoveryKind::Static;
                    }
                }
                "ip_hash" => {
                    options.selection = SelectionKind::Ketama;
                    options.selector = source_addr_and_uri_path_selector;
                }
                "least_conn" => {
                    options.selection = SelectionKind::RoundRobin;
                }
                "random" => {
                    options.selection = SelectionKind::Random;
                }
                "health_check" => {
                    options.health_checks = if parts.len() > 1 && parts[1] == "none" {
                        HealthCheckKind::None
                    } else {
                        HealthCheckKind::None
                    };
                }
                _ => {
                    return Err(self.error(
                        &format!("Unknown upstream directive: {}", parts[0]),
                        line_num,
                        0,
                        line.len(),
                    ))
                }
            }
        }
        Ok((upstreams, options))
    }

    fn parse_location_block(
        &mut self,
        line: &str,
        line_num: usize,
        path_control: &mut PathControl,
        rate_limiting: &mut RateLimitingConfig,
    ) -> Result<(), NginxParseError> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 || !line.trim().ends_with("{") {
            return Err(self.error("Invalid location directive", line_num, 0, line.len()));
        }
        while let Some((line_num, line)) = self.next_clean_line() {
            let line = line.trim_end_matches(';');
            if line == "}" {
                break;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts[0] {
                "deny" => {
                    let mut filter = BTreeMap::new();
                    filter.insert("kind".to_string(), "block-cidr-range".to_string());
                    filter.insert(
                        "addrs".to_string(),
                        self.parse_value(&parts, 1, line_num, "deny")?,
                    );
                    path_control.request_filters.push(filter);
                }
                "proxy_set_header" => {
                    let mut filter = BTreeMap::new();
                    filter.insert("kind".to_string(), "upsert-header".to_string());
                    filter.insert(
                        "key".to_string(),
                        self.parse_value(&parts, 1, line_num, "proxy_set_header key")?,
                    );
                    filter.insert(
                        "value".to_string(),
                        self.parse_quoted_value(&parts, 2, line_num, "proxy_set_header value")?,
                    );
                    path_control.upstream_request_filters.push(filter);
                }
                "add_header" => {
                    let mut filter = BTreeMap::new();
                    filter.insert("kind".to_string(), "upsert-header".to_string());
                    filter.insert(
                        "key".to_string(),
                        self.parse_value(&parts, 1, line_num, "add_header key")?,
                    );
                    filter.insert(
                        "value".to_string(),
                        self.parse_quoted_value(&parts, 2, line_num, "add_header value")?,
                    );
                    path_control.upstream_response_filters.push(filter);
                }
                "proxy_hide_header" => {
                    let mut filter = BTreeMap::new();
                    filter.insert("kind".to_string(), "remove-header-key-regex".to_string());
                    filter.insert(
                        "pattern".to_string(),
                        self.parse_value(&parts, 1, line_num, "proxy_hide_header")?,
                    );
                    path_control.upstream_request_filters.push(filter);
                }
                "hide_header" => {
                    let mut filter = BTreeMap::new();
                    filter.insert("kind".to_string(), "remove-header-key-regex".to_string());
                    filter.insert(
                        "pattern".to_string(),
                        self.parse_value(&parts, 1, line_num, "hide_header")?,
                    );
                    path_control.upstream_response_filters.push(filter);
                }
                "limit_req" => {
                    self.parse_limit_req(&parts, line_num, rate_limiting)?;
                }
                "proxy_pass" => {
                    // Capture the proxy_pass directive from the location block so it can be promoted.
                    let mut filter = BTreeMap::new();
                    filter.insert("kind".to_string(), "proxy_pass".to_string());
                    filter.insert(
                        "value".to_string(),
                        self.parse_value(&parts, 1, line_num, "proxy_pass")?,
                    );
                    path_control.upstream_request_filters.push(filter);
                }
                _ => return Err(self.error("Unknown location directive", line_num, 0, line.len())),
            }
        }
        Ok(())
    }

    fn parse_limit_req(
        &self,
        parts: &[&str],
        line_num: usize,
        rate_limiting: &mut RateLimitingConfig,
    ) -> Result<(), NginxParseError> {
        let mut tokens_per_bucket = 10;
        let refill_qty = 1;
        let mut refill_rate_ms = 10;
        let max_buckets = 4000;
        let mut multi_kind = None;
        let mut single_kind = None;
        for i in 1..parts.len() {
            if parts[i].starts_with("zone=") {
                let zone = parts[i].trim_start_matches("zone=").trim_matches('"');
                match zone {
                    "ip" => multi_kind = Some(MultiRequestKeyKind::SourceIp),
                    _ => {
                        if zone.starts_with("any_") {
                            let pat = zone.trim_start_matches("any_");
                            single_kind = Some(SingleRequestKeyKind::UriGroup {
                                pattern: RegexShim::new(pat).map_err(|e| {
                                    self.error(
                                        &format!("Invalid regex: {}", e),
                                        line_num,
                                        0,
                                        parts[i].len(),
                                    )
                                })?,
                            });
                        } else {
                            multi_kind = Some(MultiRequestKeyKind::Uri {
                                pattern: RegexShim::new(zone).map_err(|e| {
                                    self.error(
                                        &format!("Invalid regex: {}", e),
                                        line_num,
                                        0,
                                        parts[i].len(),
                                    )
                                })?,
                            });
                        }
                    }
                }
            } else if parts[i].starts_with("burst=") {
                tokens_per_bucket = parts[i]
                    .trim_start_matches("burst=")
                    .parse()
                    .map_err(|_| self.error("Invalid burst value", line_num, 0, parts[i].len()))?;
            } else if parts[i].starts_with("rate=") {
                let rate_str = parts[i].trim_start_matches("rate=");
                if rate_str.ends_with("r/s") {
                    let rate: f64 = rate_str.trim_end_matches("r/s").parse().map_err(|_| {
                        self.error("Invalid rate value", line_num, 0, rate_str.len())
                    })?;
                    refill_rate_ms = (1000.0 / rate).round() as u64;
                } else if rate_str.ends_with("r/m") {
                    let rate: f64 = rate_str.trim_end_matches("r/m").parse().map_err(|_| {
                        self.error("Invalid rate value", line_num, 0, rate_str.len())
                    })?;
                    refill_rate_ms = (60000.0 / rate).round() as u64;
                }
            }
        }
        if let Some(kind) = multi_kind {
            rate_limiting.rules.push(AllRateConfig::Multi {
                kind,
                config: MultiRaterConfig {
                    threads: 4,
                    max_buckets,
                    max_tokens_per_bucket: tokens_per_bucket,
                    refill_qty,
                    refill_interval_millis: refill_rate_ms as usize,
                },
            });
        } else if let Some(kind) = single_kind {
            rate_limiting.rules.push(AllRateConfig::Single {
                kind,
                config: SingleInstanceConfig {
                    max_tokens_per_bucket: tokens_per_bucket,
                    refill_qty,
                    refill_interval_millis: refill_rate_ms as usize,
                },
            });
        } else {
            return Err(self.error(
                "Invalid or missing zone in limit_req",
                line_num,
                0,
                parts.join(" ").len(),
            ));
        }
        Ok(())
    }

    fn parse_value<T: FromStr>(
        &self,
        parts: &[&str],
        index: usize,
        line_num: usize,
        directive: &str,
    ) -> Result<T, NginxParseError> {
        if parts.len() <= index {
            return Err(self.error(
                &format!("Missing value for {}", directive),
                line_num,
                0,
                parts.join(" ").len(),
            ));
        }
        parts[index].parse().map_err(|_| {
            self.error(
                &format!("Invalid value for {}", directive),
                line_num,
                0,
                parts[index].len(),
            )
        })
    }

    fn parse_quoted_value(
        &self,
        parts: &[&str],
        index: usize,
        line_num: usize,
        directive: &str,
    ) -> Result<String, NginxParseError> {
        if parts.len() <= index {
            return Err(self.error(
                &format!("Missing value for {}", directive),
                line_num,
                0,
                parts.join(" ").len(),
            ));
        }
        Ok(parts[index].trim_matches('"').to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_basic_config() {
        let config = r#"
            system {
                threads_per_service 8;
                daemonize false;
                pid_file "/tmp/stryvo.pidfile";
            }
            http {
                server {
                    server_name Example1;
                    listen 0.0.0.0:8080;
                    upstream backend {
                        server 91.107.223.4:80;
                    }
                    location / {
                        proxy_pass backend;
                    }
                }
            }
        "#;
        let nginx = Nginx::from_string(config).unwrap();
        let config = Config::from(nginx);
        assert_eq!(config.threads_per_service, 8);
        assert_eq!(config.daemonize, false);
        assert_eq!(config.pid_file, Some(PathBuf::from("/tmp/stryvo.pidfile")));
        assert_eq!(config.basic_proxies.len(), 1);
    }

    #[test]
    fn test_missing_brace() {
        let config = r#"
            system
                threads_per_service 8;
            }
        "#;
        let result = Nginx::from_string(config);
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(e.line, 2);
            assert!(e.message.contains("Expected '{'"));
        }
    }

    #[test]
    fn test_invalid_value() {
        let config = r#"
            system {
                threads_per_service invalid;
            }
        "#;
        let result = Nginx::from_string(config);
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(e.line, 3);
            assert!(e.message.contains("Invalid value"));
        }
    }

    #[test]
    fn test_rate_limiting() {
        let config = r#"
            http {
                server {
                    listen 0.0.0.0:8080;
                    upstream backend { server 1.2.3.4:80; }
                    location / {
                        proxy_pass backend;
                        limit_req zone=ip burst=10 rate=100r/s;
                        limit_req zone=any_mp4 burst=50 rate=666r/s;
                    }
                }
            }
        "#;
        let nginx = Nginx::from_string(config).unwrap();
        let config = Config::from(nginx);
        assert_eq!(config.basic_proxies[0].rate_limiting.rules.len(), 2);
    }

    #[test]
    fn test_upstream_random() {
        let config = r#"
            upstream backend_simple {
                server 91.107.223.4:80;
                random;
            }
            http {
                server {
                    server_name Example2;
                    listen 0.0.0.0:8000;
                    location / {
                        proxy_pass backend_simple;
                    }
                }
            }
        "#;
        let nginx = Nginx::from_string(config).unwrap();
        let config = Config::from(nginx);
        assert_eq!(config.basic_proxies.len(), 1);
        assert_eq!(
            config.basic_proxies[0].upstream_options.selection,
            SelectionKind::Random
        );
    }

    #[test]
    fn test_cache_config() {
        let config = r#"
            http {
                server {
                    server_name CacheExample;
                    listen 0.0.0.0:8080;
                    upstream backend {
                        server 91.107.223.4:80;
                    }
                    cache {
                        cache_enabled true;
                        cache_ttl 7200;
                        browser_cache_enabled true;
                        browser_cache_ttl 3600;
                        cache_dir "/var/cache/stryvo";
                        max_file_size 1048576;
                        shard_count 16;
                    }
                    location / {
                        proxy_pass backend;
                    }
                }
            }
        "#;
        let nginx = Nginx::from_string(config).unwrap();
        let config = Config::from(nginx);
        assert_eq!(config.basic_proxies.len(), 1);
        let proxy = &config.basic_proxies[0];
        assert_eq!(proxy.name, "CacheExample");
        assert_eq!(proxy.cache_enabled, true);
        assert_eq!(proxy.cache_ttl, 7200);
        assert_eq!(proxy.browser_cache_enabled, true);
        assert_eq!(proxy.browser_cache_ttl, 3600);
        assert_eq!(proxy.cache_dir, Some(PathBuf::from("/var/cache/stryvo")));
        assert_eq!(proxy.max_file_size, Some(1048576));
        assert_eq!(proxy.shard_count, Some(16));
    }
}