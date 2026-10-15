//! This Desktop's build identity (Lesani/xshell#49), from `build.rs` (`xshell-buildinfo`). The
//! xshelld built from the same tree has the same one.

use std::sync::LazyLock;
use xshell_protocol::build::BuildId;

/// The stamp embedded in the binary: `@@XSHELL-BUILD-V1:<display>:<time>@@`, or
/// `@@XSHELL-BUILD-V1:unknown@@`. Found in the bytes by `xshell_protocol::build::scan_stamp`.
pub const STAMP: &str = env!("XSHELL_BUILDINFO_STAMP");

/// Keeps [`STAMP`] in the binary even where nothing reads it at run time.
#[used]
static KEEP: &str = STAMP;

/// This build, or `None` when it was built without git.
pub static BUILD: LazyLock<Option<BuildId>> =
    LazyLock::new(|| xshell_protocol::build::parse_stamp(STAMP).unwrap_or(None));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_stamp_is_well_formed() {
        let parsed = xshell_protocol::build::parse_stamp(STAMP).expect("a stamp");
        assert_eq!(*BUILD, parsed);
        assert_eq!(
            xshell_protocol::build::scan_stamp(STAMP.as_bytes()),
            Ok(parsed.clone())
        );
        match &parsed {
            None => assert_eq!(STAMP, "@@XSHELL-BUILD-V1:unknown@@"),
            Some(b) => assert_eq!(
                STAMP,
                format!("@@XSHELL-BUILD-V1:{}:{}@@", b.display(), b.time)
            ),
        }
        let _ = KEEP;
    }
}
