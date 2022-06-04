use clap::{Parser, Subcommand};
use library::test2::Config;

#[derive(Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Serve,
    Test {
        server: String,
        #[clap(long)]
        download: bool,
        #[clap(long)]
        upload: bool,
        #[clap(long)]
        both: bool,
        #[clap(long)]
        bandwidth_sample_rate: Option<u64>,
    },
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        &Commands::Test {
            ref server,
            download,
            upload,
            both,
            bandwidth_sample_rate,
        } => {
            let mut config = Config {
                download: true,
                upload: true,
                both: true,
                bandwidth_interval: bandwidth_sample_rate.unwrap_or(20),
            };

            if download || upload || both {
                config.download = download;
                config.upload = upload;
                config.both = both;
            }

            library::test2::test(config, &server);
        }
        Commands::Serve => {
            library::serve2::serve();
        }
    }
}
