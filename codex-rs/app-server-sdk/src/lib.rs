use anyhow::Context;
use anyhow::Result;
use codex_app_server::InProcessAppServer;
pub use codex_app_server::InProcessAppServerOptions;
pub use codex_app_server_protocol as protocol;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::VecDeque;

#[derive(Debug)]
pub enum AppServerSdkMessage {
    Request(ServerRequest),
    Notification(ServerNotification),
    RawNotification(JSONRPCNotification),
}

pub struct AppServerSdk {
    server: InProcessAppServer,
    next_request_id: i64,
    pending_messages: VecDeque<JSONRPCMessage>,
}

impl AppServerSdk {
    pub async fn new(options: InProcessAppServerOptions) -> Result<Self> {
        Ok(Self {
            server: InProcessAppServer::new(options).await?,
            next_request_id: 0,
            pending_messages: VecDeque::new(),
        })
    }

    pub async fn initialize(&mut self) -> Result<InitializeResponse> {
        self.initialize_with_params(InitializeParams {
            client_info: ClientInfo {
                name: "codex-app-server-sdk".to_string(),
                title: Some("Codex App Server SDK".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            capabilities: Some(InitializeCapabilities {
                experimental_api: true,
                ..Default::default()
            }),
        })
        .await
    }

    pub async fn initialize_with_params(
        &mut self,
        params: InitializeParams,
    ) -> Result<InitializeResponse> {
        let request_id = self.next_request_id();
        let response = self
            .send_request(
                ClientRequest::Initialize {
                    request_id: request_id.clone(),
                    params,
                },
                request_id,
                "initialize",
            )
            .await?;
        self.send_notification(ClientNotification::Initialized)
            .await?;
        Ok(response)
    }

    pub async fn model_list(&mut self, params: ModelListParams) -> Result<ModelListResponse> {
        let request_id = self.next_request_id();
        self.send_request(
            ClientRequest::ModelList {
                request_id: request_id.clone(),
                params,
            },
            request_id,
            "model/list",
        )
        .await
    }

    pub async fn thread_start(&mut self, params: ThreadStartParams) -> Result<ThreadStartResponse> {
        let request_id = self.next_request_id();
        self.send_request(
            ClientRequest::ThreadStart {
                request_id: request_id.clone(),
                params,
            },
            request_id,
            "thread/start",
        )
        .await
    }

    pub async fn thread_resume(
        &mut self,
        params: ThreadResumeParams,
    ) -> Result<ThreadResumeResponse> {
        let request_id = self.next_request_id();
        self.send_request(
            ClientRequest::ThreadResume {
                request_id: request_id.clone(),
                params,
            },
            request_id,
            "thread/resume",
        )
        .await
    }

    pub async fn turn_start(&mut self, params: TurnStartParams) -> Result<TurnStartResponse> {
        let request_id = self.next_request_id();
        self.send_request(
            ClientRequest::TurnStart {
                request_id: request_id.clone(),
                params,
            },
            request_id,
            "turn/start",
        )
        .await
    }

    pub async fn next_message(&mut self) -> Result<AppServerSdkMessage> {
        let message = self.read_message().await?;
        match message {
            JSONRPCMessage::Request(request) => Ok(AppServerSdkMessage::Request(
                request
                    .try_into()
                    .context("failed to decode server request")?,
            )),
            JSONRPCMessage::Notification(notification) => {
                match ServerNotification::try_from(notification.clone()) {
                    Ok(notification) => Ok(AppServerSdkMessage::Notification(notification)),
                    Err(_) => Ok(AppServerSdkMessage::RawNotification(notification)),
                }
            }
            JSONRPCMessage::Response(response) => Err(anyhow::anyhow!(
                "unexpected JSON-RPC response outside request flow: {response:?}"
            )),
            JSONRPCMessage::Error(error) => Err(anyhow::anyhow!(
                "unexpected JSON-RPC error outside request flow: {error:?}"
            )),
        }
    }

    pub async fn send_server_request_response<T: Serialize>(
        &mut self,
        request_id: RequestId,
        response: T,
    ) -> Result<()> {
        let result = serde_json::to_value(response)?;
        self.server
            .send(JSONRPCMessage::Response(JSONRPCResponse {
                id: request_id,
                result,
            }))
            .await?;
        Ok(())
    }

    pub async fn send_server_request_error(
        &mut self,
        request_id: RequestId,
        error: codex_app_server_protocol::JSONRPCErrorError,
    ) -> Result<()> {
        self.server
            .send(JSONRPCMessage::Error(JSONRPCError {
                id: request_id,
                error,
            }))
            .await?;
        Ok(())
    }

    pub async fn shutdown(self) -> Result<()> {
        self.server.shutdown().await?;
        Ok(())
    }

    async fn send_request<T>(
        &mut self,
        request: ClientRequest,
        request_id: RequestId,
        method: &str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.server
            .send(client_request_to_jsonrpc(request)?)
            .await?;
        self.wait_for_response(request_id, method).await
    }

    async fn send_notification(&mut self, notification: ClientNotification) -> Result<()> {
        self.server
            .send(client_notification_to_jsonrpc(notification)?)
            .await?;
        Ok(())
    }

    async fn wait_for_response<T>(&mut self, request_id: RequestId, method: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        if let Some(message) = self.take_pending_message(&request_id) {
            return response_from_message(message, &request_id, method);
        }

        loop {
            let message = self.server.recv().await?;
            if matches_request_id(&message, &request_id) {
                return response_from_message(message, &request_id, method);
            }
            self.pending_messages.push_back(message);
        }
    }

    async fn read_message(&mut self) -> Result<JSONRPCMessage> {
        if let Some(message) = self.pending_messages.pop_front() {
            return Ok(message);
        }

        self.server.recv().await.map_err(Into::into)
    }

    fn next_request_id(&mut self) -> RequestId {
        let request_id = RequestId::Integer(self.next_request_id);
        self.next_request_id += 1;
        request_id
    }

    fn take_pending_message(&mut self, request_id: &RequestId) -> Option<JSONRPCMessage> {
        self.pending_messages
            .iter()
            .position(|message| matches_request_id(message, request_id))
            .and_then(|index| self.pending_messages.remove(index))
    }
}

fn client_request_to_jsonrpc(request: ClientRequest) -> Result<JSONRPCMessage> {
    Ok(JSONRPCMessage::Request(
        serde_json::from_value(serde_json::to_value(request)?)
            .context("failed to encode client request")?,
    ))
}

fn client_notification_to_jsonrpc(notification: ClientNotification) -> Result<JSONRPCMessage> {
    Ok(JSONRPCMessage::Notification(
        serde_json::from_value(serde_json::to_value(notification)?)
            .context("failed to encode client notification")?,
    ))
}

fn matches_request_id(message: &JSONRPCMessage, request_id: &RequestId) -> bool {
    match message {
        JSONRPCMessage::Response(response) => &response.id == request_id,
        JSONRPCMessage::Error(error) => &error.id == request_id,
        JSONRPCMessage::Request(_) | JSONRPCMessage::Notification(_) => false,
    }
}

fn response_from_message<T>(
    message: JSONRPCMessage,
    request_id: &RequestId,
    method: &str,
) -> Result<T>
where
    T: DeserializeOwned,
{
    match message {
        JSONRPCMessage::Response(response) => serde_json::from_value(response.result)
            .with_context(|| format!("failed to decode response for `{method}`")),
        JSONRPCMessage::Error(error) => Err(anyhow::anyhow!(
            "request `{method}` with id {request_id:?} failed: {} ({})",
            error.error.message,
            error.error.code
        )),
        JSONRPCMessage::Request(request) => Err(anyhow::anyhow!(
            "unexpected server request while waiting for `{method}` response: {request:?}"
        )),
        JSONRPCMessage::Notification(notification) => Err(anyhow::anyhow!(
            "unexpected notification while waiting for `{method}` response: {notification:?}"
        )),
    }
}
