//! The Desktop's client side of Remote Hosts, without Tauri: the ssh transport, the protocol
//! link, managed install, per-Host supervision and the attachment table. The Desktop wraps
//! [`Manager`] in Tauri commands and events; tests drive it against a real `xshelld`.

pub mod cancel;
pub mod config;
pub mod dial;
pub mod errors;
pub mod handle;
pub mod install;
pub mod link;
#[cfg(unix)]
pub mod local;
pub mod manager;
pub mod process;
pub mod ring;
pub mod ssh_config;
pub mod status;
pub mod supervisor;
pub mod transport;
pub mod version;

pub use cancel::CancelToken;
pub use config::{HostConfig, HOST_ID_PATTERN, LOCAL_HOST_ID};
#[cfg(unix)]
pub use dial::UnixSocketDialer;
pub use dial::{Connection, DialError, Dialed, Dialer};
pub use errors::{HostError, HostErrorCode, HostErrorHint, SwitchError};
pub use handle::{HostHandle, TermSink};
pub use install::{BinarySource, ChainSource, DirSource, FileSource};
pub use link::Waiter;
#[cfg(unix)]
pub use local::{LocalDaemon, LocalDaemonConfig, LocalDialer};
pub use manager::{Manager, ManagerConfig, Observer};
pub use status::{HostSnapshot, HostStatus, HostTestResult, Phase, StatusKind};
pub use transport::{LocalShellTransport, SshTransport, Transport, TransportFactory};
