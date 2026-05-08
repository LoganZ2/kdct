use clap::Parser;

#[derive(Parser, Debug, Default, Clone)]
#[clap(
    about,
    version(env!("CARGO_PKG_VERSION")),
)]
pub struct Cli {
    /// The path to the configuration file
    #[arg(name = "CONFIG")]
    pub config_path: Option<std::path::PathBuf>,

    /// Run as a server
    #[arg(long, short)]
    pub server: bool,

    /// Run as a client
    #[arg(long, short)]
    pub client: bool,
}
