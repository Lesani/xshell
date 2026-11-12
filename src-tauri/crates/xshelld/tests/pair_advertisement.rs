#![cfg(unix)]
//! The public CLI's displayed code must already name a registered Relay slot.
mod common;
#[path = "desktop/workerd.rs"]
#[allow(dead_code)]
mod workerd;
use std::io::Write;
use xshell_protocol::ring::pairing::PairCode;
use xshell_protocol::ring::relay::pair::{PairOptions, PairPipe};

#[test]
fn legacy_pair_advertises_code_only_after_relay_slot_registration() {
    struct RegistrationProbe {
        relay: String,
        pending: Vec<u8>,
        registered: Option<bool>,
    }
    impl Write for RegistrationProbe {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.pending.extend_from_slice(bytes);
            while let Some(at) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.pending.drain(..=at).collect();
                if let Some(code) = String::from_utf8_lossy(&line)
                    .trim()
                    .strip_prefix("Pairing code: ")
                {
                    let code = PairCode::parse(code).unwrap();
                    // This Write has not returned: no guest work after advertising can
                    // race ahead and make an unregistered code appear usable by chance.
                    let peer =
                        PairPipe::open(&self.relay, &code.secret().slot(), &PairOptions::default())
                            .unwrap();
                    self.registered = Some(peer.has_peer());
                    // The probe deliberately stops before Noise/authentication/adoption.
                    // Existing public pairing controls cover successful enrollment.
                    peer.close();
                    assert_eq!(
                        self.registered,
                        Some(true),
                        "displayed code must already have its joining socket on the Relay"
                    );
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let relay = workerd::Relay::start();
    let home = common::TestHome::new();
    let opts = xshelld::cli::Opts {
        relay: Some(relay.url()),
        ..Default::default()
    };
    let mut probe = RegistrationProbe {
        relay: relay.url(),
        pending: Vec::new(),
        registered: None,
    };
    let exit = xshelld::pair::run_pair_to(&opts, &home.paths(), &mut probe);
    assert_eq!(
        exit, 1,
        "the registration probe intentionally closes without adopting"
    );
    assert_eq!(
        probe.registered,
        Some(true),
        "displayed code must already have its joining socket on the Relay"
    );
}
