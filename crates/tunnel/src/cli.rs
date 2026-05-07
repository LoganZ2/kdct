use clap::Parser;

#[derive(Parser, Debug, Default, Clone)]
#[clap(
    about,
    version(env!("CARGO_PKG_VERSION")),
)]
pub struct Cli {
    /// The path to the configuration file
    #[clap(parse(from_os_str), name = "CONFIG")]
    pub config_path: Option<std::path::PathBuf>,

    /// Run as a server
    #[clap(long, short)]
    pub server: bool,

    /// Run as a client
    #[clap(long, short)]
    pub client: bool,
}
