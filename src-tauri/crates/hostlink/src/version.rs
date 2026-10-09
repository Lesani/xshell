//! What a Daemon's `hello` means for this Desktop (ADR-0003): usable, usable with an upgrade
//! pending, or unusable until one side is upgraded.

use xshell_core::protocol::msg::{Hello, ProtocolRange};
use xshell_core::protocol::negotiate::negotiate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompatibleReason {
    /// The Daemon only speaks older protocols: upgrading it fixes this.
    Older,
    /// The Daemon only speaks newer protocols: the Desktop must be updated.
    Newer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    Compatible {
        negotiated: u32,
        upgrade_pending: bool,
    },
    Incompatible {
        reason: IncompatibleReason,
        message: String,
    },
}

pub fn incompatible_message(ours: ProtocolRange, hello: &Hello) -> String {
    format!(
        "no common protocol version: this Desktop speaks {ours}, xshelld {} on the host speaks {}",
        hello.version, hello.protocol
    )
}

/// `managed == false` (a daemon command override) never reports an upgrade as pending: the
/// Desktop does not install or upgrade such a Daemon.
pub fn classify(desktop: &str, ours: ProtocolRange, hello: &Hello, managed: bool) -> Classified {
    match negotiate(ours, hello.protocol) {
        Ok(negotiated) => {
            let older = match (
                semver::Version::parse(&hello.version),
                semver::Version::parse(desktop),
            ) {
                (Ok(d), Ok(us)) => d < us,
                _ => false,
            };
            Classified::Compatible {
                negotiated,
                upgrade_pending: managed && older,
            }
        }
        Err(_) => Classified::Incompatible {
            reason: if hello.protocol.max < ours.min {
                IncompatibleReason::Older
            } else {
                IncompatibleReason::Newer
            },
            message: incompatible_message(ours, hello),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(v: &str, min: u32, max: u32) -> Hello {
        Hello {
            protocol: ProtocolRange { min, max },
            version: v.into(),
            capabilities: vec![],
        }
    }
    const R11: ProtocolRange = ProtocolRange { min: 1, max: 1 };
    const R23: ProtocolRange = ProtocolRange { min: 2, max: 3 };

    #[test]
    fn classify_older_overlap_is_pending() {
        assert_eq!(
            classify("1.5.0", R11, &hello("1.4.2", 1, 1), true),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: true
            }
        );
    }

    #[test]
    fn classify_newer_is_compatible() {
        assert_eq!(
            classify("1.5.0", R11, &hello("1.6.0", 1, 2), true),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: false
            }
        );
    }

    #[test]
    fn classify_override_never_pending() {
        assert_eq!(
            classify("1.5.0", R11, &hello("1.0.0", 1, 1), false),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: false
            }
        );
    }

    #[test]
    fn classify_no_overlap_older() {
        let c = classify("1.5.0", R23, &hello("1.5.0", 1, 1), true);
        let Classified::Incompatible { reason, message } = c else {
            panic!("{c:?}")
        };
        assert_eq!(reason, IncompatibleReason::Older);
        assert!(
            message.contains("1..1") && message.contains("2..3"),
            "{message}"
        );
    }

    #[test]
    fn classify_no_overlap_newer() {
        let c = classify("1.5.0", R11, &hello("2.0.0", 2, 3), true);
        assert!(matches!(
            c,
            Classified::Incompatible {
                reason: IncompatibleReason::Newer,
                ..
            }
        ));
    }

    #[test]
    fn classify_prerelease_is_older() {
        assert_eq!(
            classify("1.5.0", R11, &hello("1.5.0-beta.1", 1, 1), true),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: true
            }
        );
        // Unparseable versions never claim an upgrade.
        assert_eq!(
            classify("1.5.0", R11, &hello("dev", 1, 1), true),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: false
            }
        );
    }
}
