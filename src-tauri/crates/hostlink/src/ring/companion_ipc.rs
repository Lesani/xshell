//! Typed counterpart client; never carries a GUI confirmation command or private seed.
use std::path::{Path, PathBuf};
use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};
#[derive(Clone)]
pub struct CompanionClient {
    endpoint: PathBuf,
    daemon_store: PathBuf,
    /// Windows: the process that must serve `endpoint` (BINDING Decision 4), checked on
    /// every exchange before anything is sent. `None`: owner-SID check only.
    #[cfg(windows)]
    server: Option<std::sync::Arc<xshell_core::local_ipc::ServerProcess>>,
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
                #[cfg(windows)]
                server: None,
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
            #[cfg(windows)]
            server: None,
        }
    }
    /// The production client for a retained profile: on Windows every exchange first
    /// proves the pipe is served by the Daemon the profile's PID record names (handle held
    /// for this client's life, PID and creation time matched). Elsewhere: [`for_profile`].
    ///
    /// [`for_profile`]: CompanionClient::for_profile
    pub fn for_profile_served(
        profile: &xshell_protocol::ring::companion::CompanionProfile,
    ) -> Result<Self, String> {
        #[allow(unused_mut)]
        let mut client = Self::for_profile(profile);
        #[cfg(windows)]
        {
            client.server = Some(std::sync::Arc::new(crate::local::recorded_daemon(
                Path::new(&profile.daemon_store),
            )?));
        }
        Ok(client)
    }
    /// [`CompanionClient::for_store`] whose every exchange first proves that `server` (the
    /// Daemon process the Desktop spawned or adopted, its handle held) serves the pipe: a
    /// squatter pipe or a successor process is refused before any request is sent.
    #[cfg(windows)]
    pub fn for_store_served_by(
        store: &Path,
        server: xshell_core::local_ipc::ServerProcess,
    ) -> Result<Self, String> {
        let mut client = Self::for_store(store)?;
        client.server = Some(std::sync::Arc::new(server));
        Ok(client)
    }
    /// The expected serving process's PID, when this client checks one (Windows).
    #[cfg(windows)]
    pub fn expected_server_pid(&self) -> Option<u32> {
        self.server.as_ref().map(|s| s.pid())
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
        let response = self
            .exchange(&request, &budget)
            .map_err(|_| "protected counterpart readiness unavailable".to_string())?;
        match response {
            CompanionResponse::Error { message } => Err(message),
            response => Ok(response),
        }
    }
    pub fn request(&self, request: CompanionRequest) -> Result<CompanionResponse, String> {
        #[cfg(any(unix, windows))]
        {
            let budget = xshell_core::local_ipc::RequestBudget::for_operation(
                std::time::Instant::now() + std::time::Duration::from_secs(5),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            );
            let response = self
                .exchange(&request, &budget)
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
    #[cfg(any(unix, windows))]
    fn exchange(
        &self,
        request: &CompanionRequest,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> std::io::Result<CompanionResponse> {
        #[cfg(windows)]
        if let Some(server) = &self.server {
            return xshell_core::local_ipc::exchange_with_server_until(
                &self.endpoint,
                request,
                budget,
                server,
            );
        }
        xshell_core::local_ipc::exchange_until(&self.endpoint, request, budget)
    }
}
