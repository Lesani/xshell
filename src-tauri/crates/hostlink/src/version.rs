//! What a Daemon's `hello` means for this Desktop (ADR-0003): usable, usable with an upgrade
//! pending, usable from another build of this version (#49), or unusable until one side is
//! upgraded.

use std::cmp::Ordering;
use xshell_protocol::build::BuildId;
use xshell_protocol::msg::{Hello, ProtocolRange};
use xshell_protocol::negotiate::{negotiate, Older};

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
        /// Same version precedence, but another build (or an unknown one): "Replace with
        /// this Desktop's build" is offered. Never together with `upgrade_pending`.
        build_differs: bool,
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

/// Whether `daemon` is the build `ours`: an unknown Daemon build (every Daemon from before
/// #49) is never the same.
pub fn same_build(daemon: Option<&BuildId>, ours: &BuildId) -> bool {
    daemon.is_some_and(|d| d.same(ours))
}

/// `managed == false` (a daemon command override) never reports an upgrade as pending or a
/// build as different: the Desktop does not install or upgrade such a Daemon.
///
/// Versions are ordered by semver precedence (build metadata ignored). An older Daemon is
/// upgrade pending. At equal precedence a Daemon whose build is not `ours_build` differs;
/// when this Desktop's own build is unknown, no build ever differs. Versions that do not
/// parse have no order: neither flag.
pub fn classify(
    desktop: &str,
    ours_build: Option<&BuildId>,
    ours: ProtocolRange,
    hello: &Hello,
    managed: bool,
) -> Classified {
    match negotiate(ours, hello.protocol) {
        Ok(negotiated) => {
            let prec = match (
                semver::Version::parse(&hello.version),
                semver::Version::parse(desktop),
            ) {
                (Ok(d), Ok(us)) => Some(d.cmp_precedence(&us)),
                _ => None,
            };
            let build_differs = managed
                && prec == Some(Ordering::Equal)
                && ours_build.is_some_and(|b| !same_build(hello.build.as_ref(), b));
            Classified::Compatible {
                negotiated,
                upgrade_pending: managed && prec == Some(Ordering::Less),
                build_differs,
            }
        }
        Err(m) => Classified::Incompatible {
            reason: match m.older() {
                Older::Theirs => IncompatibleReason::Older,
                Older::Ours => IncompatibleReason::Newer,
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
            build: None,
        }
    }
    const R11: ProtocolRange = ProtocolRange { min: 1, max: 1 };
    const R23: ProtocolRange = ProtocolRange { min: 2, max: 3 };

    #[test]
    fn classify_older_overlap_is_pending() {
        assert_eq!(
            classify("1.5.0", None, R11, &hello("1.4.2", 1, 1), true),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: true,
                build_differs: false
            }
        );
    }

    #[test]
    fn classify_newer_is_compatible() {
        assert_eq!(
            classify("1.5.0", None, R11, &hello("1.6.0", 1, 2), true),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: false,
                build_differs: false
            }
        );
    }

    #[test]
    fn classify_override_never_pending() {
        assert_eq!(
            classify("1.5.0", None, R11, &hello("1.0.0", 1, 1), false),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: false,
                build_differs: false
            }
        );
    }

    #[test]
    fn classify_no_overlap_older() {
        let c = classify("1.5.0", None, R23, &hello("1.5.0", 1, 1), true);
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
        let c = classify("1.5.0", None, R11, &hello("2.0.0", 2, 3), true);
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
            classify("1.5.0", None, R11, &hello("1.5.0-beta.1", 1, 1), true),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: true,
                build_differs: false
            }
        );
        // Unparseable versions never claim an upgrade.
        assert_eq!(
            classify("1.5.0", None, R11, &hello("dev", 1, 1), true),
            Classified::Compatible {
                negotiated: 1,
                upgrade_pending: false,
                build_differs: false
            }
        );
    }

    fn b(commit: &str, dirty: Option<&str>) -> BuildId {
        BuildId {
            commit: commit.into(),
            dirty: dirty.map(Into::into),
            time: 7,
        }
    }

    fn with_build(v: &str, build: Option<BuildId>) -> Hello {
        Hello {
            build,
            ..hello(v, 1, 1)
        }
    }

    fn flags(c: Classified) -> (bool, bool) {
        match c {
            Classified::Compatible {
                upgrade_pending,
                build_differs,
                ..
            } => (upgrade_pending, build_differs),
            c => panic!("{c:?}"),
        }
    }

    const A: &str = "85ee920a1b2c";
    const B: &str = "000000000000";

    #[test]
    fn classify_builds() {
        let ours = b(A, None);
        let c = |v: &str, d: Option<BuildId>, managed| {
            flags(classify(
                "1.5.0",
                Some(&ours),
                R11,
                &with_build(v, d),
                managed,
            ))
        };
        // The same build (the time is diagnostic only).
        assert_eq!(
            c(
                "1.5.0",
                Some(BuildId {
                    time: 1,
                    ..ours.clone()
                }),
                true
            ),
            (false, false)
        );
        // Another commit, another dirty digest, or an unknown build (pre-#49): different.
        assert_eq!(c("1.5.0", Some(b(B, None)), true), (false, true));
        assert_eq!(c("1.5.0", Some(b(A, Some(B))), true), (false, true));
        assert_eq!(c("1.5.0", None, true), (false, true));
        // A Daemon command override is never offered a replacement.
        assert_eq!(c("1.5.0", None, false), (false, false));
        // Older stays upgrade pending only; newer is neither.
        assert_eq!(c("1.4.0", Some(b(B, None)), true), (true, false));
        assert_eq!(c("1.6.0", Some(b(B, None)), true), (false, false));
        // Build metadata does not order versions (`cmp_precedence`, not `Ord`).
        assert_eq!(c("1.5.0+x", Some(ours.clone()), true), (false, false));
        assert_eq!(c("1.5.0+a", None, true), (false, true));
        // Versions that do not parse have no order.
        assert_eq!(c("dev", None, true), (false, false));
    }

    #[test]
    fn classify_unknown_own_build_never_differs() {
        for d in [None, Some(b(B, None))] {
            assert_eq!(
                flags(classify("1.5.0", None, R11, &with_build("1.5.0", d), true)),
                (false, false)
            );
        }
    }

    /// `1.5.0+x` against `1.5.0`: semver `Ord` calls it newer; precedence calls it equal.
    #[test]
    fn classify_build_metadata_is_not_older() {
        assert_eq!(
            flags(classify(
                "1.5.0+zzz",
                None,
                R11,
                &hello("1.5.0+aaa", 1, 1),
                true
            )),
            (false, false)
        );
        assert_eq!(
            flags(classify("1.5.0", None, R11, &hello("1.5.0+x", 1, 1), true)),
            (false, false)
        );
    }
}
