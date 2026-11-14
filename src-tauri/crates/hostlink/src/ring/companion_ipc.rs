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
    pub fn daemon_store(&self) -> &Path {
        &self.daemon_store
    }
    pub fn endpoint(&self) -> &Path {
        &self.endpoint
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
