
# Stryvo - Reverse Proxy Application

Stryvo is a high-performance reverse proxy application built on the [Pingora](https://github.com/cloudflare/pingora) reverse proxy engine from Cloudflare. Written in Rust, Stryvo offers a robust, configurable, and secure solution for routing, filtering, and modifying proxied requests. Forked from the [River](https://github.com/memorysafety/river) project, Stryvo extends its capabilities with additional features like caching, compression, enhanced logging, Nginx-style configuration parsing, and Docker support.

## Features

- **High Performance**: Leverages **Pingora** for exceptional speed and efficiency.
- **Flexible Configuration**: Supports KDL, TOML, and Nginx-style configuration formats.
- **TLS Support**: Full TLS termination for secure HTTP/1.x and HTTP/2 connections.
- **Load Balancing**: Implements Round Robin, Random, FNV, and Ketama strategies.
- **Rate Limiting**: Configurable limits based on source IP, URIs, or URI patterns.
- **Path Control**: Advanced request and response filtering and modification.
- **File Serving**: Static file serving for web content delivery.
- **Hot Reloading**: Reload configurations without service interruption.
- **Caching**: Proxy-side and browser caching to reduce latency and server load.
- **Compression**: Supports Gzip, Brotli, and Zstd for efficient data transfer.
- **Logging**: Comprehensive access and error logging for monitoring and debugging.
- **Nginx Parser**: Use familiar Nginx-style syntax for configuration.
- **Docker Support**: Includes Dockerfile and docker-compose for easy deployment.

## Current State

Stryvo is at version 0.6.0. Refer to the [v0.6.0 release notes](https://github.com/PooriaJ/stryvo/blob/main/docs/release-notes/2025-04-26-v0.5.0.md) for details on recent updates.

**Note**: The project is under active development, and configuration formats or features may change. Stability is not guaranteed until further notice.

## Installation

### Prerequisites

- **Rust**: Version 1.86 or higher for building from source.
- **Docker**: Required for containerized deployments.
- **System Dependencies**: `pkg-config`, `libssl-dev`, `cmake`, `perl`, and `ca-certificates` for building and running.

### Pre-compiled Binaries

Pre-compiled binaries are available on the [Releases](https://github.com/PooriaJ/stryvo/releases) page for:

- x86-64 Linux (GNU libc)
- x86-64 Linux (MUSL libc)
- aarch64 macOS (M-series devices)

The primary target is **x86-64 Linux (GNU libc)**. Other platforms are supported on a best-effort basis and may lack some features.

### Building from Source

1. Clone the repository:
   ```bash
   git clone https://github.com/PooriaJ/stryvo.git
   cd stryvo
   ```

2. Build the application:
   ```bash
   cargo build --release
   ```

3. Run the binary:
   ```bash
   ./target/release/stryvo --config-kdl ./source/stryvo/assets/test-config.kdl
   ```

### Docker Deployment

Stryvo provides a Dockerfile and docker-compose.yml for containerized deployments.

#### Using docker-compose (Recommended)

1. Clone the repository:
   ```bash
   git clone https://github.com/PooriaJ/stryvo.git
   cd stryvo
   ```

2. Customize the configuration file in `source/stryvo/assets/` (e.g., `test-config.kdl`).

3. Start the container:
   ```bash
   docker-compose up -d
   ```

This maps ports 80 and 443, mounts configuration and log directories, and runs Stryvo with the specified configuration.

#### Using Docker Directly

1. Build the Docker image:
   ```bash
   docker build -t stryvo-proxy .
   ```

2. Run the container:
   ```bash
   docker run -d \
     --name stryvo-proxy \
     -p 80:80 \
     -p 443:443 \
     -v $(pwd)/source/stryvo/assets:/app/assets \
     -v $(pwd)/logs:/app/logs \
     stryvo-proxy
   ```

3. To use a custom configuration:
   ```bash
   docker run -d \
     --name stryvo-proxy \
     -p 80:80 \
     -p 443:443 \
     -v $(pwd)/source/stryvo/assets:/app/assets \
     -v $(pwd)/logs:/app/logs \
     stryvo-proxy --config-kdl /app/assets/your-config.kdl
   ```

## Configuration

Stryvo supports three configuration formats: KDL, TOML, and Nginx-style syntax. KDL is the recommended format for its clarity and flexibility.

### KDL Configuration

KDL is a structured, human-readable format. Below is an example configuration:

```kdl
system {
    threads-per-service 8
    daemonize false
    pid-file "/tmp/stryvo.pidfile"
    logging {
        access-log "stryvo-access.log"
        error-log "stryvo-error.log"
        level "INFO"
        enabled true
    }
}

services {
    Example1 {
        listeners {
            "0.0.0.0:8080"
            "0.0.0.0:4443" cert-path="./assets/test.crt" key-path="./assets/test.key" offer-h2=true
        }
        connectors {
            load-balance {
                selection "Ketama" key="UriPath"
                discovery "Static health-check "None"
            }
            "91.107.223.4:443" tls-sni="stryvo.ir" proto="h2-or-h1"
        }
        rate-limiting {
            rule kind="source-ip" max-buckets=4000 tokens-per-bucket=10 refill-qty=1 refill-rate-ms=10
            rule kind="specific-uri" pattern="static/.*" max-buckets=2000 tokens-per-bucket=20 refill-qty=5 refill-rate-ms=1
            rule kind="any-matching-uri" pattern=r".*\.mp4" tokens-per-bucket=50 refill-qty=2 refill-rate-ms=3
        }
        path-control {
            request-filters {
                filter kind="block-cidr-range" addrs="192.168.0.0/16, 10.0.0.0/8, 2001:0db8::0/32"
            }
            upstream-request {
                filter kind="remove-header-key-regex" pattern=".*(secret|SECRET).*"
                filter kind="upsert-header" key="x-proxy-friend" value="stryvo"
            }
            upstream-response {
                filter kind="remove-header-key-regex" pattern=".*ETag.*"
                filter kind="upsert-header" key="x-with-love-from" value="stryvo"
            }
        }
        cache {
            cache-enabled true
            cache-ttl 10
            shard-count 32
            cache-dir "/tmp/test/stryvo-cache"
            max-file-size 1048576
            browser_cache_enabled false
            browser_cache_ttl 3600
        }
    }
}
```

For detailed KDL configuration options, see the [KDL Configuration Documentation](https://stryvo.ir/stryvo-user-manual/config/kdl.html).

### TOML Configuration

TOML is supported for structured configuration. Example:

```toml
[system]
threads-per-service = 8

[[basic-proxy]]
name = "Example1"
[[basic-proxy.listeners]]
[basic-proxy.listeners.source]
kind = "Tcp"
[basic-proxy.listeners.source.value]
addr = "0.0.0.0:8080"
```

### Nginx-Style Configuration

Stryvo parses Nginx-like syntax for users familiar with Nginx. Example:

```nginx
system {
    threads_per_service 8;
    daemonize false;
    pid_file "/tmp/stryvo.pidfile";
}

http {
    server {
        server_name Example1;
        listen 0.0.0.0:8680;
        location / {
            proxy_pass backend_onevariable;
            deny 192.168.0.0/16;
            add_header x-with-love-from "stryvo";
        }
    }
}
```

### Command Line Options

Run Stryvo with various options:

```bash
stryvo --config-kdl /path/to/config.kdl
stryvo --config-toml /path/to/config.toml
stryvo --config-nginx /path/to/config.nginx
```

Use `stryvo --help` for a full list of options.

## Usage Examples

### Basic Reverse Proxy

Forward requests to a single backend:

```kdl
services {
    BasicProxy {
        listeners {
            "0.0.0.0:8000"
        }
        connectors {
            "backend.example.com:80"
        }
    }
}
```

### TLS Termination

Handle HTTPS traffic:

```kdl
services {
    TlsProxy {
        listeners {
            "0.0.0.0:8443" cert-path="/path/to/cert.crt" key-path="/path/to/key.key" offer-h2=true
        }
        connectors {
            "backend.example.com:80"
        }
    }
}
```

### Load Balancing

Distribute requests across multiple backends:

```kdl
services {
    LoadBalancedProxy {
        listeners {
            "0.0.0.0:8000"
        }
        connectors {
            load-balance {
                selection "RoundRobin"
                discovery "Static"
                health-check "None"
            }
            "backend1.example.com:80"
            "backend2.example.com:80"
        }
    }
}
```

### File Serving

Serve static files:

```kdl
services {
    FileServer {
        listeners {
            "0.0.0.0:9000"
        }
        file-server {
            base-path "./static"
        }
    }
}
```

### Caching

Enable proxy caching:

```kdl
services {
    CachedProxy {
        listeners {
            "0.0.0.0:8000"
        }
        connectors {
            "backend.example.com:80"
        }
        cache {
            cache-enabled true
            cache-ttl 300
            cache-dir "/tmp/stryvo-cache"
            max-file-size 1048576
        }
    }
}
```

### Compression

Enable response compression:

```kdl
services {
    CompressedProxy {
        listeners {
            "0.0.0.0:8000"
        }
        connectors {
            "backend.example.com:80"
        }
    }
}
```

## Logging

Stryvo logs access and error events to configurable files:

```kdl
system {
    logging {
        access-log "logs/stryvo-access.log"
        error-log "logs/stryvo-error.log"
        level "INFO"
        enabled true
    }
}
```

Logs are stored in the `/app/logs` directory when using Docker, mapped to `./logs` on the host.

## Documentation

- [User Manual](https://stryvo.ir/stryvo-user-manual/)
- [GitHub Repository](https://github.com/PooriaJ/stryvo)
- [KDL Configuration Guide](https://stryvo.ir/stryvo-user-manual/config/kdl.html)

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>).

### Contribution

Contributions are welcome! Unless explicitly stated otherwise, contributions are licensed under the Apache-2.0 license without additional terms.

## Fork Information

Stryvo is forked from the [River](https://github.com/memorysafety/river) project, extending its functionality with caching, compression, logging, Nginx-style configuration, and Docker support.
