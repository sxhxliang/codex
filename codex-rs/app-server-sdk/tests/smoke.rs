use anyhow::Context;
use anyhow::Result;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_app_server_sdk::AppServerSdk;
use codex_app_server_sdk::AppServerSdkMessage;
use codex_app_server_sdk::InProcessAppServerOptions;
use codex_core::features::Feature;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
async fn sdk_can_run_a_turn_in_process() -> Result<()> {
    let mock_server = create_mock_responses_server_repeating_assistant("hello from sdk").await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &mock_server.uri(),
        &BTreeMap::<Feature, bool>::new(),
        32_000,
        Some(false),
        "mock_provider",
        "compact",
    )?;

    let mut sdk = AppServerSdk::new(InProcessAppServerOptions {
        codex_home: Some(codex_home.path().to_path_buf()),
        ..Default::default()
    })
    .await?;
    let _initialize = timeout(std::time::Duration::from_secs(10), sdk.initialize())
        .await
        .context("initialize timed out")??;

    let thread = timeout(
        std::time::Duration::from_secs(10),
        sdk.thread_start(ThreadStartParams::default()),
    )
    .await
    .context("thread_start timed out")??;
    let turn = sdk.turn_start(TurnStartParams {
        thread_id: thread.thread.id.clone(),
        input: vec![UserInput::Text {
            text: "say hello".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    });
    let turn = timeout(std::time::Duration::from_secs(10), turn)
        .await
        .context("turn_start timed out")??;

    let mut final_message = None;
    timeout(std::time::Duration::from_secs(10), async {
        loop {
            let message = sdk.next_message().await?;
            match message {
                AppServerSdkMessage::Notification(ServerNotification::ItemCompleted(item)) => {
                    if let ThreadItem::AgentMessage { text, .. } = item.item {
                        final_message = Some(text);
                    }
                }
                AppServerSdkMessage::Notification(ServerNotification::TurnCompleted(completed)) => {
                    if completed.turn.id == turn.turn.id {
                        break;
                    }
                }
                AppServerSdkMessage::Request(request) => {
                    panic!("unexpected server request: {request:?}");
                }
                AppServerSdkMessage::Notification(_) | AppServerSdkMessage::RawNotification(_) => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("waiting for turn completion timed out")??;

    assert_eq!(final_message, Some("hello from sdk".to_string()));
    sdk.shutdown().await?;
    Ok(())
}
