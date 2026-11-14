//! Typed counterpart client; never carries a GUI confirmation command or private seed.
use std::path::{Path, PathBuf};
use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};
#[derive(Clone)]
pub struct CompanionClient {
    endpoint: PathBuf,
}
impl CompanionClient {
    pub fn new(endpoint: PathBuf) -> Self {
        Self { endpoint }
    }
    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }
    #[cfg(unix)]
    pub fn request_until(
        &self,
        request: CompanionRequest,
        until: std::time::Instant,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<CompanionResponse, String> {
        let budget = xshell_core::local_ipc::RequestBudget::for_operation(until, cancel);
        let response = xshell_core::local_ipc::exchange_until(&self.endpoint, &request, &budget)
            .map_err(|_| "protected counterpart readiness unavailable".to_string())?;
        match response {
            CompanionResponse::Error { message } => Err(message),
            response => Ok(response),
        }
    }
    pub fn request(&self, request: CompanionRequest) -> Result<CompanionResponse, String> {
        #[cfg(unix)]
        {
            let response =
                xshell_core::local_ipc::exchange::<_, CompanionResponse>(&self.endpoint, &request)
                    .map_err(|e| format!("protected counterpart unavailable: {e}"))?;
            match response {
                CompanionResponse::Error { message } => Err(message),
                response => Ok(response),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = request;
            Err("protected counterpart platform adapter is not implemented".into())
        }
    }
}
