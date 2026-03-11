pub mod auth;
pub mod commands;
pub mod context;
pub mod models;
pub mod output;
pub mod ranking;
pub mod store;
pub mod usage;
pub mod utils;

pub async fn run() -> anyhow::Result<()> {
    commands::run_cli().await
}
