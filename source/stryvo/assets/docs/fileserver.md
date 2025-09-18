# Stryvo File System Documentation

## Overview

Stryvo's file system provides static file serving capabilities through the Pingora proxy framework. It includes secure path resolution, content type detection, and efficient file handling, with built-in security features to prevent path traversal and ensure reliable file delivery.

## Technical Specifications

### Core Components

#### 1. File Server (`FileServer`)
- **Purpose**: Handles static file serving for HTTP requests
- **Location**: `src/files.rs`
- **Key Features**:
  - Integrates with Pingora's `ProxyHttp` trait
  - Serves files from a configured base path
  - Supports directory index files (e.g., `index.html`)
  - Handles 404 responses for missing files

#### 2. Path Resolution System
- **Purpose**: Resolves URI paths to secure filesystem paths
- **Location**: `src/files.rs` (`resolve_path` method)
- **Key Features**:
  - Removes leading slashes from URI paths
  - Joins paths with the configured base path
  - Validates paths to prevent traversal attacks
  - Automatically serves `index.html` for directories

#### 3. Content Type Detection
- **Purpose**: Determines MIME types for served files
- **Location**: `src/files.rs` (`get_content_type` method)
- **Key Features**:
  - Maps file extensions to MIME types (e.g., `.html` → `text/html`)
  - Supports common web file types (HTML, CSS, JS, images)
  - Falls back to `application/octet-stream` for unknown types

## File Serving Flow

The following schematic illustrates the file serving process for HTTP requests:

```
┌──────────────┐           ┌──────────────────┐           ┌──────────────────┐
│ Client       │           │ FileServer       │           │ resolve_path     │
│ Request      │───1.────▶│ request_filter   │───2.────▶│ - Clean URI      │
└──────────────┘           │                  │           │ - Join base path │
                           └────────┬─────────┘           │ - Security check │
                                    │                     └────────┬─────────┘
                                    │ 3. Valid Path?              │
                                    │                             │
                                    ▼                             ▼
                            ┌──────────────┐           ┌──────────────────┐
                            │ serve_file    │◄───4.────│ serve_404        │
                            │ - Read file   │           │ - 404 Response   │
                            │ - Set headers │           └──────────────────┘
                            │ - Send response│
                            └───────┬──────┘
                                    │
                                    ▼
                            ┌──────────────┐
                            │ Client Response│
                            └──────────────┘
```

**Flow Steps**:
1. **Client Request**: The system receives an HTTP request with a URI path.
2. **Path Resolution**: `resolve_path` cleans the URI, joins it with the base path, and validates it for security.
3. **Path Validation**: If the path is valid and exists, proceed to serve the file; otherwise, return a 404.
4. **File Serving**: `serve_file` reads the file, sets headers (e.g., `Content-Type`), and sends the response.
5. **Response Delivery**: The client receives the file or a 404 response.

## Component Details

### File Server

**Structure**:
```rust
pub struct FileServer {
    pub base_path: Option<PathBuf>,
}
```

**Key Operations**:
- `request_filter`: Checks the base path, resolves the URI, and serves the file or a 404 response.
- `resolve_path`: Cleans and validates the file path, ensuring it stays within the base directory.
- `get_content_type`: Maps file extensions to MIME types.
- `serve_file`: Reads the file and sends it with appropriate headers.
- `serve_404`: Returns a 404 response for invalid or missing files.

### Path Resolution System

**Key Method**:
```rust
fn resolve_path(&self, uri_path: &str) -> Option<PathBuf> {
    let base = self.base_path.as_ref()?;
    let clean_path = uri_path.trim_start_matches('/');
    let mut full_path = base.join(clean_path);
    if full_path.is_dir() {
        full_path = full_path.join("index.html");
    }
    match full_path.canonicalize() {
        Ok(canonical) => {
            if canonical.starts_with(base) {
                Some(canonical)
            } else {
                None // Path traversal prevention
            }
        }
        Err(_) => None,
    }
}
```

**Features**:
- Removes leading slashes to normalize URI paths
- Appends `index.html` for directory requests
- Uses `canonicalize` to resolve symbolic links and prevent path traversal
- Ensures resolved paths stay within the base directory

### Content Type Detection

**Key Method**:
```rust
fn get_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}
```

**Features**:
- Maps common file extensions to MIME types
- Provides a default type for unknown extensions
- Ensures compatibility with web browsers

## File Serving Implementation

**Key Method**:
```rust
async fn serve_file(&self, session: &mut Session, file_path: &Path) -> Result<()> {
    match fs::read(file_path) {
        Ok(content) => {
            let mut resp = ResponseHeader::build(200, None)?;
            resp.insert_header("content-type", Self::get_content_type(file_path))?;
            resp.insert_header("content-length", content.len().to_string())?;
            session.write_response_header(Box::new(resp), false).await?;
            session.write_response_body(Some(content.into()), true).await?;
            Ok(())
        }
        Err(_) => self.serve_404(session).await
    }
}
```

**Features**:
- Reads file content synchronously for simplicity
- Sets `Content-Type` and `Content-Length` headers
- Sends the response asynchronously via Pingora's `Session`
- Falls back to `serve_404` on read errors

## Configuration

### Example Configuration
```yaml
file_server:
  name: "static-files"
  base_path: "/var/www/html"
  listeners:
    - address: "0.0.0.0:8080"
```

**Notes**:
- `base_path`: Specifies the root directory for file serving (required)
- `name`: Identifies the file server service
- `listeners`: Defines network addresses and ports for the service

### Service Creation
```rust
pub fn stryvo_file_server(
    conf: FileServerConfig,
    server: &Server,
) -> Box<dyn pingora::services::Service> {
    let file_server = FileServer {
        base_path: conf.base_path.clone(),
    };
    let mut my_proxy = pingora_proxy::http_proxy_service_with_name(
        &server.configuration, 
        file_server, 
        &conf.name
    );
    populate_listners(conf.listeners, &mut my_proxy);
    Box::new(my_proxy)
}
```

**Features**:
- Creates a Pingora proxy service with the `FileServer` instance
- Configures listeners based on the provided configuration
- Integrates with the Pingora framework for HTTP handling

## Performance Optimization

### File Reading
- Uses synchronous `fs::read` for simplicity, suitable for small to medium files
- Sets `Content-Length` header for HTTP compliance
- Avoids unnecessary file operations by validating paths early

### Content Type Detection
- Employs fast, extension-based MIME type detection
- Minimizes overhead with static string returns
- Covers common web file types for broad compatibility

## Monitoring and Debugging

### Logging
- Leverages Pingora's proxy logging for file access attempts
- Logs 404 errors for missing files or invalid paths
- Reports path traversal attempts as `None` from `resolve_path`

### Common Issues
1. **404 Errors**:
   - **Cause**: File does not exist or path is invalid
   - **Solution**: Verify file existence and `base_path` configuration
2. **Permission Errors**:
   - **Cause**: Insufficient read permissions
   - **Solution**: Ensure the process has access to the base directory
3. **Path Traversal Attempts**:
   - **Cause**: Malicious URI paths
   - **Solution**: Rely on built-in canonicalization checks

### Debugging Tips
- Check `base_path` for correctness and absoluteness
- Verify file permissions for the base directory and files
- Test with simple files before serving complex directory structures
- Monitor proxy logs for file access and error details

## Best Practices

### Security
- Always set a valid `base_path` to restrict file access
- Rely on `resolve_path` to prevent path traversal
- Handle file read errors gracefully with 404 responses

### Performance
- Use for static assets (e.g., HTML, CSS, JS, images)
- Organize files logically under the base path
- Leverage `index.html` for clean directory URLs

### Configuration
- Set `base_path` to the web root directory
- Configure listeners for appropriate ports
- Use descriptive service names for clarity

## Limitations
- Synchronous file reading may not scale for very large files
- No support for range requests or partial file serving
- Limited MIME type mappings compared to advanced servers
- No built-in caching or compression (handled separately)

This documentation reflects the current implementation of Stryvo's file system as of the provided code. For further details on integration or extension, refer to the source code in `src/files.rs`.