use anyhow::Result;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_app_server_sdk::AppServerSdk;
use codex_app_server_sdk::AppServerSdkMessage;
use codex_app_server_sdk::InProcessAppServerOptions;

fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let message = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Summarize this repository in one sentence.".to_string());

    let mut sdk = AppServerSdk::new(InProcessAppServerOptions::default()).await?;
    let _initialize = sdk.initialize().await?;

    let thread = sdk.thread_start(ThreadStartParams::default()).await?;
    let turn = sdk
        .turn_start(TurnStartParams {
            thread_id: thread.thread.id.clone(),
            input: vec![UserInput::Text {
                text: message,
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;

    loop {
        match sdk.next_message().await? {
            AppServerSdkMessage::Notification(ServerNotification::AgentMessageDelta(delta)) => {
                print!("{}", delta.delta);
            }
            AppServerSdkMessage::Notification(ServerNotification::ItemCompleted(item)) => {
                if let ThreadItem::AgentMessage { text, .. } = item.item {
                    println!("\n\n[agent message item]\n{text}");
                }
            }
            AppServerSdkMessage::Notification(ServerNotification::TurnCompleted(completed)) => {
                println!("\n[turn status: {:?}]", completed.turn.status);
                if completed.turn.id == turn.turn.id
                    && completed.turn.status != TurnStatus::InProgress
                {
                    break;
                }
            }
            AppServerSdkMessage::Request(request) => {
                eprintln!("server requested client action: {request:?}");
            }
            AppServerSdkMessage::Notification(_) | AppServerSdkMessage::RawNotification(_) => {}
        }
    }

    sdk.shutdown().await?;
    Ok(())
}
