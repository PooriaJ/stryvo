# VHost System Documentation

## Overview

The VHost (Virtual Host) system in Stryvo is a sophisticated reverse proxy management system that handles domain routing, SSL/TLS certificate management, and upstream service configuration. It provides dynamic configuration management with real-time updates and comprehensive security features.

## Technical Specifications

### Core Components

#### 1. Certificate Manager (`CertificateManager`)
- **Purpose**: SSL/TLS certificate lifecycle management
- **Location**: `src/proxy/vhost/certificate.rs`
- **Key Features**:
  - Automatic certificate provisioning
  - Certificate renewal and rotation
  - SNI (Server Name Indication) support
  - Multi-domain certificate handling

#### 2. Router (`VHostRouter`)
- **Purpose**: Request routing and load balancing
- **Location**: `src/proxy/vhost/router.rs`
- **Key Features**:
  - Host-based routing
  - Path-based routing
  - Weighted load balancing
  - Health check integration

#### 3. Service Handler (`VHostProxyService`)
- **Purpose**: HTTP request processing and proxying
- **Location**: `src/proxy/vhost/service.rs`
- **Key Features**:
  - HTTP/HTTPS request handling
  - Upstream connection management
  - Response processing and transformation
  - Access logging and metrics collection

## Request Flow Diagram

```
┌─────────────┐    ┌──────────────┐    ┌─────────────┐    ┌──────────────┐
│   Client    │───▶│  TLS/SSL     │───▶│   VHost     │───▶│   Upstream   │
│  Request    │    │  Termination │    │   Router    │    │   Service    │
└─────────────┘    └──────────────┘    └─────────────┘    └──────────────┘
       │                   │                   │                   │
       │                   ▼                   ▼                   ▼
       │            ┌──────────────┐    ┌─────────────┐    ┌──────────────┐
       │            │ Certificate  │    │   Service   │    │   Response   │
       │            │  Validation  │    │  Selection  │    │  Processing  │
       │            └──────────────┘    └─────────────┘    └──────────────┘
       │                   │                   │                   │
       │                   ▼                   ▼                   ▼
       │            ┌──────────────┐    ┌─────────────┐    ┌──────────────┐
       │            │   SNI/Host   │    │    Load     │    │   Caching &  │
       │            │  Resolution  │    │  Balancing  │    │ Compression  │
       │            └──────────────┘    └─────────────┘    └──────────────┘
       │                                        │                   │
       └────────────────────────────────────────┼───────────────────┘
                                                ▼
                                        ┌──────────────┐
                                        │   Access     │
                                        │   Logging    │
                                        └──────────────┘
```

## Component Functionality

### Configuration Structure

**Service Configuration**:
```yaml
services:
  example-service:
    domains:
      - example.com
      - www.example.com
    upstream:
      - host: 192.168.1.100
        port: 8080
        weight: 100
    ssl:
      certificate_path: "/path/to/cert.pem"
      private_key_path: "/path/to/key.pem"
    cache:
      enabled: true
      ttl: 3600
```

### Certificate Manager

**SSL/TLS Features**:
- **Automatic Certificate Discovery**: Scans certificate directories
- **SNI Support**: Multiple certificates per IP address
- **Certificate Validation**: Expiry checking and validation
- **Hot Certificate Reload**: Updates certificates without service interruption

**Certificate Storage**:
```rust
pub struct CertificateStore {
    certificates: HashMap<String, CertificateChain>,
    default_certificate: Option<CertificateChain>,
}
```

### Router

**Routing Logic**:
1. **Host Header Matching**: Primary routing mechanism
2. **Path-based Routing**: Secondary routing for complex scenarios
3. **Load Balancing**: Weighted round-robin algorithm
4. **Health Checks**: Automatic upstream health monitoring

**Load Balancing Algorithms**:
- **Round Robin**: Equal distribution across upstreams
- **Weighted Round Robin**: Distribution based on upstream weights
- **Least Connections**: Routes to upstream with fewest active connections
- **IP Hash**: Consistent routing based on client IP

### Service Handler

**Request Processing Pipeline**:
1. **TLS Termination**: SSL/TLS decryption and certificate validation
2. **Host Resolution**: Determine target service from Host header
3. **Upstream Selection**: Choose appropriate backend server
4. **Request Forwarding**: Proxy request to upstream
5. **Response Processing**: Handle upstream response
6. **Logging & Metrics**: Record access logs and performance metrics

## Configuration Guidelines

### Service Configuration

**Basic Service Setup**:
```yaml
# config/services/my-app.yaml
name: my-app
domains:
  - myapp.example.com
  - api.myapp.example.com
upstream:
  - host: 10.0.1.10
    port: 3000
    weight: 100
    health_check:
      path: /health
      interval: 30s
      timeout: 5s
ssl:
  certificate_path: "/etc/ssl/certs/myapp.crt"
  private_key_path: "/etc/ssl/private/myapp.key"
  protocols:
    - TLSv1.2
    - TLSv1.3
cache:
  enabled: true
  ttl: 1800
  vary_headers:
    - Accept-Encoding
    - Authorization
compression:
  enabled: true
  algorithms:
    - gzip
    - brotli
  min_size: 1024
rate_limiting:
  requests_per_minute: 1000
  burst_size: 100
```

### Advanced Configuration

**Multi-Environment Setup**:
```yaml
# Production environment
name: my-app-prod
domains:
  - myapp.com
  - www.myapp.com
upstream:
  - host: prod-server-1.internal
    port: 8080
    weight: 50
  - host: prod-server-2.internal
    port: 8080
    weight: 50
ssl:
  certificate_path: "/etc/ssl/certs/myapp-prod.crt"
  private_key_path: "/etc/ssl/private/myapp-prod.key"
  hsts:
    enabled: true
    max_age: 31536000
    include_subdomains: true
security:
  cors:
    enabled: true
    allowed_origins:
      - https://admin.myapp.com
    allowed_methods:
      - GET
      - POST
      - PUT
      - DELETE
  headers:
    x_frame_options: DENY
    x_content_type_options: nosniff
    x_xss_protection: "1; mode=block"
```

### Best Practices

#### Security
1. **Certificate Management**:
   - Use strong encryption algorithms (RSA 2048+ or ECDSA P-256+)
   - Implement certificate rotation policies
   - Monitor certificate expiration dates
   - Use HSTS headers for enhanced security

2. **Access Control**:
   - Implement rate limiting to prevent abuse
   - Use IP whitelisting for administrative endpoints
   - Enable CORS policies for web applications
   - Set appropriate security headers

#### Performance
1. **Load Balancing**:
   - Configure health checks for all upstreams
   - Use weighted routing for capacity planning
   - Implement circuit breakers for fault tolerance
   - Monitor upstream response times

2. **Caching**:
   - Enable caching for static content
   - Configure appropriate TTL values
   - Use cache invalidation strategies
   - Monitor cache hit rates

#### Monitoring
1. **Logging**:
   - Enable comprehensive access logging
   - Configure structured logging formats
   - Implement log rotation policies
   - Set up log aggregation and analysis

2. **Metrics**:
   - Monitor request rates and response times
   - Track error rates and status codes
   - Monitor upstream health and availability
   - Set up alerting for critical metrics

## Troubleshooting

### Common Issues

1. **Certificate Problems**:
   - **Symptom**: SSL handshake failures
   - **Solution**: Verify certificate paths and permissions
   - **Debug**: Check certificate expiration and chain validity

2. **Routing Issues**:
   - **Symptom**: 404 errors or wrong upstream selection
   - **Solution**: Verify domain configuration and DNS resolution
   - **Debug**: Check host header matching and routing rules

3. **Upstream Connectivity**:
   - **Symptom**: 502/503 errors
   - **Solution**: Verify upstream health and network connectivity
   - **Debug**: Check upstream response times and health check status

### Debugging Commands

```bash
# Check service configuration
curl -H "Host: example.com" http://localhost:8080/debug/config

# Verify certificate status
openssl s_client -connect example.com:443 -servername example.com

# Test upstream connectivity
curl -v http://upstream-server:8080/health

# Monitor access logs
tail -f /var/log/stryvo/access.log
```

## Integration Examples

### Docker Compose Integration

```yaml
version: '3.8'
services:
  stryvo:
    image: stryvo:latest
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ./config:/etc/stryvo/config
      - ./certs:/etc/ssl/certs
      - ./logs:/var/log/stryvo
    environment:
      - STRYVO_CONFIG_PATH=/etc/stryvo/config
      - STRYVO_LOG_LEVEL=info
  
  app:
    image: my-app:latest
    ports:
      - "3000:3000"
    environment:
      - NODE_ENV=production
```

### Kubernetes Integration

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: stryvo-proxy
spec:
  replicas: 3
  selector:
    matchLabels:
      app: stryvo-proxy
  template:
    metadata:
      labels:
        app: stryvo-proxy
    spec:
      containers:
      - name: stryvo
        image: stryvo:latest
        ports:
        - containerPort: 80
        - containerPort: 443
        volumeMounts:
        - name: config
          mountPath: /etc/stryvo/config
        - name: certs
          mountPath: /etc/ssl/certs
      volumes:
      - name: config
        configMap:
          name: stryvo-config
      - name: certs
        secret:
          secretName: stryvo-certs
```

This documentation provides a comprehensive guide to understanding, configuring, and operating the VHost system in Stryvo. For additional support, refer to the logging and monitoring documentation for operational insights.