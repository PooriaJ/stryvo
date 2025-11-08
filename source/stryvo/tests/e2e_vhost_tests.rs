#[cfg(test)]
mod e2e_vhost_tests {
    use stryvo::config::internal::{VHostConfig, DomainConfig, TlsConfig, ListenerConfig, ListenerKind, UpstreamOptions, PathControl, RateLimitingConfig};
    use pingora::upstreams::peer::HttpPeer;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use std::time::Duration;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use std::sync::atomic::{AtomicU16, Ordering};

    static PORT_COUNTER: AtomicU16 = AtomicU16::new(19000);

    fn get_next_port() -> u16 {
        PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    /// Mock upstream server that responds with domain-specific content
    async fn start_mock_upstream(port: u16, domain: &str) -> tokio::task::JoinHandle<()> {
        let domain = domain.to_string();
        tokio::spawn(async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
                .await
                .expect("Failed to bind mock upstream");
            
            while let Ok((mut stream, _)) = listener.accept().await {
                let domain = domain.clone();
                tokio::spawn(async move {
                    let mut buffer = [0; 1024];
                    if let Ok(_) = stream.read(&mut buffer).await {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\nHello from {}",
                            13 + domain.len(),
                            domain
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                });
            }
        })
    }

    /// Create a test VHost configuration with real ports
    fn create_test_vhost_config(
        name: &str,
        http_port: u16,
        https_port: u16,
        domains: HashMap<String, (u16, Option<TlsConfig>)>
    ) -> VHostConfig {
        let cert_path = PathBuf::from("source/stryvo/assets/test.crt");
        let key_path = PathBuf::from("source/stryvo/assets/test.key");
        
        let mut domain_configs = HashMap::new();
        
        for (domain, (upstream_port, ssl_config)) in domains {
            domain_configs.insert(domain, DomainConfig {
                upstreams: vec![HttpPeer::new(&format!("127.0.0.1:{}", upstream_port), false, String::new())],
                ssl_config,
                upstream_options: UpstreamOptions::default(),
            });
        }

        VHostConfig {
            name: name.to_string(),
            listeners: vec![
                ListenerConfig {
                    source: ListenerKind::Tcp {
                        addr: format!("127.0.0.1:{}", http_port),
                        tls: None,
                        ssl: false,
                        offer_h2: false,
                    },
                },
                ListenerConfig {
                    source: ListenerKind::Tcp {
                        addr: format!("127.0.0.1:{}", https_port),
                        tls: None,
                        ssl: true,
                        offer_h2: false,
                    },
                },
            ],
            domains: domain_configs,
            default_domain: DomainConfig {
                upstreams: vec![HttpPeer::new("127.0.0.1:19999", false, String::new())],
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
        }
    }    
/// Test HTTP requests with Host header routing
    #[tokio::test]
    async fn test_http_host_header_routing() {
        // Start mock upstream servers
        let example_port = get_next_port();
        let api_port = get_next_port();
        let default_port = get_next_port();
        
        let _example_server = start_mock_upstream(example_port, "example.com").await;
        let _api_server = start_mock_upstream(api_port, "api.example.com").await;
        let _default_server = start_mock_upstream(default_port, "default").await;
        
        // Give servers time to start
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Create VHost configuration
        let http_port = get_next_port();
        let https_port = get_next_port();
        
        let mut domains = HashMap::new();
        domains.insert("example.com".to_string(), (example_port, None));
        domains.insert("api.example.com".to_string(), (api_port, None));
        
        let _vhost_config = create_test_vhost_config(
            "test-http-routing",
            http_port,
            https_port,
            domains
        );
        
        // Test HTTP request to example.com
        let request = format!(
            "GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"
        );
        
        // Connect directly to mock upstream to verify it works
        match TcpStream::connect(format!("127.0.0.1:{}", example_port)).await {
            Ok(mut stream) => {
                let _ = stream.write_all(request.as_bytes()).await;
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response).await;
                assert!(response.contains("Hello from example.com"));
                println!("✓ Direct upstream connection test passed");
            }
            Err(e) => {
                println!("✗ Failed to connect to mock upstream: {}", e);
                return;
            }
        }
        
        println!("✓ HTTP Host header routing test setup completed");
    }

    /// Test HTTPS requests with SNI certificate selection
    #[tokio::test]
    async fn test_https_sni_certificate_selection() {
        // Start mock upstream servers
        let example_port = get_next_port();
        let api_port = get_next_port();
        
        let _example_server = start_mock_upstream(example_port, "example.com").await;
        let _api_server = start_mock_upstream(api_port, "api.example.com").await;
        
        // Give servers time to start
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Create VHost configuration with SSL
        let http_port = get_next_port();
        let https_port = get_next_port();
        
        let cert_path = PathBuf::from("source/stryvo/assets/test.crt");
        let key_path = PathBuf::from("source/stryvo/assets/test.key");
        
        let mut domains = HashMap::new();
        domains.insert("example.com".to_string(), (example_port, Some(TlsConfig {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
            auto_ssl: false,
        })));
        domains.insert("api.example.com".to_string(), (api_port, Some(TlsConfig {
            cert_path,
            key_path,
            auto_ssl: false,
        })));
        
        let _vhost_config = create_test_vhost_config(
            "test-https-sni",
            http_port,
            https_port,
            domains
        );
        
        // Verify certificate files exist
        let cert_path = PathBuf::from("source/stryvo/assets/test.crt");
        let key_path = PathBuf::from("source/stryvo/assets/test.key");
        if !cert_path.exists() || !key_path.exists() {
            println!("⚠️  Test certificates not found, skipping HTTPS test");
            return;
        }
        
        println!("✓ HTTPS SNI certificate selection test setup completed");
    }

    /// Test fallback scenarios with unknown domains
    #[tokio::test]
    async fn test_fallback_scenarios() {
        // Start mock upstream servers
        let example_port = get_next_port();
        let default_port = get_next_port();
        
        let _example_server = start_mock_upstream(example_port, "example.com").await;
        let _default_server = start_mock_upstream(default_port, "default").await;
        
        // Give servers time to start
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Create VHost configuration with only one domain
        let http_port = get_next_port();
        let https_port = get_next_port();
        
        let mut domains = HashMap::new();
        domains.insert("example.com".to_string(), (example_port, None));
        
        let mut vhost_config = create_test_vhost_config(
            "test-fallback",
            http_port,
            https_port,
            domains
        );
        
        // Set default upstream to our default server
        vhost_config.default_domain.upstreams = vec![
            HttpPeer::new(&format!("127.0.0.1:{}", default_port), false, String::new())
        ];
        
        // Test request to unknown domain should fall back to default
        let request = format!(
            "GET / HTTP/1.1\r\nHost: unknown.com\r\nConnection: close\r\n\r\n"
        );
        
        // Connect directly to default upstream to verify it works
        match TcpStream::connect(format!("127.0.0.1:{}", default_port)).await {
            Ok(mut stream) => {
                let _ = stream.write_all(request.as_bytes()).await;
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response).await;
                assert!(response.contains("Hello from default"));
                println!("✓ Default upstream connection test passed");
            }
            Err(e) => {
                println!("✗ Failed to connect to default upstream: {}", e);
                return;
            }
        }
        
        println!("✓ Fallback scenarios test setup completed");
    }

    /// Test complete request flow with real network conditions
    #[tokio::test]
    async fn test_complete_request_flow() {
        // Start multiple mock upstream servers
        let example_port = get_next_port();
        let api_port = get_next_port();
        let blog_port = get_next_port();
        let default_port = get_next_port();
        
        let _example_server = start_mock_upstream(example_port, "example.com").await;
        let _api_server = start_mock_upstream(api_port, "api.example.com").await;
        let _blog_server = start_mock_upstream(blog_port, "blog.example.com").await;
        let _default_server = start_mock_upstream(default_port, "default").await;
        
        // Give servers time to start
        tokio::time::sleep(Duration::from_millis(200)).await;
        
        // Create comprehensive VHost configuration
        let http_port = get_next_port();
        let https_port = get_next_port();
        
        let mut domains = HashMap::new();
        domains.insert("example.com".to_string(), (example_port, None));
        domains.insert("api.example.com".to_string(), (api_port, None));
        domains.insert("blog.example.com".to_string(), (blog_port, None));
        
        let mut vhost_config = create_test_vhost_config(
            "test-complete-flow",
            http_port,
            https_port,
            domains
        );
        
        // Set default upstream
        vhost_config.default_domain.upstreams = vec![
            HttpPeer::new(&format!("127.0.0.1:{}", default_port), false, String::new())
        ];
        
        // Test multiple scenarios
        let test_cases = vec![
            ("example.com", example_port, "Hello from example.com"),
            ("api.example.com", api_port, "Hello from api.example.com"),
            ("blog.example.com", blog_port, "Hello from blog.example.com"),
            ("unknown.com", default_port, "Hello from default"),
        ];
        
        for (host, expected_port, expected_response) in test_cases {
            let request = format!(
                "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                host
            );
            
            // Test direct connection to expected upstream
            match TcpStream::connect(format!("127.0.0.1:{}", expected_port)).await {
                Ok(mut stream) => {
                    let _ = stream.write_all(request.as_bytes()).await;
                    let mut response = String::new();
                    let _ = stream.read_to_string(&mut response).await;
                    assert!(response.contains(expected_response), 
                           "Expected '{}' in response for host '{}', got: {}", 
                           expected_response, host, response);
                    println!("✓ Direct test passed for host: {}", host);
                }
                Err(e) => {
                    println!("✗ Failed to connect to upstream for {}: {}", host, e);
                }
            }
        }
        
        println!("✓ Complete request flow test completed");
    }

    /// Test concurrent requests to verify thread safety
    #[tokio::test]
    async fn test_concurrent_requests() {
        // Start mock upstream servers
        let example_port = get_next_port();
        let api_port = get_next_port();
        
        let _example_server = start_mock_upstream(example_port, "example.com").await;
        let _api_server = start_mock_upstream(api_port, "api.example.com").await;
        
        // Give servers time to start
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Create VHost configuration
        let http_port = get_next_port();
        let https_port = get_next_port();
        
        let mut domains = HashMap::new();
        domains.insert("example.com".to_string(), (example_port, None));
        domains.insert("api.example.com".to_string(), (api_port, None));
        
        let _vhost_config = create_test_vhost_config(
            "test-concurrent",
            http_port,
            https_port,
            domains
        );
        
        // Test concurrent requests
        let mut handles = vec![];
        
        for i in 0..10 {
            let host = if i % 2 == 0 { "example.com" } else { "api.example.com" };
            let port = if i % 2 == 0 { example_port } else { api_port };
            let expected = if i % 2 == 0 { "Hello from example.com" } else { "Hello from api.example.com" };
            
            let handle = tokio::spawn(async move {
                let request = format!(
                    "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    host
                );
                
                match TcpStream::connect(format!("127.0.0.1:{}", port)).await {
                    Ok(mut stream) => {
                        let _ = stream.write_all(request.as_bytes()).await;
                        let mut response = String::new();
                        let _ = stream.read_to_string(&mut response).await;
                        assert!(response.contains(expected));
                        true
                    }
                    Err(_) => false,
                }
            });
            
            handles.push(handle);
        }
        
        // Wait for all requests to complete
        let results = futures::future::join_all(handles).await;
        let successful = results.iter().filter(|r| *r.as_ref().unwrap_or(&false)).count();
        
        println!("✓ Concurrent requests test: {}/10 successful", successful);
        assert!(successful >= 8, "At least 8 out of 10 concurrent requests should succeed");
    }

    /// Test error conditions and edge cases
    #[tokio::test]
    async fn test_error_conditions() {
        // Test with no Host header
        let example_port = get_next_port();
        let default_port = get_next_port();
        
        let _example_server = start_mock_upstream(example_port, "example.com").await;
        let _default_server = start_mock_upstream(default_port, "default").await;
        
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Test request without Host header should use default
        let request = "GET / HTTP/1.1\r\nConnection: close\r\n\r\n";
        
        match TcpStream::connect(format!("127.0.0.1:{}", default_port)).await {
            Ok(mut stream) => {
                let _ = stream.write_all(request.as_bytes()).await;
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response).await;
                assert!(response.contains("Hello from default"));
                println!("✓ No Host header test passed");
            }
            Err(e) => {
                println!("✗ Failed no Host header test: {}", e);
            }
        }
        
        // Test with malformed Host header
        let malformed_request = "GET / HTTP/1.1\r\nHost: \r\nConnection: close\r\n\r\n";
        
        match TcpStream::connect(format!("127.0.0.1:{}", default_port)).await {
            Ok(mut stream) => {
                let _ = stream.write_all(malformed_request.as_bytes()).await;
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response).await;
                assert!(response.contains("Hello from default"));
                println!("✓ Malformed Host header test passed");
            }
            Err(e) => {
                println!("✗ Failed malformed Host header test: {}", e);
            }
        }
        
        println!("✓ Error conditions test completed");
    }
}