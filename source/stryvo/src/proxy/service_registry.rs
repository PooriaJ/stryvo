//! Service registry for background services
//!
//! This module provides a registry for background services that need to run
//! continuously, such as cache persistence.

use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tracing::{error, info};

/// A service that can be registered and managed by the service registry
pub trait BackgroundService: Send + Sync + 'static {
    /// Start the service
    fn start(&self) -> JoinHandle<()>;

    /// Get the name of the service
    fn name(&self) -> &str;
}

/// Registry for background services
pub struct ServiceRegistry {
    services: Mutex<Vec<(String, JoinHandle<()>)>>,
}

impl ServiceRegistry {
    /// Create a new service registry
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            services: Mutex::new(Vec::new()),
        })
    }

    /// Register a background service
    pub fn register<S: BackgroundService>(&self, service: &S) {
        let name = service.name().to_string();

        // Ensure we're in a Tokio runtime context
        let handle_result = if let Ok(_) = tokio::runtime::Handle::try_current() {
            // We're already in a Tokio runtime, use it
            info!("Using existing Tokio runtime for service: {}", name);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| service.start()))
        } else {
            // No runtime context, create a new one
            info!("Creating new Tokio runtime for service: {}", name);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create Tokio runtime");

            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                rt.block_on(async { service.start() })
            }))
        };

        // Process the result
        match handle_result {
            Ok(handle) => {
                info!("Registered background service: {}", name);
                self.services.lock().unwrap().push((name, handle));
            }
            Err(e) => {
                // Log the error but don't panic
                error!("Failed to start background service {}: {:?}", name, e);
                info!("Service {} will be disabled", name);
            }
        }
    }
}

impl Drop for ServiceRegistry {
    fn drop(&mut self) {
        let services = self.services.lock().unwrap();
        for (name, handle) in services.iter() {
            info!("Aborting service on shutdown: {}", name);
            handle.abort();
        }
    }
}