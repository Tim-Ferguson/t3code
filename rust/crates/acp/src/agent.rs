//! Agent-side ACP v2 facade sharing the same bidirectional dispatcher as Client.
use crate::{
    AcpError, Client, Peer, RequestContext, RpcError,
    client::{NotificationHandler, RequestHandler, Role},
    schema::{SchemaName, Wire},
    v2,
};
use futures_util::future::BoxFuture;
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
pub struct Agent {
    connection: Client,
}
impl Agent {
    pub fn new(peer: Arc<dyn Peer>, request_timeout: Duration) -> Self {
        Self {
            connection: Client::with_role(peer, request_timeout, Role::Agent),
        }
    }
    pub fn connection(&self) -> &Client {
        &self.connection
    }
    pub fn shutdown(&self) {
        self.connection.shutdown();
    }
    pub fn handle_raw(&self, method: &str, handler: RequestHandler) {
        self.connection.handle_request(method, handler);
    }
    pub fn handle_unknown_request(&self, handler: RequestHandler) {
        self.connection.handle_unknown_request(handler);
    }
    pub fn handle_unknown_notification(&self, handler: NotificationHandler) {
        self.connection.handle_unknown_notification(handler);
    }
    /// Decoded request/response schemas are distinct Rust types. Handler failures
    /// retain protocol errors; other errors use stable messages without causes.
    pub fn handle<Q: SchemaName + Send + Sync + 'static, R: SchemaName + Send + Sync + 'static>(
        &self,
        method: &str,
        handler: Arc<
            dyn Fn(Wire<Q>, RequestContext) -> BoxFuture<'static, Result<Wire<R>, AcpError>>
                + Send
                + Sync,
        >,
    ) {
        let name = method.to_owned();
        let events = self.connection.handler_events();
        self.connection.handle_request(
            method,
            Arc::new(move |value, context| {
                let handler = handler.clone();
                let name = name.clone();
                let events = events.clone();
                Box::pin(async move {
                    let request =
                        Wire::<Q>::decode(value).map_err(|_| RpcError::invalid_params())?;
                    let request_id = context.request_id.clone();
                    handler(request, context)
                        .await
                        .map(Wire::into_value)
                        .map_err(|cause| {
                            let error =
                                crate::errors::RequestError::from_core_handler_error(cause, &name);
                            let protocol = error.to_protocol_error();
                            let _ = events.send(crate::ClientEvent::RequestHandlerFailed {
                                request_id,
                                error: error.into(),
                            });
                            protocol
                        })
                })
            }),
        );
    }
    async fn request<Q: SchemaName, R: SchemaName>(
        &self,
        method: &str,
        request: Wire<Q>,
    ) -> Result<Wire<R>, AcpError> {
        Ok(Wire::<R>::decode(
            self.connection
                .raw_request(method, request.into_value())
                .await?,
        )?)
    }
    pub async fn request_permission(
        &self,
        request: v2::RequestPermissionRequest,
    ) -> Result<v2::RequestPermissionResponse, AcpError> {
        self.request("session/request_permission", request).await
    }
    pub async fn elicit(
        &self,
        request: v2::CreateElicitationRequest,
    ) -> Result<v2::CreateElicitationResponse, AcpError> {
        self.request("elicitation/create", request).await
    }
    pub async fn connect_mcp(
        &self,
        request: v2::ConnectMcpRequest,
    ) -> Result<v2::ConnectMcpResponse, AcpError> {
        self.request("mcp/connect", request).await
    }
    pub async fn message_mcp(
        &self,
        request: v2::MessageMcpRequest,
    ) -> Result<v2::MessageMcpResponse, AcpError> {
        self.request("mcp/message", request).await
    }
    pub async fn disconnect_mcp(
        &self,
        request: v2::DisconnectMcpRequest,
    ) -> Result<v2::DisconnectMcpResponse, AcpError> {
        self.request("mcp/disconnect", request).await
    }
    pub async fn session_update(
        &self,
        request: v2::UpdateSessionNotification,
    ) -> Result<(), AcpError> {
        self.connection
            .raw_notify("session/update", request.into_value())
            .await
    }
    pub async fn elicitation_complete(
        &self,
        request: v2::CompleteElicitationNotification,
    ) -> Result<(), AcpError> {
        self.connection
            .raw_notify("elicitation/complete", request.into_value())
            .await
    }
    pub async fn notify_mcp(&self, request: v2::MessageMcpNotification) -> Result<(), AcpError> {
        self.connection
            .raw_notify("mcp/message", request.into_value())
            .await
    }
    pub async fn handle_cancel(
        &self,
        handler: Arc<
            dyn Fn(v2::CancelNotification) -> BoxFuture<'static, Result<(), AcpError>>
                + Send
                + Sync,
        >,
    ) {
        self.connection
            .handle_notification(
                "session/cancel",
                Arc::new(move |value| {
                    let handler = handler.clone();
                    Box::pin(async move { handler(v2::CancelNotification::decode(value)?).await })
                }),
            )
            .await;
    }
}

macro_rules! agent_handlers {($(($method:ident,$wire:literal,$request:ident,$response:ident)),*$(,)?)=>{impl Agent{$(pub fn $method(&self,handler:Arc<dyn Fn(v2::$request,RequestContext)->BoxFuture<'static,Result<v2::$response,AcpError>>+Send+Sync>){self.handle($wire,handler);})*}};}
agent_handlers! {
    (handle_initialize,"initialize",InitializeRequest,InitializeResponse),
    (handle_authenticate,"auth/login",LoginAuthRequest,LoginAuthResponse),
    (handle_logout,"auth/logout",LogoutAuthRequest,LogoutAuthResponse),
    (handle_create_session,"session/new",NewSessionRequest,NewSessionResponse),
    (handle_list_sessions,"session/list",ListSessionsRequest,ListSessionsResponse),
    (handle_fork_session,"session/fork",ForkSessionRequest,ForkSessionResponse),
    (handle_load_session,"session/load",ResumeSessionRequest,ResumeSessionResponse),
    (handle_resume_session,"session/resume",ResumeSessionRequest,ResumeSessionResponse),
    (handle_close_session,"session/close",CloseSessionRequest,CloseSessionResponse),
    (handle_delete_session,"session/delete",DeleteSessionRequest,DeleteSessionResponse),
    (handle_list_providers,"providers/list",ListProvidersRequest,ListProvidersResponse),
    (handle_set_provider,"providers/set",SetProviderRequest,SetProviderResponse),
    (handle_disable_provider,"providers/disable",DisableProviderRequest,DisableProviderResponse),
    (handle_set_session_config_option,"session/set_config_option",SetSessionConfigOptionRequest,SetSessionConfigOptionResponse),
    (handle_prompt,"session/prompt",PromptRequest,PromptResponse),
}
