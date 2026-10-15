//! Build identity (Lesani/xshell#49): which build of a version a Desktop or Daemon is. The
//! `version` stays the plain release number; a [`BuildId`] names the commit, a digest of the
//! uncommitted changes (if any) and the commit time. Unknown (built without git): `None`.
//!
//! Each binary embeds its identity as a stamp string written by `xshell-buildinfo`:
//! `@@XSHELL-BUILD-V1:<display>:<time>@@`, or `@@XSHELL-BUILD-V1:unknown@@`. [`scan_stamp`]
//! finds it in a binary's bytes, so a binary for another platform can be checked without
//! running it.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The start of an embedded stamp (`@@XSHELL-BUILD-V1:`), each byte plus one. The binary that
/// carries this scanner must never contain the prefix itself: the linker packs string
/// constants back to back, so the prefix, `unknown` and `@@` could line up into a stamp.
const PREFIX_SHIFTED: [u8; 18] = shift(b"@@XSHELL-BUILD-V1:");
const STAMP_END: &str = "@@";
const UNKNOWN: &str = "unknown";

const fn shift<const N: usize>(s: &[u8; N]) -> [u8; N] {
    let mut out = [0; N];
    let mut i = 0;
    while i < N {
        out[i] = s[i] + 1;
        i += 1;
    }
    out
}

fn prefix_at(bytes: &[u8]) -> bool {
    bytes.len() >= PREFIX_SHIFTED.len()
        && bytes
            .iter()
            .zip(PREFIX_SHIFTED)
            .all(|(b, p)| b.wrapping_add(1) == p)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BuildId {
    /// The first 12 lowercase hex digits of the commit.
    pub commit: String,
    /// 12 lowercase hex digits of the digest of the uncommitted changes; `None` when clean.
    pub dirty: Option<String>,
    /// The commit's time, unix seconds. Diagnostic only: [`BuildId::same`] ignores it.
    pub time: u64,
}

impl BuildId {
    /// `85ee920a1b2c`, or `85ee920a1b2c+d.3f9a01c2b4e5` with uncommitted changes.
    pub fn display(&self) -> String {
        self.to_string()
    }

    /// The same build: commit and digest equal (the time is diagnostic only).
    pub fn same(&self, other: &BuildId) -> bool {
        self.commit == other.commit && self.dirty == other.dirty
    }

    /// Parses [`BuildId::display`]'s form plus a time.
    fn parse_display(display: &str, time: u64) -> Option<BuildId> {
        let (commit, dirty) = match display.split_once("+d.") {
            Some((c, d)) => (c, Some(d)),
            None => (display, None),
        };
        if !is_hex12(commit) || !dirty.is_none_or(is_hex12) {
            return None;
        }
        Some(BuildId {
            commit: commit.to_string(),
            dirty: dirty.map(str::to_string),
            time,
        })
    }
}

impl fmt::Display for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.dirty {
            Some(d) => write!(f, "{}+d.{d}", self.commit),
            None => f.write_str(&self.commit),
        }
    }
}

fn is_hex12(s: &str) -> bool {
    s.len() == 12 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The directory a Daemon build is installed in under `~/.xshell/server/`: `<version>` when
/// the build is unknown, else `<version>+<display>`.
pub fn install_dir_name(version: &str, build: Option<&BuildId>) -> String {
    match build {
        Some(b) => format!("{version}+{b}"),
        None => version.to_string(),
    }
}

/// The identity a stamp string names: `Ok(None)` for `unknown`, `Err` when `s` is not
/// exactly one stamp.
pub fn parse_stamp(s: &str) -> Result<Option<BuildId>, String> {
    match match_at(s.as_bytes(), 0) {
        Some((b, len)) if len == s.len() => Ok(b),
        _ => Err(format!("not a build stamp: {s:?}")),
    }
}

/// The identity stamped into a binary. Every full match must agree; no match is an error (a
/// binary from before #49, or not an xshell binary at all). A universal (fat) Mach-O binary is
/// checked slice by slice: every architecture must carry the same stamp, so one stamped slice
/// cannot vouch for another.
pub fn scan_stamp(bytes: &[u8]) -> Result<Option<BuildId>, String> {
    let Some(slices) = fat_slices(bytes)? else {
        return scan_flat(bytes);
    };
    let mut found: Option<Option<BuildId>> = None;
    for (i, (cpu, slice)) in slices.iter().enumerate() {
        let b = scan_flat(slice).map_err(|e| format!("slice {i} (cpu type {cpu:#x}): {e}"))?;
        match &found {
            None => found = Some(b),
            Some(f) if *f == b => {}
            Some(f) => {
                return Err(format!(
                    "slices disagree: {} and {}",
                    show(f.as_ref()),
                    show(b.as_ref())
                ))
            }
        }
    }
    found.ok_or_else(|| "a universal binary without slices".to_string())
}

const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
/// More slices than any real universal binary has; guards against other `cafebabe` files.
const FAT_MAX_SLICES: u32 = 32;

/// One architecture of a universal binary: its cpu type and bytes.
type Slice<'a> = (u32, &'a [u8]);

/// The slices of a fat Mach-O binary; `None` when `bytes` is not one.
fn fat_slices(bytes: &[u8]) -> Result<Option<Vec<Slice<'_>>>, String> {
    let be32 = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let be64 = |at: usize| -> Option<u64> {
        Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
    };
    let wide = match be32(0) {
        Some(FAT_MAGIC) => false,
        Some(FAT_MAGIC_64) => true,
        _ => return Ok(None),
    };
    let bad = || "malformed universal binary header".to_string();
    let n = be32(4).ok_or_else(bad)?;
    if n == 0 || n > FAT_MAX_SLICES {
        return Err(bad());
    }
    let entry = if wide { 32 } else { 20 };
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let at = 8 + i * entry;
        let cpu = be32(at).ok_or_else(bad)?;
        let (offset, size) = if wide {
            (
                be64(at + 8).ok_or_else(bad)?,
                be64(at + 16).ok_or_else(bad)?,
            )
        } else {
            (
                u64::from(be32(at + 8).ok_or_else(bad)?),
                u64::from(be32(at + 12).ok_or_else(bad)?),
            )
        };
        let start = usize::try_from(offset).map_err(|_| bad())?;
        let end = start
            .checked_add(usize::try_from(size).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        out.push((cpu, bytes.get(start..end).ok_or_else(bad)?));
    }
    Ok(Some(out))
}

fn scan_flat(bytes: &[u8]) -> Result<Option<BuildId>, String> {
    let mut found: Option<Option<BuildId>> = None;
    let mut i = 0;
    while let Some(off) = (i..bytes.len()).position(|j| prefix_at(&bytes[j..])) {
        let at = i + off;
        match match_at(bytes, at) {
            Some((b, len)) => {
                match &found {
                    None => found = Some(b),
                    Some(f) if *f == b => {}
                    Some(f) => {
                        return Err(format!(
                            "ambiguous build stamps: {} and {}",
                            show(f.as_ref()),
                            show(b.as_ref())
                        ))
                    }
                }
                i = at + len;
            }
            None => i = at + 1,
        }
    }
    found.ok_or_else(|| "no build stamp".to_string())
}

fn show(b: Option<&BuildId>) -> String {
    b.map_or_else(|| UNKNOWN.to_string(), |b| b.to_string())
}

/// A full stamp starting at `at`: its identity and length.
fn match_at(bytes: &[u8], at: usize) -> Option<(Option<BuildId>, usize)> {
    let rest = bytes.get(at..)?;
    if !prefix_at(rest) {
        return None;
    }
    let head = PREFIX_SHIFTED.len();
    let rest = &rest[head..];
    let unknown = [UNKNOWN, STAMP_END].concat();
    if rest.starts_with(unknown.as_bytes()) {
        return Some((None, head + unknown.len()));
    }
    // `<display>:<time>@@`: display is at most 12 + 3 + 12 bytes, time at most 20 digits.
    let window = &rest[..rest.len().min(27 + 1 + 20 + STAMP_END.len())];
    let colon = window.iter().position(|b| *b == b':')?;
    let display = std::str::from_utf8(&window[..colon]).ok()?;
    let after = &window[colon + 1..];
    let digits = after.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || !after[digits..].starts_with(STAMP_END.as_bytes()) {
        return None;
    }
    let time: u64 = std::str::from_utf8(&after[..digits]).ok()?.parse().ok()?;
    let b = BuildId::parse_display(display, time)?;
    Some((Some(b), head + colon + 1 + digits + STAMP_END.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(commit: &str, dirty: Option<&str>, time: u64) -> BuildId {
        BuildId {
            commit: commit.into(),
            dirty: dirty.map(Into::into),
            time,
        }
    }

    #[test]
    fn display_same_and_dir_name() {
        let clean = id("85ee920a1b2c", None, 1);
        let dirty = id("85ee920a1b2c", Some("3f9a01c2b4e5"), 1);
        assert_eq!(clean.display(), "85ee920a1b2c");
        assert_eq!(dirty.display(), "85ee920a1b2c+d.3f9a01c2b4e5");
        assert!(clean.same(&id("85ee920a1b2c", None, 2)), "time is ignored");
        assert!(!clean.same(&dirty));
        assert!(!clean.same(&id("000000000000", None, 1)));
        assert_eq!(install_dir_name("1.5.0", None), "1.5.0");
        assert_eq!(
            install_dir_name("1.5.0", Some(&dirty)),
            "1.5.0+85ee920a1b2c+d.3f9a01c2b4e5"
        );
    }

    #[test]
    fn the_shifted_prefix_is_the_prefix() {
        assert!(prefix_at(b"@@XSHELL-BUILD-V1:"));
        assert!(!prefix_at(b"@@XSHELL-BUILD-V1"));
        assert!(!prefix_at(b"@@XSHELL-BUILD-V2:"));
    }

    #[test]
    fn stamp_round_trip() {
        let dirty = id("85ee920a1b2c", Some("3f9a01c2b4e5"), 1760000000);
        let s = format!("@@XSHELL-BUILD-V1:{}:{}@@", dirty.display(), dirty.time);
        assert_eq!(parse_stamp(&s), Ok(Some(dirty)));
        assert_eq!(parse_stamp("@@XSHELL-BUILD-V1:unknown@@"), Ok(None));
        for bad in [
            "@@XSHELL-BUILD-V1:85ee920a1b2c:1760000000@",
            "@@XSHELL-BUILD-V1:85ee920a1b2c:1760000000@@x",
            "@@XSHELL-BUILD-V1:85EE920A1B2C:1@@",
            "@@XSHELL-BUILD-V1:85ee920a1b2c:@@",
            "@@XSHELL-BUILD-V1:85ee920a1b2c+d.3f9a:1@@",
            "@@XSHELL-BUILD-V1::1@@",
            "@@XSHELL-BUILD-V1:",
            "",
        ] {
            assert!(parse_stamp(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn scan_finds_one_agreeing_stamp() {
        let one = b"\x00junk@@XSHELL-BUILD-V1:85ee920a1b2c:1760000000@@\x00more";
        assert_eq!(
            scan_stamp(one),
            Ok(Some(id("85ee920a1b2c", None, 1760000000)))
        );
        // The bare prefix (a scanner's own constant) and junk after it are not stamps.
        let mut twice = b"@@XSHELL-BUILD-V1:\x00@@XSHELL-BUILD-V1:zzz@@".to_vec();
        twice.extend_from_slice(one);
        twice.extend_from_slice(one);
        assert_eq!(scan_stamp(&twice), scan_stamp(one));
        assert_eq!(
            scan_stamp(b"x@@XSHELL-BUILD-V1:unknown@@y"),
            Ok(None),
            "unknown is a stamp too"
        );
        assert_eq!(scan_stamp(b"no stamp here"), Err("no build stamp".into()));
        assert_eq!(
            scan_stamp(b"@@XSHELL-BUILD-V1:"),
            Err("no build stamp".into())
        );
        let two = [
            &one[..],
            b"@@XSHELL-BUILD-V1:000000000000:1@@",
            b"@@XSHELL-BUILD-V1:unknown@@",
        ]
        .concat();
        assert!(scan_stamp(&two).unwrap_err().contains("ambiguous"));
        // A stamp right at the end of the bytes.
        assert_eq!(
            scan_stamp(b"@@XSHELL-BUILD-V1:85ee920a1b2c+d.3f9a01c2b4e5:5@@"),
            Ok(Some(id("85ee920a1b2c", Some("3f9a01c2b4e5"), 5)))
        );
    }

    /// A universal binary: a fat header (32- or 64-bit entries), then each slice.
    fn fat(wide: bool, slices: &[&[u8]]) -> Vec<u8> {
        let entry = if wide { 32 } else { 20 };
        let magic: u32 = if wide { 0xcafe_babf } else { 0xcafe_babe };
        let mut head = magic.to_be_bytes().to_vec();
        head.extend((slices.len() as u32).to_be_bytes());
        let mut body = Vec::new();
        let mut offset = 8 + entry * slices.len();
        for (i, s) in slices.iter().enumerate() {
            head.extend((0x0100_0007u32 + i as u32).to_be_bytes()); // cputype
            head.extend(3u32.to_be_bytes()); // cpusubtype
            if wide {
                head.extend((offset as u64).to_be_bytes());
                head.extend((s.len() as u64).to_be_bytes());
                head.extend(0u32.to_be_bytes()); // align
                head.extend(0u32.to_be_bytes()); // reserved
            } else {
                head.extend((offset as u32).to_be_bytes());
                head.extend((s.len() as u32).to_be_bytes());
                head.extend(0u32.to_be_bytes()); // align
            }
            body.extend_from_slice(s);
            offset += s.len();
        }
        head.extend(body);
        head
    }

    #[test]
    fn universal_binaries_need_a_stamp_in_every_slice() {
        let stamped: &[u8] = b"\x7fcode@@XSHELL-BUILD-V1:85ee920a1b2c:1760000000@@code";
        let other: &[u8] = b"\x7fcode@@XSHELL-BUILD-V1:000000000000:1760000000@@code";
        let bare: &[u8] = b"\x7fcode without a stamp";
        let want = Ok(Some(id("85ee920a1b2c", None, 1760000000)));
        for wide in [false, true] {
            assert_eq!(scan_stamp(&fat(wide, &[stamped, stamped])), want);
            // One stamped slice must not vouch for an unstamped one, in either order.
            let e = scan_stamp(&fat(wide, &[stamped, bare])).unwrap_err();
            assert!(e.contains("slice 1") && e.contains("no build stamp"), "{e}");
            let e = scan_stamp(&fat(wide, &[bare, stamped])).unwrap_err();
            assert!(e.contains("slice 0"), "{e}");
            let e = scan_stamp(&fat(wide, &[stamped, other])).unwrap_err();
            assert!(e.contains("disagree"), "{e}");
        }
        // A header pointing past the end, or with no slices, is refused.
        let mut cut = fat(false, &[stamped, stamped]);
        cut.truncate(cut.len() - 4);
        assert!(scan_stamp(&cut).unwrap_err().contains("malformed"));
        assert!(scan_stamp(&fat(false, &[]))
            .unwrap_err()
            .contains("malformed"));
    }

    #[test]
    fn json_shape() {
        let b = id("85ee920a1b2c", None, 1760000000);
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"commit":"85ee920a1b2c","dirty":null,"time":1760000000}"#
        );
        let back: BuildId =
            serde_json::from_str(r#"{"commit":"85ee920a1b2c","dirty":"3f9a01c2b4e5","time":1}"#)
                .unwrap();
        assert_eq!(back.display(), "85ee920a1b2c+d.3f9a01c2b4e5");
    }
}
