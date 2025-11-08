#[cfg(test)]
mod integration_tests {
    use stryvo::config::internal::{Config, VHostConfig, DomainConfig, TlsConfig, ListenerConfig, ListenerKind, UpstreamOptions, PathControl, RateLimitingConfig};
    use stryvo::proxy::stryvo_vhost_service;
    use pingora::{server::Server, upstreams::peer::HttpPeer};
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Test that the main.rs integration can create VHost services correctly
    #[test]
    fn test_main_vhost_integration() {
        // Create a test VHost configuration similar to what main.rs would process
        let mut domains = HashMap::new();
        
        // Resolve test certificate files from available locations
        let cert_path = {
            let candidates = [
                "source/stryvo/assets/certs/test.local.crt",
                "assets/certs/test.local.crt",
                "source/stryvo/assets/test.crt",
                "assets/test.crt",
            ];
            candidates
                .iter()
                .map(|p| PathBuf::from(p))
                .find(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("source/stryvo/assets/certs/test.local.crt"))
        };
        
        let key_path = {
            let candidates = [
                "source/stryvo/assets/certs/test.local.key",
                "assets/certs/test.local.key",
                "source/stryvo/assets/test.key",
                "assets/test.key",
            ];
            candidates
                .iter()
                .map(|p| PathBuf::from(p))
                .find(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("source/stryvo/assets/certs/test.local.key"))
        };
        
        domains.insert("example.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.10:80", false, String::new())],
            ssl_config: Some(TlsConfig {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                auto_ssl: false,
            }),
            upstream_options: UpstreamOptions::default(),
        });

        let vhost_config = VHostConfig {
            name: "integration-test-vhost".to_string(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "127.0.0.1:18080".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains,
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("192.168.1.100:80", false, String::new())],
                ssl_config: Some(TlsConfig {
                    cert_path,
                    key_path,
                    auto_ssl: false,
                }),
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

        // Create a test server configuration (similar to main.rs)
        let test_config = Config {
            validate_configs: false,
            threads_per_service: 1,
            daemonize: false,
            pid_file: None,
            upgrade_socket: None,
            upgrade: false,
            basic_proxies: vec![],
            vhost_proxies: vec![vhost_config.clone()],
            file_servers: vec![],
            logging: stryvo::logging::LoggingConfig::default(),
            acme: stryvo::acme::config::AcmeConfig::default(),
        };

        // Create a test server (similar to main.rs)
        let test_server = Server::new_with_opt_and_conf(
            test_config.pingora_opt(),
            test_config.pingora_server_conf()
        );

        // Test the VHost service creation (this is what main.rs does)
        let service = stryvo_vhost_service(vhost_config, None, &test_server);
        
        // Verify the service was created successfully
        assert_eq!(service.name(), "integration-test-vhost");
        
        // The service should be ready to be added to the server
        // In main.rs, this would be: my_server.add_services(vec![service]);
        // We can't actually start the server in a test, but we can verify it was created
        
        println!("Integration test passed: VHost service created successfully");
    }

    /// Test that main.rs can handle multiple VHost services
    #[test]
    fn test_main_multiple_vhost_integration() {
        // Create multiple VHost configurations
        let cert_path = {
            let candidates = [
                "source/stryvo/assets/certs/test.local.crt",
                "assets/certs/test.local.crt",
                "source/stryvo/assets/test.crt",
                "assets/test.crt",
            ];
            candidates
                .iter()
                .map(|p| PathBuf::from(p))
                .find(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("source/stryvo/assets/certs/test.local.crt"))
        };
        
        let key_path = {
            let candidates = [
                "source/stryvo/assets/certs/test.local.key",
                "assets/certs/test.local.key",
                "source/stryvo/assets/test.key",
                "assets/test.key",
            ];
            candidates
                .iter()
                .map(|p| PathBuf::from(p))
                .find(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("source/stryvo/assets/certs/test.local.key"))
        };

        let mut vhost_configs = vec![];

        // First VHost service
        let mut domains1 = HashMap::new();
        domains1.insert("site1.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.10:80", false, String::new())],
            ssl_config: Some(TlsConfig {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                auto_ssl: false,
            }),
            upstream_options: UpstreamOptions::default(),
        });

        vhost_configs.push(VHostConfig {
            name: "vhost-service-1".to_string(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "127.0.0.1:18081".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains: domains1,
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("192.168.1.100:80", false, String::new())],
                ssl_config: Some(TlsConfig {
                    cert_path: cert_path.clone(),
                    key_path: key_path.clone(),
                    auto_ssl: false,
                }),
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
        });

        // Second VHost service
        let mut domains2 = HashMap::new();
        domains2.insert("site2.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.20:80", false, String::new())],
            ssl_config: Some(TlsConfig {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                auto_ssl: false,
            }),
            upstream_options: UpstreamOptions::default(),
        });

        vhost_configs.push(VHostConfig {
            name: "vhost-service-2".to_string(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "127.0.0.1:18082".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains: domains2,
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("192.168.1.200:80", false, String::new())],
                ssl_config: Some(TlsConfig {
                    cert_path,
                    key_path,
                    auto_ssl: false,
                }),
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
        });

        // Create test server
        let test_config = Config {
            validate_configs: false,
            threads_per_service: 1,
            daemonize: false,
            pid_file: None,
            upgrade_socket: None,
            upgrade: false,
            basic_proxies: vec![],
            vhost_proxies: vhost_configs.clone(),
            file_servers: vec![],
            logging: stryvo::logging::LoggingConfig::default(),
            acme: stryvo::acme::config::AcmeConfig::default(),
        };

        let test_server = Server::new_with_opt_and_conf(
            test_config.pingora_opt(),
            test_config.pingora_server_conf()
        );

        // Test creating multiple VHost services (simulating main.rs loop)
        let mut services = vec![];
        for vhost_conf in vhost_configs {
            let service = stryvo_vhost_service(vhost_conf, None, &test_server);
            services.push(service);
        }

        // Verify both services were created
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].name(), "vhost-service-1");
        assert_eq!(services[1].name(), "vhost-service-2");

        println!("Multiple VHost integration test passed: {} services created", services.len());
    }

    /// Test that VHost service creation handles validation errors gracefully
    #[test]
    fn test_main_vhost_validation_error_handling() {
        // Create an invalid VHost configuration (no upstreams)
        let vhost_config = VHostConfig {
            name: "invalid-vhost".to_string(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "127.0.0.1:18083".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains: HashMap::new(),
            default_domain: DomainConfig {
                upstreams: vec![], // Invalid: no upstreams
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

        let test_config = Config {
            validate_configs: false,
            threads_per_service: 1,
            daemonize: false,
            pid_file: None,
            upgrade_socket: None,
            upgrade: false,
            basic_proxies: vec![],
            vhost_proxies: vec![vhost_config.clone()],
            file_servers: vec![],
            logging: stryvo::logging::LoggingConfig::default(),
            acme: stryvo::acme::config::AcmeConfig::default(),
        };

        let _test_server = Server::new_with_opt_and_conf(
            test_config.pingora_opt(),
            test_config.pingora_server_conf()
        );

        // Test that invalid configuration is caught during validation
        // Note: In the actual main.rs, this would panic, but we test the validation separately
        let validation_result = vhost_config.validate();
        assert!(validation_result.is_err());
        assert!(validation_result.unwrap_err().contains("must have at least one upstream server"));

        println!("VHost validation error handling test passed");
    }
}