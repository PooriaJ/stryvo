use crate::config::internal::{ListenerConfig, ListenerKind};
use pingora::listeners::tls::TlsSettings;

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
                ssl: _,  // Legacy mode - ignore SSL flag when TLS config is present
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
                addr: _,
                tls: None,
                ssl: true,
                offer_h2: _,
            } => {
                // SSL flag mode - this will be handled by VHost service with SNI
                // For basic proxy, we need a default certificate (this is a limitation)
                panic!("SSL flag mode is only supported for VHost services. Use cert-path/key-path for basic proxy services.");
            }
            ListenerKind::Tcp {
                addr,
                tls: None,
                ssl: false,
                offer_h2,
            } => {
                if offer_h2 {
                    panic!("Unsupported configuration: {addr:?} configured without SSL, but H2 enabled which requires SSL");
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