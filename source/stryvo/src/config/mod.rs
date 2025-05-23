pub mod cli;
pub mod internal;
pub mod kdl;
pub mod nginx;
pub mod toml;

use std::fs::read_to_string;

use clap::Parser;
use cli::Cli;

use crate::config::nginx::Nginx;
use crate::config::toml::Toml;

pub fn render_config() -> internal::Config {
    // To begin with, start with the blank internal config. We will layer on top of that.
    let mut config = internal::Config::default();

    // Then, obtain the command line information, as that may
    // change the paths to look for configuration files. It also handles
    // bailing immediately if the user passes `--help`.
    tracing::info!("Parsing CLI options");
    let c = Cli::parse();
    tracing::info!(
        config = ?c,
        "CLI config"
    );

    let toml_opts = c.config_toml.as_ref().map(Toml::from_path);

    let nginx_opts = c.config_nginx.as_ref().map(Nginx::from_path);

    let kdl_opts = c.config_kdl.as_ref().map(|kdl_path| {
        let kdl_contents = read_to_string(kdl_path).unwrap_or_else(|e| {
            panic!("Error loading KDL file: {e:?}");
        });
        let doc: ::kdl::KdlDocument = kdl_contents.parse().unwrap_or_else(|e| {
            panic!("Error parsing KDL file: {e:?}");
        });
        let val: internal::Config = doc.try_into().unwrap_or_else(|e| {
            panic!("Error rendering config from KDL file: {e:?}");
        });
        val
    });

    // 2.6.7: stryvo MUST give the following priority to configuration:
    //   1. Command Line Options (highest priority)
    //   2. Environment Variable Options
    //   3. Configuration File Options (lowest priority)
    //
    // Apply in reverse order as we are layering.
    match (toml_opts, kdl_opts, nginx_opts) {
        (Some(tf), None, None) => {
            tracing::info!("Applying TOML options");
            apply_toml(&mut config, &tf);
        }
        (None, Some(kf), None) => {
            tracing::info!("Applying KDL options");
            config = kf;
        }
        (None, None, Some(nf)) => {
            tracing::info!("Applying Nginx-like options");
            config = nf
                .unwrap_or_else(|e| {
                    panic!("Error loading Nginx config: {e:?}");
                })
                .into();
        }
        (None, None, None) => {
            tracing::info!("No configuration file provided");
        }
        _ => {
            tracing::error!("Refusing to merge multiple configuration formats: Please choose one.");
            panic!("Too many configuration options selected!");
        }
    }

    tracing::info!("Applying CLI options");
    apply_cli(&mut config, &c);

    // We always validate the configuration - if the user selected "validate"
    // then pingora will exit when IT also validates the config.
    tracing::info!(?config, "Full configuration",);
    tracing::info!("Validating...");
    config.validate();
    tracing::info!("Validation complete");
    config
}

fn apply_cli(conf: &mut internal::Config, cli: &Cli) {
    let Cli {
        validate_configs,
        threads_per_service,
        config_toml: _,
        config_kdl: _,
        config_nginx: _,
        daemonize,
        upgrade,
        pidfile,
        upgrade_socket,
    } = cli;

    conf.validate_configs |= validate_configs;
    conf.daemonize |= daemonize;
    conf.upgrade |= upgrade;

    if let Some(pidfile) = pidfile {
        if let Some(current_pidfile) = conf.pid_file.as_ref() {
            if pidfile != current_pidfile {
                panic!(
                    "Mismatched commanded PID files. CLI: {pidfile:?}, Config: {current_pidfile:?}"
                );
            }
        }
        conf.pid_file = Some(pidfile.into());
    }

    if let Some(upgrade_socket) = upgrade_socket {
        if let Some(current_upgrade_socket) = conf.upgrade_socket.as_ref() {
            if upgrade_socket != current_upgrade_socket {
                panic!(
                    "Mismatched commanded upgrade sockets. CLI: {upgrade_socket:?}, Config: {current_upgrade_socket:?}"
                );
            }
        }
        conf.upgrade_socket = Some(upgrade_socket.into());
    }

    if let Some(tps) = threads_per_service {
        conf.threads_per_service = *tps;
    }
}

fn apply_toml(conf: &mut internal::Config, toml: &Toml) {
    let Toml {
        system,
        basic_proxy,
    } = toml;

    let basic_proxy: Vec<internal::ProxyConfig> =
        basic_proxy.iter().cloned().map(Into::into).collect();

    // As toml is a configuration file, it should SET the value. We have to later consider
    // if we EXTEND or REPLACE when used with more config file formats, or allow for setting
    // of proxies in env/cli options.
    assert!(
        conf.basic_proxies.is_empty(),
        "Non-empty 'basic proxies' list when applying TOML settings. This is unexpected."
    );
    conf.basic_proxies = basic_proxy;

    conf.threads_per_service = system.threads_per_service;
}
