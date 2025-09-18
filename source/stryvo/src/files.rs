//! File Serving

use std::path::{Path, PathBuf};
use std::fs;

use pingora::{server::Server};
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use pingora_http::ResponseHeader;
use pingora_core::upstreams::peer::HttpPeer;

use crate::{config::internal::FileServerConfig, utils::populate_listners};

/// Create a new file serving service
pub fn stryvo_file_server(
    conf: FileServerConfig,
    server: &Server,
) -> Box<dyn pingora::services::Service> {
    let file_server = FileServer {
        base_path: conf.base_path.clone(),
    };
    let mut my_proxy =
        pingora_proxy::http_proxy_service_with_name(&server.configuration, file_server, &conf.name);

    populate_listners(conf.listeners, &mut my_proxy);

    Box::new(my_proxy)
}

pub struct FileServer {
    pub base_path: Option<PathBuf>,
}

impl FileServer {
    fn resolve_path(&self, uri_path: &str) -> Option<PathBuf> {
        let base = self.base_path.as_ref()?;
        
        // Remove leading slash and resolve relative to base path
        let clean_path = uri_path.trim_start_matches('/');
        let mut full_path = base.join(clean_path);
        
        // If path is a directory, try to serve index.html
        if full_path.is_dir() {
            full_path = full_path.join("index.html");
        }
        
        // Security check: ensure the resolved path is within base_path
        match full_path.canonicalize() {
            Ok(canonical) => {
                if canonical.starts_with(base) {
                    Some(canonical)
                } else {
                    None // Path traversal attempt
                }
            }
            Err(_) => None, // File doesn't exist or other error
        }
    }
    
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
            Err(_) => {
                self.serve_404(session).await
            }
        }
    }
    
    async fn serve_404(&self, session: &mut Session) -> Result<()> {
        let resp = ResponseHeader::build(404, None)?;
        let body = b"404 Not Found";
        
        session.write_response_header(Box::new(resp), false).await?;
        session.write_response_body(Some(body.as_slice().into()), true).await?;
        Ok(())
    }
}

/// A simple static file server implementation
#[async_trait::async_trait]
impl ProxyHttp for FileServer {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // This should never happen - we fully handle the request at the
        // `request_filter` stage, so no requests should make it to the
        // later `upstream_peer` stage.
        Err(pingora_core::Error::new_str("Request Failed"))
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        if self.base_path.is_none() {
            // No base path configured, pass through
            return Ok(false);
        }
        
        let uri_path = session.req_header().uri.path();
        
        if let Some(file_path) = self.resolve_path(uri_path) {
            self.serve_file(session, &file_path).await?;
        } else {
            self.serve_404(session).await?;
        }
        
        Ok(true) // Request handled
    }
}
