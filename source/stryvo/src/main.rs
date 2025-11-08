mod acme;
mod config;
mod files;
mod logging;
mod proxy;
mod utils;

use crate::{files::stryvo_file_server, proxy::{stryvo_proxy_service, stryvo_vhost_service}};
use pingora::{server::Server, services::Service};
use tokio::runtime::Runtime;
use tracing::{error, info};

fn main() {
    // Read from the various configuration files
    let conf = config::render_config();

    // Set up tracing with our custom logging configuration
    if let Err(e) = logging::init_logging(&conf.logging) {
        eprintln!("Failed to initialize logging: {}", e);
    }

    // Start the Server, which we will add services to.
    let mut my_server =
        Server::new_with_opt_and_conf(conf.pingora_opt(), conf.pingora_server_conf());

    tracing::info!("Applying Basic Proxies...");
    let mut services: Vec<Box<dyn Service>> = vec![];

    // Create a Tokio runtime for async operations
    let rt = Runtime::new().expect("Failed to create Tokio runtime");

    // At the moment, we only support basic proxy services. These have some path
    // control, but don't support things like load balancing, health checks, etc.
    for beep in conf.basic_proxies {
        tracing::info!("Configuring Basic Proxy: {}", beep.name);
        let (service, persistence) = stryvo_proxy_service(beep, &my_server);

        // Load cache metadata for the proxy service if it has a cache
        if let Some(persistence) = persistence {
            rt.block_on(async {
                if let Err(e) = persistence.load().await {
                    error!("Failed to load cache metadata for proxy {}: {}", service.name(), e);
                } else {
                    info!("Successfully loaded cache metadata for proxy {}", service.name());
                }
            });
        }

        services.push(service);
    }

    // Configure VHost proxy services with SNI certificate selection
    for vhost_conf in conf.vhost_proxies {
        tracing::info!("Configuring VHost Proxy: {}", vhost_conf.name);
        let service = stryvo_vhost_service(vhost_conf, Some(conf.acme.clone()), &my_server);
        services.push(service);
    }

    for fs in conf.file_servers {
        tracing::info!("Configuring File Server: {}", fs.name);
        let service = stryvo_file_server(fs, &my_server);
        services.push(service);
    }

    // Now we hand it over to pingora to run forever.
    tracing::info!("Bootstrapping...");
    my_server.bootstrap();
    tracing::info!("Bootstrapped. Adding Services...");
    my_server.add_services(services);
    tracing::info!("Starting Server...");
    my_server.run_forever();
}