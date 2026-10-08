//! The Desktop's client side of Remote Hosts, without Tauri: the ssh transport, the protocol
//! link, managed install, per-Host supervision and the attachment table. The Desktop wraps
//! [`Manager`] in Tauri commands and events; tests drive it against a real `xshelld`.

pub mod cancel;
pub mod config;
pub mod errors;
pub mod handle;
pub mod install;
pub mod link;
pub mod manager;
pub mod process;
pub mod ssh_config;
pub mod status;
pub mod supervisor;
pub mod transport;
pub mod version;

pub use cancel::CancelToken;
pub use config::{HostConfig, HOST_ID_PATTERN};
pub use errors::{HostError, HostErrorCode, HostErrorHint};
pub use handle::{HostHandle, TermSink};
pub use install::{BinarySource, ChainSource, DirSource, FileSource};
pub use link::Waiter;
pub use manager::{Manager, ManagerConfig, Observer};
pub use status::{HostSnapshot, HostStatus, HostTestResult, Phase, StatusKind};
pub use transport::{LocalShellTransport, SshTransport, Transport, TransportFactory};
