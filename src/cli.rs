use std::net::SocketAddr;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about = "Offline emulator for the Braintrust API")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the emulator HTTP server.
    Serve(ServeArgs),
}

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Address to listen on.
    #[arg(long, env = "BT_EMULATOR_LISTEN", default_value = "127.0.0.1:8000")]
    pub listen: SocketAddr,
    /// Database URL: `sqlite://path.db`, `sqlite::memory:` or `postgres://...`.
    #[arg(
        long,
        env = "BT_EMULATOR_DATABASE_URL",
        default_value = "sqlite://bt-emulator.db"
    )]
    pub database_url: String,
    /// Only accept this API key (default: accept any bearer token).
    #[arg(long, env = "BT_EMULATOR_API_KEY")]
    pub api_key: Option<String>,
    /// OpenAI-compatible API that prompt functions run against when invoked
    /// (e.g. `http://localhost:11434/v1`). Without it, invoking a prompt fails.
    #[arg(long, env = "BT_EMULATOR_LLM_BASE_URL")]
    pub llm_base_url: Option<String>,
    /// API key sent to `--llm-base-url`.
    #[arg(long, env = "BT_EMULATOR_LLM_API_KEY")]
    pub llm_api_key: Option<String>,
    /// Run inline code functions in local `node`/`python3` processes when invoked.
    /// Anyone who can reach the emulator can then run code on this machine.
    #[arg(long, env = "BT_EMULATOR_ALLOW_CODE_EXECUTION")]
    pub allow_code_execution: bool,
}

pub async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

pub async fn run(cli: Cli) -> Result<(), crate::error::Error> {
    let Command::Serve(args) = cli.command;
    let db = crate::db::Db::connect(&args.database_url).await?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    println!("listening on http://{}", listener.local_addr()?);
    crate::server::serve(
        listener,
        db,
        crate::api::Config {
            api_key: args.api_key,
            runtime: crate::invoke::Runtime {
                llm_base_url: args.llm_base_url,
                llm_api_key: args.llm_api_key,
                allow_code: args.allow_code_execution,
                ..Default::default()
            },
        },
        crate::server::shutdown_signal(),
    )
    .await?;
    Ok(())
}
