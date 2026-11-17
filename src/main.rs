#[tokio::main]
async fn main() -> std::process::ExitCode {
    bt_emulator::cli::main().await
}
