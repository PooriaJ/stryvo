mod config;
mod files;
mod logging;
mod proxy;

use crate::{files::stryvo_file_server, proxy::stryvo_proxy_service};
use config::internal::{ListenerConfig, ListenerKind};
use pingora::{server::Server, services::Service};
use pingora_core::listeners::TlsSettings;
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

pub fn populate_listners<T>(
    listeners: Vec<ListenerConfig>,
    service: &mut pingora_core::services::listening::Service<T>,
) {
    for list_cfg in listeners {
        // NOTE: See https://github.com/cloudflare/pingora/issues/182 for tracking "paths aren't
        // always UTF-8 strings".
        //
        // See also https://github.com/cloudflare/pingora/issues/183 for tracking "ip addrs shouldn't
        // be strings"
        match list_cfg.source {
            ListenerKind::Tcp {
                addr,
                tls: Some(tls_cfg),
                offer_h2,
            } => {
                let cert_path = tls_cfg
                    .cert_path
                    .to_str()
                    .expect("cert path should be utf8");
                let key_path = tls_cfg.key_path.to_str().expect("key path should be utf8");

                // TODO: Make conditional!
                let mut settings = TlsSettings::intermediate(cert_path, key_path)
                    .expect("adding TLS listener shouldn't fail");
                if offer_h2 {
                    settings.enable_h2();
                }

                service.add_tls_with_settings(&addr, None, settings);
            }
            ListenerKind::Tcp {
                addr,
                tls: None,
                offer_h2,
            } => {
                if offer_h2 {
                    panic!("Unsupported configuration: {addr:?} configured without TLS, but H2 enabled which requires TLS");
                }
                service.add_tcp(&addr);
            }
            ListenerKind::Uds(path) => {
                let path = path.to_str().unwrap();
                service.add_uds(path, None); // todo
            }
        }
    }
}