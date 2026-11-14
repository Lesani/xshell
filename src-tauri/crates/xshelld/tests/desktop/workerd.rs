//! Owned production workerd fixture. Missing explicit inputs are hard failures, not skips.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use xshell_protocol::ring::relay::contract::RelayTarget;
const CONFIG_VARS: [&str; 11] = [
    "RELAY_ORIGIN",
    "GATEWAY_PUBLIC_KEYS",
    "AUTH_TIMEOUT_MS",
    "QUOTA_FRAMES_PER_DAY",
    "PAIR_OPENS_PER_MINUTE",
    "PAIR_MAX_SLOTS",
    "PAIR_MAX_SLOTS_PER_PREFIX",
    "PAIR_TTL_MS",
    "CLIENT_IP_HEADER",
    "PUSH_GATEWAY_URL",
    "FOREGROUND_LEASE_MS",
];
pub struct Relay {
    dir: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    port: u16,
    /// Unix passes this reserved listener to workerd. Windows keeps the original
    /// bind-then-release port probe and `--socket-addr`.
    #[cfg(unix)]
    listener: TcpListener,
    child: Option<Child>,
}
impl Relay {
    pub fn start() -> Self {
        let root = PathBuf::from(
            std::env::var_os("XSHELL_ADOPTION_RELAY_DIR")
                .expect("XSHELL_ADOPTION_RELAY_DIR required"),
        );
        let bin = PathBuf::from(std::env::var_os("WORKERD").expect("absolute WORKERD required"));
        assert!(
            root.is_absolute()
                && root.join("dist/index.js").is_file()
                && root.join("workerd/config.capnp").is_file()
        );
        assert!(bin.is_absolute() && bin.is_file());
        let dir = tempfile::Builder::new()
            .prefix("adoption-workerd-")
            .tempdir()
            .unwrap();
        std::fs::create_dir(dir.path().join("data")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        #[cfg(windows)]
        drop(listener);
        let mut relay = Self {
            dir,
            root,
            bin,
            port,
            #[cfg(unix)]
            listener,
            child: None,
        };
        relay.spawn();
        relay
    }
    fn spawn(&mut self) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.path().join("workerd.log"))
            .unwrap();
        let mut command = Command::new(&self.bin);
        command
            .current_dir(&self.root)
            .arg("serve")
            .arg("workerd/config.capnp")
            .arg(self.socket_arg())
            .arg(format!(
                "--directory-path=disk={}",
                self.dir.path().join("data").display()
            ))
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        for name in CONFIG_VARS {
            command.env_remove(name);
        }
        #[cfg(unix)]
        let fd = self.listener.as_raw_fd();
        // Only the owned child inherits this listener. Parent CLOEXEC is unchanged;
        // the listener remains reserved even while workerd is stopped for restart.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn production workerd");
        let until = Instant::now() + Duration::from_secs(20);
        while Instant::now() < until {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if self.healthy() {
                self.child = Some(child);
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "workerd readiness failed: {}",
            std::fs::read_to_string(self.dir.path().join("workerd.log")).unwrap()
        );
    }
    fn healthy(&self) -> bool {
        let Ok(mut stream) = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", self.port).parse().unwrap(),
            Duration::from_millis(100),
        ) else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
        let _ = stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let mut header = [0; 64];
        stream.read(&mut header).is_ok_and(|length| {
            String::from_utf8_lossy(&header[..length]).starts_with("HTTP/1.1 200")
        })
    }
    #[cfg(unix)]
    fn socket_arg(&self) -> String {
        format!("--socket-fd=http={}", self.listener.as_raw_fd())
    }
    #[cfg(windows)]
    fn socket_arg(&self) -> String {
        format!("--socket-addr=http=127.0.0.1:{}", self.port)
    }
    pub fn restart(&mut self) {
        self.kill();
        #[cfg(unix)]
        assert!(
            matches!(TcpListener::bind(self.listener.local_addr().unwrap()),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse),
            "owned listener must remain reserved with no workerd child"
        );
        #[cfg(unix)]
        assert_ne!(
            unsafe { libc::fcntl(self.listener.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0,
            "child inheritance must not change parent CLOEXEC"
        );
        self.spawn();
    }
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    pub fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
    pub fn target(&self) -> RelayTarget {
        RelayTarget {
            url: self.url(),
            tls: None,
            auth_timeout: Duration::from_secs(10),
            gateway: None,
            quota_frames_per_day: None,
            pair_opens_per_minute: None,
            pair_ttl: None,
            client_ip_header: None,
            fake_gateway: None,
            foreground_lease: None,
        }
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.kill();
    }
}
