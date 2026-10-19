//! Typed wrappers for the RPCs the CLI and TUI call. One method per RPC;
//! the service and method names are the proto's, which is also the URL.

use crate::connect::{ServerStream, Transport};
use crate::error::Error;
use crate::proto::*;

/// The service clients for one (host, credential) pair.
#[derive(Debug, Clone)]
pub struct Clients {
    pub transport: Transport,
}

macro_rules! unary {
    ($service:literal, $( $(#[$m:meta])* $name:ident => $method:literal, $req:ty, $resp:ty; )*) => {
        $(
            $(#[$m])*
            pub async fn $name(&self, req: $req) -> Result<$resp, Error> {
                self.transport.unary($service, $method, &req).await
            }
        )*
    };
}

impl Clients {
    pub fn new(transport: Transport) -> Self {
        Clients { transport }
    }

    pub fn host(&self) -> &str {
        self.transport.host()
    }

    // ---- SessionService --------------------------------------------------
    unary!("SessionService",
        list_sessions => "ListSessions", ListSessionsRequest, ListSessionsResponse;
        create_session => "CreateSession", CreateSessionRequest, CreateSessionResponse;
        get_session => "GetSession", GetSessionRequest, GetSessionResponse;
        delete_session => "DeleteSession", DeleteSessionRequest, DeleteSessionResponse;
        send_prompt => "SendPrompt", SendPromptRequest, SendPromptResponse;
        interrupt => "Interrupt", InterruptRequest, InterruptResponse;
        edit_queued_prompt => "EditQueuedPrompt", EditQueuedPromptRequest, EditQueuedPromptResponse;
        dequeue_queued_prompt => "DequeueQueuedPrompt", DequeueQueuedPromptRequest, DequeueQueuedPromptResponse;
        complete_tool_call => "CompleteToolCall", CompleteToolCallRequest, CompleteToolCallResponse;
        list_session_events => "ListSessionEvents", ListSessionEventsRequest, ListSessionEventsResponse;
        get_log => "GetLog", GetLogRequest, GetLogResponse;
        resume => "Resume", ResumeRequest, ResumeResponse;
        evict_idle => "EvictIdle", EvictIdleRequest, EvictIdleResponse;
    );

    /// `SessionService.Exec`: a server stream of stdout/stderr/exit events.
    pub async fn exec(&self, req: ExecRequest) -> Result<ServerStream<ExecOutput>, Error> {
        self.transport
            .server_stream("SessionService", "Exec", &req)
            .await
    }

    // ---- FleetService ----------------------------------------------------
    unary!("FleetService",
        list_hosts => "ListHosts", ListHostsRequest, ListHostsResponse;
        get_host => "GetHost", GetHostRequest, GetHostResponse;
        drain_host => "DrainHost", DrainHostRequest, DrainHostResponse;
        delete_host => "DeleteHost", DeleteHostRequest, DeleteHostResponse;
        uncordon_host => "UncordonHost", UncordonHostRequest, UncordonHostResponse;
        teleport_session => "TeleportSession", TeleportSessionRequest, TeleportSessionResponse;
        flush_session => "FlushSession", FlushSessionRequest, FlushSessionResponse;
        bundle_gc => "BundleGc", BundleGcRequest, BundleGcResponse;
        snapshot_blob_gc => "SnapshotBlobGc", SnapshotBlobGcRequest, SnapshotBlobGcResponse;
        chunk_gc => "ChunkGc", ChunkGcRequest, ChunkGcResponse;
    );

    // ---- ImageService ----------------------------------------------------
    unary!("ImageService",
        list_enabled_images => "ListEnabledImages", ListEnabledImagesRequest, ListEnabledImagesResponse;
        enable_image => "EnableImage", EnableImageRequest, EnableImageResponse;
        update_image => "UpdateImage", UpdateImageRequest, UpdateImageResponse;
        disable_image => "DisableImage", DisableImageRequest, DisableImageResponse;
        refresh_image => "RefreshImage", RefreshImageRequest, RefreshImageResponse;
        list_enable_jobs => "ListEnableJobs", ListEnableJobsRequest, ListEnableJobsResponse;
        get_enable_job => "GetEnableJob", GetEnableJobRequest, GetEnableJobResponse;
        retry_enable_job => "RetryEnableJob", RetryEnableJobRequest, RetryEnableJobResponse;
        list_registries => "ListRegistries", ListRegistriesRequest, ListRegistriesResponse;
        add_registry => "AddRegistry", AddRegistryRequest, AddRegistryResponse;
        delete_registry => "DeleteRegistry", DeleteRegistryRequest, DeleteRegistryResponse;
    );

    // ---- TaskService -----------------------------------------------------
    unary!("TaskService",
        create_task => "CreateTask", CreateTaskRequest, CreateTaskResponse;
        list_tasks => "ListTasks", ListTasksRequest, ListTasksResponse;
        get_task => "GetTask", GetTaskRequest, GetTaskResponse;
        delete_task => "DeleteTask", DeleteTaskRequest, DeleteTaskResponse;
    );

    // ---- ProfileService --------------------------------------------------
    unary!("ProfileService",
        list_profiles => "ListProfiles", ListProfilesRequest, ListProfilesResponse;
        get_profile => "GetProfile", GetProfileRequest, GetProfileResponse;
    );

    // ---- ApiKeyService ---------------------------------------------------
    unary!("ApiKeyService",
        create_api_key => "CreateApiKey", CreateApiKeyRequest, CreateApiKeyResponse;
        list_api_keys => "ListApiKeys", ListApiKeysRequest, ListApiKeysResponse;
        revoke_api_key => "RevokeApiKey", RevokeApiKeyRequest, RevokeApiKeyResponse;
        create_cli_key => "CreateCliKey", CreateCliKeyRequest, CreateCliKeyResponse;
        revoke_cli_key => "RevokeCliKey", RevokeCliKeyRequest, RevokeCliKeyResponse;
        who_am_i => "WhoAmI", WhoAmIRequest, WhoAmIResponse;
    );
}
