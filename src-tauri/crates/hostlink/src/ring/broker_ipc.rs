//! Persistent protected Host broker transport. Companion request budgets are not used here.
use serde::{de::DeserializeOwned, Serialize};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub(crate) const CHUNK: usize = 64 * 1024;
const MAX: usize = 64 * 1024 * 1024 + CHUNK;
const QUEUED: usize = 16 * 1024 * 1024;
const OUTPUT: usize = 8 * 1024 * 1024;
const CONTROL: usize = 1024 * 1024;
#[derive(Default)]
pub(crate) struct Assembly(AtomicBool);
struct Permit(Arc<Assembly>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0 .0.store(false, Ordering::Release);
    }
}
fn reserve(size: usize, assembly: &Arc<Assembly>) -> io::Result<Option<Permit>> {
    if size == 0 || size > MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker frame bounds",
        ));
    }
    if size <= QUEUED {
        return Ok(None);
    }
    assembly
        .0
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| Some(Permit(assembly.clone())))
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "large broker assembly busy"))
}
#[derive(Debug)]
pub(crate) struct AssemblyBusy {
    pub request_id: u64,
}
impl std::fmt::Display for AssemblyBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "large broker assembly busy")
    }
}
impl std::error::Error for AssemblyBusy {}
struct Count(usize);
impl Write for Count {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0 += b.len();
        if self.0 > MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "broker frame bounds",
            ));
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Frame {
    bytes: Vec<u8>,
    output: bool,
    key: Option<u64>,
    _permit: Option<Permit>,
}
#[derive(Default)]
struct Queue {
    frames: VecDeque<Frame>,
    total: usize,
    output: usize,
    stopped: bool,
}
/// Ordered queue with separate control headroom. Admission never waits on a reader.
pub(crate) struct Outbox {
    queue: Mutex<Queue>,
    cv: Condvar,
    assembly: Arc<Assembly>,
}
impl Outbox {
    pub fn new(assembly: Arc<Assembly>) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(Queue::default()),
            cv: Condvar::new(),
            assembly,
        })
    }
    pub fn send(&self, value: &impl Serialize, output: bool) -> io::Result<()> {
        self.send_key(value, output, None)
    }
    pub fn send_key(
        &self,
        value: &impl Serialize,
        output: bool,
        key: Option<u64>,
    ) -> io::Result<()> {
        let mut count = Count(0);
        serde_json::to_writer(&mut count, value)?;
        let permit = reserve(count.0, &self.assembly)?;
        let mut q = self.queue.lock().unwrap();
        if q.stopped {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "broker disconnected",
            ));
        }
        let large = permit.is_some();
        if (output && q.output + count.0 > OUTPUT)
            || (!large && q.total + count.0 > QUEUED + if output { 0 } else { CONTROL })
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "broker client queue full",
            ));
        }
        let bytes = serde_json::to_vec(value)?;
        if !large {
            q.total += bytes.len();
        }
        if output {
            q.output += bytes.len();
        }
        q.frames.push_back(Frame {
            bytes,
            output,
            key,
            _permit: permit,
        });
        self.cv.notify_one();
        Ok(())
    }
    /// True only when the complete request was still queued and cannot have been sent.
    pub fn cancel(&self, key: u64) -> bool {
        let mut q = self.queue.lock().unwrap();
        if let Some(index) = q.frames.iter().position(|frame| frame.key == Some(key)) {
            let frame = q.frames.remove(index).unwrap();
            if frame._permit.is_none() {
                q.total -= frame.bytes.len();
            }
            if frame.output {
                q.output -= frame.bytes.len();
            }
            return true;
        }
        false
    }
    pub fn stop(&self) {
        let mut q = self.queue.lock().unwrap();
        q.stopped = true;
        q.frames.clear();
        q.total = 0;
        q.output = 0;
        self.cv.notify_all();
    }
    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        loop {
            let frame = {
                let mut q = self.queue.lock().unwrap();
                while q.frames.is_empty() && !q.stopped {
                    q = self.cv.wait(q).unwrap();
                }
                if q.stopped {
                    return Ok(());
                }
                let frame = q.frames.pop_front().unwrap();
                if frame._permit.is_none() {
                    q.total -= frame.bytes.len();
                }
                if frame.output {
                    q.output -= frame.bytes.len();
                }
                frame
            };
            writer.write_all(&(frame.bytes.len() as u32).to_be_bytes())?;
            writer.write_all(&frame.key.unwrap_or(0).to_be_bytes())?;
            for chunk in frame.bytes.chunks(CHUNK) {
                writer.write_all(chunk)?;
            }
        }
    }
}
/// Small synchronous control sender using exactly the persistent broker framing.
// Used only by the Unix-only `administration::AdministrationClient`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn write_control(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.is_empty() || bytes.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "broker control frame bounds",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&0u64.to_be_bytes())?;
    writer.write_all(&bytes)
}
pub(crate) fn read<T: DeserializeOwned>(
    reader: &mut impl Read,
    assembly: &Arc<Assembly>,
) -> io::Result<T> {
    read_limited(reader, assembly, MAX)
}
pub(crate) fn read_limited<T: DeserializeOwned>(
    reader: &mut impl Read,
    assembly: &Arc<Assembly>,
    maximum: usize,
) -> io::Result<T> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker frame bounds",
        ));
    }
    let mut key = [0; 8];
    reader.read_exact(&mut key)?;
    let permit = reserve(size, assembly);
    if permit
        .as_ref()
        .is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock)
    {
        // The authenticated header identifies the waiter without allocating an assembly.
        // Drain bounded chunks so a refused request cannot desynchronize this connection.
        let mut left = size;
        let mut chunk = [0; CHUNK];
        while left > 0 {
            let n = left.min(CHUNK);
            reader.read_exact(&mut chunk[..n])?;
            left -= n;
        }
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            AssemblyBusy {
                request_id: u64::from_be_bytes(key),
            },
        ));
    }
    let _permit = permit?;
    let mut bytes = vec![0; size];
    for chunk in bytes.chunks_mut(CHUNK) {
        reader.read_exact(chunk)?;
    }
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(any(unix, windows))]
pub(crate) mod native {
    use std::io::{self, Read, Write};
    use std::time::Instant;
    pub use xshell_core::local_ipc::Stream;
    /// One absolute control-request budget, including partial reads/writes and snapshots.
    // Used only by the Unix-only `administration::AdministrationClient`.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub struct DeadlineStream<'a> {
        pub stream: &'a mut Stream,
        pub until: Instant,
    }
    impl DeadlineStream<'_> {
        #[cfg_attr(not(unix), allow(dead_code))]
        fn left(&self) -> io::Result<std::time::Duration> {
            self.until
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "broker control deadline"))
        }
    }
    impl Read for DeadlineStream<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.stream.set_read_timeout(Some(self.left()?))?;
            self.stream.read(bytes)
        }
    }
    impl Write for DeadlineStream<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.stream.set_write_timeout(Some(self.left()?))?;
            self.stream.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }
    #[cfg(unix)]
    pub fn pair() -> std::io::Result<(Stream, Stream)> {
        Stream::pair()
    }
    pub use xshell_core::local_ipc::{protected_connect as connect, ProtectedListener as Listener};
    #[cfg(windows)]
    pub use xshell_core::pipe::pair;
}
// Unsupported platforms have no insecure transport fallback.
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_bounds_and_large_assembly_are_independent_of_companion_limits() {
        let assembly = Arc::new(Assembly::default());
        assert!(reserve(MAX + 1, &assembly).is_err());
        assert!(reserve(0, &assembly).is_err());
        let permit = reserve(QUEUED + 1, &assembly).unwrap();
        assert!(reserve(QUEUED + 1, &assembly).is_err());
        assert!(reserve(CHUNK, &assembly).is_ok());
        drop(permit);
        assert!(reserve(MAX, &assembly).is_ok());
    }
    #[test]
    fn slow_output_cannot_consume_control_admission() {
        let out = Outbox::new(Arc::new(Assembly::default()));
        let chunk = "x".repeat(CHUNK);
        while out.send(&chunk, true).is_ok() {}
        assert!(out.send(&"mutation reply", false).is_ok());
        out.stop();
        assert!(out.send(&"late", false).is_err());
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    #[test]
    fn large_message_reservation_keeps_control_headroom_and_unsent_cancel_releases_it() {
        let out = Outbox::new(Arc::new(Assembly::default()));
        let large = "L".repeat(QUEUED + 1024);
        out.send_key(&large, false, Some(1)).unwrap();
        out.send(&"control reply", false).unwrap();
        assert!(out.send_key(&large, false, Some(2)).is_err());
        assert!(out.cancel(1));
        assert!(!out.cancel(1));
        out.send_key(&large, false, Some(3)).unwrap();
        out.stop();
    }
}
