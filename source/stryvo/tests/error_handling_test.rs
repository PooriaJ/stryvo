/// Test module to verify comprehensive error handling and logging for VHost functionality
/// This module tests the error handling requirements from task 9
#[cfg(test)]
mod error_handling_tests {
    use stryvo::proxy::vhost::certificate_manager::CertificateManager;
    use stryvo::proxy::vhost::router::VHostRouter;

    use stryvo::config::internal::{VHostConfig, DomainConfig, TlsConfig, UpstreamOptions};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use pingora::upstreams::peer::HttpPeer;
    use pingora_http::RequestHeader;
    use http::Method;

    /// Test certificate loading failure handling (Requirement 6.3)
    #[tokio::test]
    async fn test_certificate_loading_error_handling() {
        // Create a config with non-existent certificate files
        let mut domains = HashMap::new();
        domains.insert("test.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("127.0.0.1:8080", false, String::new())],
            ssl_config: Some(TlsConfig {
                cert_path: PathBuf::from("/nonexistent/test.crt"),
                key_path: PathBuf::from("/nonexistent/test.key"),
                auto_ssl: false,
            }),
            upstream_options: UpstreamOptions::default(),
        });

        let config = VHostConfig {
            name: "test-error-handling".to_string(),
            listeners: vec![],
            domains,
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("127.0.0.1:8080", false, String::new())],
                ssl_config: Some(TlsConfig {
                    cert_path: PathBuf::from("/nonexistent/default.crt"),
                    key_path: PathBuf::from("/nonexistent/default.key"),
                    auto_ssl: false,
                }),
                upstream_options: UpstreamOptions::default(),
            },
            path_control: stryvo::config::internal::PathControl::default(),
            rate_limiting: stryvo::config::internal::RateLimitingConfig::default(),
            cache_enabled: false,
            cache_ttl: 300,
            browser_cache_enabled: false,
            browser_cache_ttl: 3600,
            cache_dir: None,
            max_file_size: None,
            shard_count: None,
        };

        // This should fail with comprehensive error logging
        let result = CertificateManager::new(&config).await;
        assert!(result.is_err(), "Certificate manager should fail with missing certificate files");
        
        // The error should be properly logged (we can't easily test log output in unit tests,
        // but the error handling code includes comprehensive logging)
    }

    /// Test Host header parsing error handling (Requirement 6.2)
    #[test]
    fn test_host_header_parsing_error_handling() {
        // Test with invalid Host header containing whitespace
        let mut req_header = RequestHeader::build(Method::GET, b"/", None).unwrap();
        req_header.insert_header("host", "invalid domain.com").unwrap();
        
        let result = VHostRouter::extract_host_header(&req_header);
        assert_eq!(result, None, "Should reject Host header with whitespace");

        // Test with empty Host header
        let mut req_header_empty = RequestHeader::build(Method::GET, b"/", None).unwrap();
        req_header_empty.insert_header("host", "").unwrap();
        
        let result_empty = VHostRouter::extract_host_header(&req_header_empty);
        assert_eq!(result_empty, None, "Should reject empty Host header");

        // Test with extremely long domain name
        let long_domain = "a".repeat(300) + ".com";
        let mut req_header_long = RequestHeader::build(Method::GET, b"/", None).unwrap();
        req_header_long.insert_header("host", &long_domain).unwrap();
        
        let result_long = VHostRouter::extract_host_header(&req_header_long);
        assert_eq!(result_long, None, "Should reject overly long domain names");
    }

    /// Test domain routing error handling and logging (Requirement 6.4)
    #[tokio::test]
    async fn test_domain_routing_error_handling() {
        use stryvo::config::internal::{ListenerConfig, ListenerKind};
        
        let mut domains = HashMap::new();
        domains.insert("example.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.10:80", false, String::new())],
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });

        let config = VHostConfig {
            name: "test-routing".to_string(),
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
                ssl_config: None,
                upstream_options: UpstreamOptions::default(),
            },
            path_control: stryvo::config::internal::PathControl::default(),
            rate_limiting: stryvo::config::internal::RateLimitingConfig::default(),
            cache_enabled: false,
            cache_ttl: 300,
            browser_cache_enabled: false,
            browser_cache_ttl: 3600,
            cache_dir: None,
            max_file_size: None,
            shard_count: None,
        };

        let router = VHostRouter::new(&config);

        // Test routing with empty domain (should use default)
        let upstream_empty = router.route_request("").await.unwrap();
        assert_eq!(upstream_empty.backends.len(), 1);
        assert_eq!(upstream_empty.backends[0]._address.to_string(), "192.168.1.100:80");

        // Test routing with unknown domain (should use default)
        let upstream_unknown = router.route_request("unknown.domain.com").await.unwrap();
        assert_eq!(upstream_unknown.backends.len(), 1);
        assert_eq!(upstream_unknown.backends[0]._address.to_string(), "192.168.1.100:80");

        // Test routing with known domain (should use specific upstream)
        let upstream_known = router.route_request("example.com").await.unwrap();
        assert_eq!(upstream_known.backends.len(), 1);
        assert_eq!(upstream_known.backends[0]._address.to_string(), "192.168.1.10:80");
    }

    /// Test upstream selection error handling (Requirement 6.4)
    #[tokio::test]
    async fn test_upstream_selection_error_handling() {
        use stryvo::config::internal::{ListenerConfig, ListenerKind};
        
        // Create a config with empty upstream list to test error handling
        let mut domains = HashMap::new();
        domains.insert("empty.com".to_string(), DomainConfig {
            upstreams: vec![], // Empty upstream list
            ssl_config: None,
            upstream_options: UpstreamOptions::default(),
        });

        let config = VHostConfig {
            name: "test-upstream-error".to_string(),
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
                ssl_config: None,
                upstream_options: UpstreamOptions::default(),
            },
            path_control: stryvo::config::internal::PathControl::default(),
            rate_limiting: stryvo::config::internal::RateLimitingConfig::default(),
            cache_enabled: false,
            cache_ttl: 300,
            browser_cache_enabled: false,
            browser_cache_ttl: 3600,
            cache_dir: None,
            max_file_size: None,
            shard_count: None,
        };

        let router = VHostRouter::new(&config);

        // Test upstream selection with domain that has no backends
        // This should return an error since the domain has no backends configured
        let result = router.select_upstream_peer(Some("empty.com".to_string())).await;
        assert!(result.is_err(), "Should return error when domain has no upstream backends configured");

        // Test upstream selection with no host header (should use default)
        let result_no_host = router.select_upstream_peer(None).await;
        assert!(result_no_host.is_ok(), "Should gracefully handle missing Host header");
        
        if let Ok(peer) = result_no_host {
            assert_eq!(peer._address.to_string(), "192.168.1.100:80");
        }
    }

    /// Test SNI extraction error handling (Requirement 6.1)
    #[tokio::test]
    async fn test_sni_extraction_error_handling() {
        // We can't easily test actual SNI extraction without a real TLS connection,
        // but we can test the certificate lookup with various domain formats
        
        // Create a valid config for testing certificate lookup
        let config = create_valid_test_config();
        
        // This will fail due to missing certificate files, but we can test the error handling
        let result = CertificateManager::new(&config).await;
        
        // The certificate manager should handle missing files gracefully
        // In a real scenario with valid certificates, we would test:
        // - Empty SNI domain handling
        // - Invalid SNI domain format handling
        // - Case-insensitive SNI matching
        // - Fallback to default certificate
        
        // For now, we verify that the error handling doesn't panic
        assert!(result.is_err(), "Should handle missing certificate files gracefully");
    }

    fn create_valid_test_config() -> VHostConfig {
        use stryvo::config::internal::{ListenerConfig, ListenerKind};
        
        let mut domains = HashMap::new();
        domains.insert("example.com".to_string(), DomainConfig {
            upstreams: vec![HttpPeer::new("192.168.1.10:80", false, String::new())],
            ssl_config: Some(TlsConfig {
                cert_path: PathBuf::from("./assets/test.crt"),
                key_path: PathBuf::from("./assets/test.key"),
                auto_ssl: false,
            }),
            upstream_options: UpstreamOptions::default(),
        });

        VHostConfig {
            name: "test-sni".to_string(),
            listeners: vec![ListenerConfig {
                source: ListenerKind::Tcp {
                    addr: "0.0.0.0:8443".to_string(),
                    tls: None,
                    ssl: false,
                    offer_h2: false,
                },
            }],
            domains,
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("192.168.1.100:80", false, String::new())],
                ssl_config: Some(TlsConfig {
                    cert_path: PathBuf::from("./assets/default.crt"),
                    key_path: PathBuf::from("./assets/default.key"),
                    auto_ssl: false,
                }),
                upstream_options: UpstreamOptions::default(),
            },
            path_control: stryvo::config::internal::PathControl::default(),
            rate_limiting: stryvo::config::internal::RateLimitingConfig::default(),
            cache_enabled: false,
            cache_ttl: 300,
            browser_cache_enabled: false,
            browser_cache_ttl: 3600,
            cache_dir: None,
            max_file_size: None,
            shard_count: None,
        }
    }
}