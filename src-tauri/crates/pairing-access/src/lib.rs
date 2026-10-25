//! Computer-side Pairing access. A live process owns ephemeral TLS keys; only public
//! descriptors and redacted typed outcomes cross inherited protected streams. Same-user
//! debugging/process inspection is outside this boundary. Import is never verification.
//! On Windows a trusted launcher must supply private inherited pipes with the intended
//! peer and ACLs: the helper classifies pipe handles, but does not verify peer SIDs/ACLs.
//! Linux runtime tests do not establish Windows or macOS IPC behavior.
pub mod tls;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use xshell_protocol::pairing_access::*;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub struct Controller {
    proposal: ControllerDescriptor,
    identity: tls::Identity,
    server: Option<ServerDescriptor>,
    verified: bool,
    stream: Option<tls::ClientStream>,
    deadline: Instant,
}
impl Controller {
    pub fn open(scope: Scope, label: String, expires_at: u64) -> Result<Self, AccessError> {
        let identity = tls::Identity::generate(false, expires_at)?;
        let proposal = ControllerDescriptor {
            version: VERSION,
            task: uuid::Uuid::new_v4(),
            label,
            certificate: identity.certificate.clone(),
            scope,
            expires_at,
        };
        proposal.validate(now())?;
        let deadline = Instant::now()
            + std::time::Duration::from_secs(expires_at.saturating_sub(now()).min(TASK_MAX_SECS));
        Ok(Self {
            proposal,
            identity,
            server: None,
            verified: false,
            stream: None,
            deadline,
        })
    }
    pub fn proposal(&self) -> &ControllerDescriptor {
        &self.proposal
    }
    pub fn controller_fingerprint(&self) -> [u8; 32] {
        tls::fingerprint(&self.proposal.certificate)
    }
    pub fn import_server(&mut self, server: ServerDescriptor) -> Result<(), AccessError> {
        self.verified = false;
        self.stream = None;
        self.server = None;
        server.validate_for(&self.proposal, self.controller_fingerprint(), now())?;
        tls::client_config(&self.identity, &server.certificate)?;
        self.server = Some(server);
        Ok(())
    }
    pub fn verify(&mut self, decision: VerificationDecision) -> Result<(), AccessError> {
        self.verified = false;
        self.stream = None;
        let server = self
            .server
            .as_ref()
            .ok_or(AccessError::BootstrapUnverified)?;
        server.validate_for(&self.proposal, self.controller_fingerprint(), now())?;
        if Instant::now() >= self.deadline {
            return Err(AccessError::Expired);
        }
        if decision.controller_fingerprint != self.controller_fingerprint()
            || decision.server_fingerprint != tls::fingerprint(&server.certificate)
            || decision.endpoint != server.endpoint
        {
            return Err(AccessError::BootstrapUnverified);
        }
        self.verified = true;
        Ok(())
    }
    pub fn connect(&self) -> Result<tls::ClientStream, AccessError> {
        if !self.verified {
            return Err(AccessError::BootstrapUnverified);
        }
        self.proposal.validate(now())?;
        if Instant::now() >= self.deadline {
            return Err(AccessError::Expired);
        }
        let server = self
            .server
            .as_ref()
            .ok_or(AccessError::BootstrapUnverified)?;
        tls::connect(
            server.endpoint,
            tls::client_config(&self.identity, &server.certificate)?,
            &server.certificate,
        )
    }
    pub fn exchange(&mut self, request: &Request) -> Result<Response, AccessError> {
        request.validate()?;
        self.proposal.validate(now())?;
        if Instant::now() >= self.deadline {
            return Err(AccessError::Expired);
        }
        if !self.verified {
            return Err(AccessError::BootstrapUnverified);
        }
        if request.task != self.proposal.task || request.scope != self.proposal.scope {
            return Err(AccessError::ScopeMismatch);
        }
        if self.stream.is_none() {
            self.stream = Some(self.connect()?);
        }
        let stream = self.stream.as_mut().unwrap();
        stream.sock.deadline(Instant::now() + tls::WRITE);
        let result = write_frame(stream, request).and_then(|_| {
            // The coordinator has two seconds; allow that plus the response write bound.
            stream
                .sock
                .deadline(Instant::now() + tls::READ + tls::WRITE);
            read_frame(stream)
        });
        if result.is_err() || matches!(request.command, Command::Cancel {}) {
            self.stream = None;
        }
        result
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "method", deny_unknown_fields, rename_all = "kebab-case")]
pub enum HelperInput {
    Open {
        scope: Scope,
        label: String,
        expires_at: u64,
    },
    ImportServer {
        descriptor: ServerDescriptor,
    },
    Verify {
        decision: VerificationDecision,
    },
    Request {
        request: Request,
    },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", deny_unknown_fields, rename_all = "kebab-case")]
pub enum HelperOutput {
    Proposal {
        descriptor: ControllerDescriptor,
        controller_fingerprint: [u8; 32],
    },
    Imported {
        server_fingerprint: [u8; 32],
        error: AccessError,
    },
    Verified {},
    Response {
        response: Response,
    },
    Error {
        error: AccessError,
    },
}
/// Synchronous bounded command loop, shared by the standalone binary and actual Desktop
/// pre-window dispatch. The caller owns protected input/output pipes for this live process.
pub fn run(input: &mut impl Read, output: &mut impl Write) -> Result<(), AccessError> {
    let mut controller: Option<Controller> = None;
    loop {
        // EOF before a new header is normal; partial frames are not.
        let mut first = [0];
        match input.read(&mut first) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(_) => return Err(AccessError::TransportClosedOutcomeUnknown),
        }
        let mut prefixed = (&first[..]).chain(&mut *input);
        let command: HelperInput = read_frame(&mut prefixed)?;
        let result = match command {
            HelperInput::Open {
                scope,
                label,
                expires_at,
            } => {
                // Replacing a live helper identity is not implicit; one task per process.
                if controller.is_some() {
                    Err(AccessError::RequestConflict)
                } else {
                    Controller::open(scope, label, expires_at).map(|c| {
                        let response = HelperOutput::Proposal {
                            descriptor: c.proposal().clone(),
                            controller_fingerprint: c.controller_fingerprint(),
                        };
                        controller = Some(c);
                        response
                    })
                }
            }
            HelperInput::ImportServer { descriptor } => controller
                .as_mut()
                .ok_or(AccessError::Unauthorized)
                .and_then(|c| {
                    c.import_server(descriptor.clone())?;
                    Ok(HelperOutput::Imported {
                        server_fingerprint: tls::fingerprint(&descriptor.certificate),
                        error: AccessError::BootstrapUnverified,
                    })
                }),
            HelperInput::Verify { decision } => controller
                .as_mut()
                .ok_or(AccessError::Unauthorized)
                .and_then(|c| {
                    c.verify(decision)?;
                    Ok(HelperOutput::Verified {})
                }),
            HelperInput::Request { request } => controller
                .as_mut()
                .ok_or(AccessError::Unauthorized)
                .and_then(|c| {
                    c.exchange(&request)
                        .map(|response| HelperOutput::Response { response })
                }),
        };
        write_frame(
            output,
            &result.unwrap_or_else(|error| HelperOutput::Error { error }),
        )?;
    }
}

pub const HELP: &str = "Pairing access commands. Public descriptors and redacted outcomes only.\nUsage: xshell pairing-access\n";
/// No descriptor, credential, address, or key is accepted in argv/environment/files.
pub fn cli_main(args: &[std::ffi::OsString]) -> i32 {
    if args == [std::ffi::OsString::from("--help")] {
        print!("{HELP}");
        return 0;
    }
    if !args.is_empty() || !protected_stdio() {
        eprintln!("invalid-request");
        return 2;
    }
    match run(&mut std::io::stdin().lock(), &mut std::io::stdout().lock()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}
fn protected_stdio() -> bool {
    #[cfg(unix)]
    {
        [libc::STDIN_FILENO, libc::STDOUT_FILENO]
            .into_iter()
            .all(|fd| {
                let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
                    return false;
                }
                let stat = unsafe { stat.assume_init() };
                let mode = stat.st_mode & libc::S_IFMT;
                if mode == libc::S_IFIFO {
                    return stat.st_uid == unsafe { libc::geteuid() } && stat.st_mode & 0o077 == 0;
                }
                if mode != libc::S_IFSOCK {
                    return false;
                }
                // Inherited connected local sockets are supported; TCP stdio is not a
                // protected local control channel, even when the parent passes its FD.
                let mut address = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
                let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                if unsafe { libc::getpeername(fd, address.as_mut_ptr().cast(), &mut len) } != 0 {
                    return false;
                }
                if unsafe { address.assume_init() }.ss_family as i32 != libc::AF_UNIX {
                    return false;
                }
                #[cfg(target_os = "linux")]
                {
                    let mut cred = std::mem::MaybeUninit::<libc::ucred>::uninit();
                    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
                    unsafe {
                        libc::getsockopt(
                            fd,
                            libc::SOL_SOCKET,
                            libc::SO_PEERCRED,
                            cred.as_mut_ptr().cast(),
                            &mut len,
                        ) == 0
                            && cred.assume_init().uid == libc::geteuid()
                    }
                }
                #[cfg(any(target_os = "macos", target_os = "freebsd"))]
                {
                    let mut uid = 0;
                    let mut gid = 0;
                    unsafe {
                        libc::getpeereid(fd, &mut uid, &mut gid) == 0 && uid == libc::geteuid()
                    }
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
                {
                    false
                }
            })
    }
    #[cfg(windows)]
    {
        // Handle classification only, not peer/SID/ACL authentication. The trusted
        // launcher is responsible for creating and privately inheriting these pipes.
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_PIPE};
        [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
        ]
        .into_iter()
        .all(|h| unsafe { GetFileType(h as _) } == FILE_TYPE_PIPE)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}
