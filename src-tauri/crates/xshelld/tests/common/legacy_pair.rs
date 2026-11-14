//! Explicit genuine v1 helper control. Never invokes the new production CLI as v1.
use std::io::{BufRead, BufReader, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

pub struct LegacyPair {
    pub worker: Option<std::thread::JoinHandle<Result<(), String>>>,
    output: Option<std::thread::JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    pub lines: mpsc::Receiver<String>,
}
impl LegacyPair {
    pub fn start(options: xshelld::cli::Opts, paths: xshelld::paths::Paths, ttl: Duration) -> Self {
        Self::spawn(move |cancel, writer| {
            let result =
                xshelld::pair::run_legacy_pair_test_control(&options, &paths, writer, ttl, cancel);
            if result.as_ref().is_err_and(|e| e == "expired") {
                let _ = writeln!(
                    writer,
                    "This code expired. Run xshelld pair again to get a new one."
                );
            }
            result
        })
    }
    /// Test-owned worker seam, also used to exercise completion/deadline ordering.
    pub fn spawn(
        run: impl FnOnce(&Arc<AtomicBool>, &mut std::os::unix::net::UnixStream) -> Result<(), String>
            + Send
            + 'static,
    ) -> Self {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (tx, lines) = mpsc::channel();
        let output = std::thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let _ = tx.send(line.unwrap());
            }
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = cancel.clone();
        let worker = std::thread::spawn(move || run(&cancelled, &mut writer));
        Self {
            worker: Some(worker),
            output: Some(output),
            cancel,
            lines,
        }
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub fn wait(&mut self) -> Result<(), String> {
        let result = self.worker.take().unwrap().join().unwrap();
        self.output.take().unwrap().join().unwrap();
        result
    }
}
impl Drop for LegacyPair {
    fn drop(&mut self) {
        self.cancel();
        if let Some(result) = self.worker.take() {
            let _ = result.join();
        }
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
    }
}
