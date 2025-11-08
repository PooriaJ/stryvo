# Automatic SSL Certificate Management (ACME)

Stryvo includes built-in support for automatic SSL certificate management using the ACME protocol, compatible with Let's Encrypt and other ACME-compliant certificate authorities.

## Overview

The auto SSL system automatically:
- Obtains SSL certificates for your domains
- Validates domain ownership using HTTP-01 or DNS-01 challenges
- Renews certificates before they expire (default: 30 days before expiration)
- Manages certificate storage and rotation
- Integrates seamlessly with Stryvo's proxy services

## Quick Start

### 1. Basic Configuration

Add ACME configuration to your config file:

```yaml
# YAML format
acme:
  enabled: true
  directory-url: "https://acme-staging-v02.api.letsencrypt.org/directory"  # Use staging for testing
  email: "your-email@example.com"  # Required for Let's Encrypt
  cert-dir: "./acme-certs"
  account-dir: "./acme-accounts"
  renewal-days: 30
  challenge-type: "http-01"
  http-challenge:
    port: 80
    bind-addr: "0.0.0.0"
  key-type: "ECDSA_P256"
```

```kdl
// KDL format
acme {
    enabled true
    directory-url "https://acme-staging-v02.api.letsencrypt.org/directory"
    email "your-email@example.com"
    cert-dir "./acme-certs"
    account-dir "./acme-accounts"
    renewal-days 30
    challenge-type "http-01"
    http-challenge {
        port 80
        bind-addr "0.0.0.0"
    }
    key-type "ECDSA_P256"
}
```

### 2. Enable Auto SSL for Domains

For Virtual Host proxies, enable auto SSL per domain:

```yaml
# YAML format
vhost-proxy:
  - name: "MyProxy"
    listeners:
      - addr: "0.0.0.0:80"   # Required for HTTP-01 challenges
      - addr: "0.0.0.0:443"
        offer-h2: true
    domains:
      "example.com":
        upstreams:
          - proxy-addr: "127.0.0.1:8080"
        ssl-config:
          cert_path: ""      # Ignored when auto-ssl is enabled
          key_path: ""       # Ignored when auto-ssl is enabled
          auto-ssl: true     # Enable automatic SSL!
```

```kdl
// KDL format
services {
    MyProxy {
        listeners {
            "0.0.0.0:80"                                    // Required for HTTP-01 challenges
            "0.0.0.0:443" ssl=true offer-h2=true
        }
        vhost {
            domain "example.com" {
                upstreams {
                    "127.0.0.1:8080"
                }
                ssl-config {
                    auto-ssl true
                }
            }
        }
    }
}
```

### 3. Production Setup

For production, change the directory URL to Let's Encrypt production:

```yaml
acme:
  directory-url: "https://acme-v02.api.letsencrypt.org/directory"
```

## Configuration Options

### ACME Configuration

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable/disable ACME automatic SSL |
| `directory-url` | string | Let's Encrypt production | ACME directory URL |
| `email` | string | required | Email for ACME account registration |
| `cert-dir` | path | `"./certs"` | Directory to store certificates |
| `account-dir` | path | `"./acme-accounts"` | Directory to store account info |
| `renewal-days` | number | `30` | Renew certificates N days before expiration |
| `challenge-type` | string | `"http-01"` | Challenge type (`http-01` or `dns-01`) |
| `key-type` | string | `"ECDSA_P256"` | Certificate key type |
| `rsa-key-size` | number | `2048` | RSA key size (when using RSA keys) |

### SSL Configuration

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `auto-ssl` | boolean | `false` | Enable automatic SSL for this domain |
| `cert_path` | path | required* | Path to certificate file (ignored with auto-ssl) |
| `key_path` | path | required* | Path to private key file (ignored with auto-ssl) |

*Required when `auto-ssl` is `false`

### Challenge Types

#### HTTP-01 Challenge (Default)
- Validates domain ownership by serving a file over HTTP
- Requires port 80 to be accessible from the internet
- Works for single domains and subdomains
- Cannot be used for wildcard certificates

```yaml
challenge-type: "http-01"
http-challenge:
  port: 80
  bind-addr: "0.0.0.0"
```

#### DNS-01 Challenge (Future)
- Validates domain ownership by creating DNS TXT records
- Supports wildcard certificates
- Requires DNS provider integration
- Currently not implemented

### Key Types

| Key Type | Description | Security | Performance |
|----------|-------------|----------|-------------|
| `ECDSA_P256` | ECDSA P-256 (recommended) | High | Fast |
| `ECDSA_P384` | ECDSA P-384 | Higher | Slower |
| `RSA` | RSA (configurable size) | Good | Slower |

## Directory Structure

When ACME is enabled, Stryvo creates the following directory structure:

```
./acme-certs/           # Certificate storage
├── example.com.crt     # Certificate file
├── example.com.key     # Private key file
├── api.example.com.crt
└── api.example.com.key

./acme-accounts/        # Account information
└── account.pem         # ACME account private key
```

## Certificate Lifecycle

1. **Initial Certificate Request**
   - Domain validation using HTTP-01 challenge
   - Certificate generation and storage
   - Automatic integration with proxy services

2. **Automatic Renewal**
   - Daily check for certificates nearing expiration
   - Automatic renewal 30 days before expiration (configurable)
   - Seamless certificate replacement

3. **Certificate Validation**
   - Continuous monitoring of certificate validity
   - Automatic handling of certificate errors
   - Fallback to manual certificates if needed

## Troubleshooting

### Common Issues

1. **Port 80 Not Available**
   ```
   Error: Failed to bind to address 0.0.0.0:80
   ```
   - Ensure port 80 is not used by another service
   - Check firewall settings
   - Verify Stryvo has permission to bind to port 80

2. **Domain Validation Failed**
   ```
   Error: Domain validation failed for example.com
   ```
   - Verify DNS records point to your server
   - Ensure domain is accessible from the internet
   - Check firewall allows HTTP traffic on port 80

3. **Rate Limiting**
   ```
   Error: ACME client error: rate limit exceeded
   ```
   - Use Let's Encrypt staging for testing
   - Wait for rate limit reset (usually 1 hour)
   - Consider using different domains for testing

### Debug Mode

Enable debug logging to troubleshoot ACME issues:

```yaml
system:
  logging:
    level: "DEBUG"
```

### Testing with Staging

Always test with Let's Encrypt staging environment first:

```yaml
acme:
  directory-url: "https://acme-staging-v02.api.letsencrypt.org/directory"
```

## Security Considerations

1. **Account Key Protection**
   - ACME account keys are stored in `account-dir`
   - Protect this directory with appropriate file permissions
   - Consider backing up account keys

2. **Certificate Storage**
   - Certificates are stored in `cert-dir`
   - Ensure proper file permissions (readable by Stryvo only)
   - Consider encrypted storage for sensitive environments

3. **Network Security**
   - Port 80 must be accessible for HTTP-01 challenges
   - Consider using a firewall to restrict access
   - Monitor for unauthorized certificate requests

## Best Practices

1. **Use Staging for Testing**
   - Always test configurations with Let's Encrypt staging
   - Staging has higher rate limits and doesn't count against production limits

2. **Monitor Certificate Expiration**
   - Set up monitoring for certificate renewal failures
   - Check logs regularly for ACME-related errors

3. **Backup Configuration**
   - Backup ACME account keys and certificates
   - Document your domain validation setup

4. **Plan for Failures**
   - Have manual certificates as fallback
   - Monitor ACME service availability
   - Test certificate renewal process

## Examples

See the example configuration files:
- `auto-ssl-example.yaml` - Complete YAML example
- `auto-ssl-example.kdl` - Complete KDL example
- `stryvo-config.yaml` - Full configuration with ACME section

## Limitations

Current limitations of the ACME implementation:

1. **DNS-01 Challenges**: Not yet implemented (required for wildcard certificates)
2. **Multiple ACME Providers**: Only one ACME provider per instance
3. **Certificate Algorithms**: Limited to RSA and ECDSA P-256/P-384
4. **Challenge Types**: Only HTTP-01 currently supported

These limitations will be addressed in future releases.