use crate::msg::ProtocolRange;
use std::fmt;

/// The two sides share no protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mismatch {
    pub ours: ProtocolRange,
    pub theirs: ProtocolRange,
}

/// Which side must update to close a [`Mismatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Older {
    Ours,
    Theirs,
}

impl Mismatch {
    /// The side that speaks only older versions: `Theirs` when their newest version is below
    /// our oldest, else `Ours`.
    pub fn older(&self) -> Older {
        if self.theirs.max < self.ours.min {
            Older::Theirs
        } else {
            Older::Ours
        }
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no common protocol version: xshelld speaks {}, peer speaks {}",
            self.ours, self.theirs
        )
    }
}

impl std::error::Error for Mismatch {}

/// The highest version both ranges contain.
pub fn negotiate(ours: ProtocolRange, theirs: ProtocolRange) -> Result<u32, Mismatch> {
    let hi = ours.max.min(theirs.max);
    let lo = ours.min.max(theirs.min);
    if lo <= hi {
        Ok(hi)
    } else {
        Err(Mismatch { ours, theirs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(min: u32, max: u32) -> ProtocolRange {
        ProtocolRange { min, max }
    }

    #[test]
    fn picks_highest_common() {
        assert_eq!(negotiate(r(1, 3), r(2, 5)), Ok(3));
    }

    #[test]
    fn exact_overlap() {
        assert_eq!(negotiate(r(1, 1), r(1, 1)), Ok(1));
    }

    #[test]
    fn no_overlap() {
        let e = negotiate(r(1, 1), r(2, 3)).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("1..1") && s.contains("2..3"), "{s}");
    }

    #[test]
    fn older_side_theirs() {
        assert_eq!(
            negotiate(r(2, 3), r(1, 1)).unwrap_err().older(),
            Older::Theirs
        );
    }

    #[test]
    fn older_side_ours() {
        assert_eq!(
            negotiate(r(1, 1), r(2, 4)).unwrap_err().older(),
            Older::Ours
        );
    }
}
