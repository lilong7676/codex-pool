#[tokio::main]
async fn main() {
    if let Err(error) = codex_pool::run().await {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
