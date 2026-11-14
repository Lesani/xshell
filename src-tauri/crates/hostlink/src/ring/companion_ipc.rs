//! Typed counterpart client; never carries a GUI confirmation command or private seed.
use std::path::{Path, PathBuf};
use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};
#[derive(Clone)]
pub struct CompanionClient {
    endpoint: PathBuf,
    daemon_store: PathBuf,
}
impl CompanionClient {
    pub fn for_store(store: &Path) -> Result<Self, String> {
        #[cfg(any(unix, windows))]
        {
            let daemon_store =
                xshell_core::local_ipc::canonical_store(store).map_err(|e| e.to_string())?;
            let endpoint = xshell_core::local_ipc::companion_endpoint(&daemon_store)
                .map_err(|e| e.to_string())?;
            Ok(Self {
                endpoint,
                daemon_store,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = store;
            Err("protected counterpart platform adapter is not implemented".into())
        }
    }
    /// A retained, already validated counterpart profile names both the endpoint and the
    /// Daemon store; Windows pipe endpoints have no parent directory to derive it from.
    pub fn for_profile(profile: &xshell_protocol::ring::companion::CompanionProfile) -> Self {
        Self {
            endpoint: PathBuf::from(&profile.endpoint),
            daemon_store: PathBuf::from(&profile.daemon_store),
        }
    }
    /// The Unix registration child's reserved endpoint inside its canonical Daemon store.
    #[cfg(unix)]
    pub(crate) fn for_endpoint_in(daemon_store: PathBuf, endpoint: PathBuf) -> Self {
        Self {
            endpoint,
            daemon_store,
        }
    }
    pub fn daemon_store(&self) -> &Path {
        &self.daemon_store
    }
    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }
    #[cfg(any(unix, windows))]
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
        #[cfg(any(unix, windows))]
        {
            let response =
                xshell_core::local_ipc::exchange::<_, CompanionResponse>(&self.endpoint, &request)
                    .map_err(|e| format!("protected counterpart unavailable: {e}"))?;
            match response {
                CompanionResponse::Error { message } => Err(message),
                response => Ok(response),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = request;
            Err("protected counterpart platform adapter is not implemented".into())
        }
    }
}
