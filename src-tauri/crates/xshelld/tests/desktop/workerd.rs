//! Owned production workerd fixture. Missing explicit inputs are hard failures, not skips.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut relay = Self {
            dir,
            root,
            bin,
            port,
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
            .arg(format!("--socket-addr=http=127.0.0.1:{}", self.port))
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
    pub fn restart(&mut self) {
        self.kill();
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
