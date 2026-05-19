use anyhow::Result;
use clap::Parser;
use codex_mock_chatgpt_account_server::server::MockServerArgs;
use codex_mock_chatgpt_account_server::server::run;

#[tokio::main]
async fn main() -> Result<()> {
    run(MockServerArgs::parse()).await
}
