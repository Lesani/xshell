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
/// Update authorization is semver precedence, never protocol-range direction.
pub fn update_eligible(
    desktop: &str,
    ours: Option<&BuildId>,
    daemon: &str,
    build: Option<&BuildId>,
) -> bool {
    match (
        semver::Version::parse(daemon),
        semver::Version::parse(desktop),
    ) {
        (Ok(d), Ok(us)) => match d.cmp_precedence(&us) {
            Ordering::Less => true,
            Ordering::Equal => ours.is_some_and(|b| !same_build(build, b)),
            Ordering::Greater => false,
        },
        _ => false,
    }
}

/// A Host that updates itself from the latest release (`daemon.update-latest`) is outdated
/// when that release is known and newer than its version (semver precedence; build metadata
/// ignored). An unknown latest release, or a version that does not parse, is never outdated.
pub fn release_outdated(latest: Option<&semver::Version>, daemon: &str) -> bool {
    match (latest, semver::Version::parse(daemon)) {
        (Some(l), Ok(d)) => l.cmp_precedence(&d).is_gt(),
        _ => false,
    }
}

/// A release-capable Host answered "current" while this Desktop's latest value was
/// `current.0` and the Host ran `current.1`: while both are still so, the offer is
/// withdrawn (the Desktop's value is ahead of what the release server gave the Host).
pub fn release_withdrawn(
    current: Option<&(semver::Version, String)>,
    latest: Option<&semver::Version>,
    daemon: &str,
) -> bool {
    current.is_some_and(|(l, v)| latest == Some(l) && v == daemon)
}

/// The update this Desktop offers for a Daemon of `daemon` (`build`): what it does, the
/// version it leads to, and who supplies it.
///
/// A release-capable Host (`daemon.update-latest`, reached over the Relay) is offered only
/// the latest release, and only when it is newer ([`release_outdated`]); never "Use this
/// Desktop's build". Any other Host keeps the rule of [`update_eligible`]: an older version
/// is an Update, another build of this version a Replace, both to this Desktop's version.
pub fn offered_update(
    desktop: &str,
    ours: Option<&BuildId>,
    latest: Option<&semver::Version>,
    release_capable: bool,
    daemon: &str,
    build: Option<&BuildId>,
) -> Option<(
    crate::status::UpdateAction,
    String,
    crate::status::UpdateSource,
)> {
    use crate::status::{UpdateAction, UpdateSource};
    if release_capable {
        return latest
            .filter(|_| release_outdated(latest, daemon))
            .map(|l| (UpdateAction::Update, l.to_string(), UpdateSource::Release));
    }
    if !update_eligible(desktop, ours, daemon, build) {
        return None;
    }
    let older = semver::Version::parse(daemon)
        .ok()
        .zip(semver::Version::parse(desktop).ok())
        .is_some_and(|(d, us)| d.cmp_precedence(&us).is_lt());
    let action = if older {
        UpdateAction::Update
    } else {
        UpdateAction::Replace
    };
    Some((action, desktop.to_string(), UpdateSource::Desktop))
}

pub fn same_build(daemon: Option<&BuildId>, ours: &BuildId) -> bool {
    daemon.is_some_and(|d| d.same(ours))
}

/// `managed == false` (a direct Daemon this Desktop may not upgrade) never reports an
/// upgrade as pending or a build as different.
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
    fn update_eligibility_requires_known_precedence_and_exact_difference() {
        let ours = b(A, None);
        for (version, build, eligible) in [
            ("1.4.0", None, true),
            ("1.5.0", None, true),
            ("1.5.0+meta", Some(ours.clone()), false),
            ("1.5.0", Some(b(B, None)), true),
            ("1.6.0", None, false),
            ("dev", None, false),
        ] {
            assert_eq!(
                update_eligible("1.5.0", Some(&ours), version, build.as_ref()),
                eligible
            );
        }
        assert!(!update_eligible("1.5.0", None, "1.5.0", None));
    }

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
        // A Daemon this Desktop may not upgrade (direct, not upgradable): never offered one.
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

    fn v(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    #[test]
    fn release_outdated_uses_precedence() {
        assert!(release_outdated(Some(&v("1.6.0-rjw.4")), "1.6.0-rjw.3"));
        assert!(!release_outdated(Some(&v("1.6.0-rjw.3")), "1.6.0-rjw.4"));
        assert!(!release_outdated(Some(&v("1.6.0-rjw.4")), "1.6.0-rjw.4"));
        // Build metadata does not order versions.
        assert!(!release_outdated(
            Some(&v("1.6.0-rjw.4+zzz")),
            "1.6.0-rjw.4+aaa"
        ));
        assert!(!release_outdated(Some(&v("1.6.0-rjw.4")), "1.6.0-rjw.4+x"));
        assert!(release_outdated(Some(&v("1.6.0")), "1.6.0-rjw.4"));
        // Unknown latest, or a version without order: never outdated.
        assert!(!release_outdated(None, "1.0.0"));
        assert!(!release_outdated(Some(&v("9.0.0")), "dev"));
    }

    #[test]
    fn offered_update_release_capable_never_replaces() {
        use crate::status::{UpdateAction, UpdateSource};
        let ours = b(A, None);
        let latest = v("1.6.0-rjw.5");
        // Latest unknown: nothing, whatever the Desktop's build.
        assert_eq!(
            offered_update("1.6.0-rjw.4", Some(&ours), None, true, "1.6.0-rjw.3", None),
            None
        );
        // Newer release: Update to it, from the release.
        assert_eq!(
            offered_update(
                "1.6.0-rjw.4",
                Some(&ours),
                Some(&latest),
                true,
                "1.6.0-rjw.4",
                Some(&b(B, None))
            ),
            Some((
                UpdateAction::Update,
                "1.6.0-rjw.5".to_string(),
                UpdateSource::Release
            ))
        );
        // Equal or older release, or the same version from another build: nothing (never a
        // Replace, never this Desktop's build).
        for (daemon, build) in [
            ("1.6.0-rjw.5", Some(b(B, None))),
            ("1.6.0-rjw.5", None),
            ("1.6.0-rjw.6", None),
            ("1.6.0-rjw.5+meta", Some(ours.clone())),
        ] {
            assert_eq!(
                offered_update(
                    "1.6.0-rjw.4",
                    Some(&ours),
                    Some(&latest),
                    true,
                    daemon,
                    build.as_ref()
                ),
                None,
                "{daemon}"
            );
        }
        // Even a Desktop newer than the Host offers nothing but the release.
        assert_eq!(
            offered_update(
                "1.7.0",
                Some(&ours),
                Some(&v("1.6.0-rjw.5")),
                true,
                "1.6.0-rjw.5",
                None
            ),
            None
        );
    }

    #[test]
    fn offered_update_without_capability_is_unchanged() {
        use crate::status::{UpdateAction, UpdateSource};
        let ours = b(A, None);
        let latest = v("9.0.0");
        for (version, build, offered) in [
            ("1.4.0", None, Some(UpdateAction::Update)),
            ("1.5.0", None, Some(UpdateAction::Replace)),
            ("1.5.0+meta", Some(ours.clone()), None),
            ("1.5.0", Some(b(B, None)), Some(UpdateAction::Replace)),
            ("1.6.0", None, None),
            ("dev", None, None),
        ] {
            // The latest release plays no part without the capability.
            for l in [None, Some(&latest)] {
                assert_eq!(
                    offered_update("1.5.0", Some(&ours), l, false, version, build.as_ref()),
                    offered.map(|a| (a, "1.5.0".to_string(), UpdateSource::Desktop)),
                    "{version}"
                );
            }
        }
        assert_eq!(
            offered_update("1.5.0", None, None, false, "1.5.0", None),
            None
        );
    }

    #[test]
    fn release_withdrawn_only_for_the_same_latest_and_host_version() {
        let l = v("1.6.0-rjw.5");
        let current = (l.clone(), "1.6.0-rjw.4".to_string());
        assert!(release_withdrawn(Some(&current), Some(&l), "1.6.0-rjw.4"));
        // Another latest value, or another Host version: offered again.
        assert!(!release_withdrawn(
            Some(&current),
            Some(&v("1.6.0-rjw.6")),
            "1.6.0-rjw.4"
        ));
        assert!(!release_withdrawn(Some(&current), Some(&l), "1.6.0-rjw.3"));
        assert!(!release_withdrawn(Some(&current), None, "1.6.0-rjw.4"));
        assert!(!release_withdrawn(None, Some(&l), "1.6.0-rjw.4"));
    }
}
