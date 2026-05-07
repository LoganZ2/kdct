pub mod cli;
pub mod config;
pub mod constants;
pub mod helper;
pub mod multi_map;
pub mod node_update;
pub mod port_pool;
pub mod protocol;
pub mod registry;
pub mod transport;

pub use cli::Cli;
pub use config::Config;
pub use constants::UDP_BUFFER_SIZE;

use anyhow::Result;
use tokio::sync::broadcast;
use tracing::debug;

#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub use client::run_client;

#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub use server::run_server;

pub async fn run(args: Cli, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    let config_path = args.config_path.as_ref().unwrap();
    let config = Config::from_file(config_path).await?;

    // Raise `nofile` limit on linux and mac
    fdlimit::raise_fd_limit();

    debug!("{:?}", config);

    run_instance(config, args, shutdown_rx).await
}

async fn run_instance(
    config: Config,
    args: Cli,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    match determine_run_mode(&config, &args) {
        RunMode::Undetermine => panic!("Cannot determine running as a server or a client"),
        RunMode::Client => {
            #[cfg(not(feature = "client"))]
            panic!("The feature 'client' is not compiled in this binary. Please rebuild the tunnel crate with the client feature.");
            #[cfg(feature = "client")]
            run_client(config, shutdown_rx).await
        }
        RunMode::Server => {
            #[cfg(not(feature = "server"))]
            panic!("The feature 'server' is not compiled in this binary. Please rebuild the tunnel crate with the server feature.");
            #[cfg(feature = "server")]
            run_server(config, shutdown_rx).await
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum RunMode {
    Server,
    Client,
    Undetermine,
}

fn determine_run_mode(config: &Config, args: &Cli) -> RunMode {
    use RunMode::*;
    if args.client && args.server {
        Undetermine
    } else if args.client {
        Client
    } else if args.server {
        Server
    } else if config.client.is_some() && config.server.is_none() {
        Client
    } else if config.server.is_some() && config.client.is_none() {
        Server
    } else {
        Undetermine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_run_mode() {
        use config::*;
        use RunMode::*;

        struct T {
            cfg_s: bool,
            cfg_c: bool,
            arg_s: bool,
            arg_c: bool,
            run_mode: RunMode,
        }

        let tests = [
            T {
                cfg_s: false,
                cfg_c: false,
                arg_s: false,
                arg_c: false,
                run_mode: Undetermine,
            },
            T {
                cfg_s: true,
                cfg_c: false,
                arg_s: false,
                arg_c: false,
                run_mode: Server,
            },
            T {
                cfg_s: false,
                cfg_c: true,
                arg_s: false,
                arg_c: false,
                run_mode: Client,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: false,
                arg_c: false,
                run_mode: Undetermine,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: true,
                arg_c: false,
                run_mode: Server,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: false,
                arg_c: true,
                run_mode: Client,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: true,
                arg_c: true,
                run_mode: Undetermine,
            },
        ];

        for t in tests {
            let config = Config {
                server: match t.cfg_s {
                    true => Some(ServerConfig::default()),
                    false => None,
                },
                client: match t.cfg_c {
                    true => Some(ClientConfig::default()),
                    false => None,
                },
            };

            let args = Cli {
                config_path: Some(std::path::PathBuf::new()),
                server: t.arg_s,
                client: t.arg_c,
                ..Default::default()
            };

            assert_eq!(determine_run_mode(&config, &args), t.run_mode);
        }
    }
}
