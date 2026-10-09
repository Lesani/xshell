//! Private state on disk (Ring keys and Rosters) on Windows: the counterpart of a 0700
//! directory and 0600 files.
//!
//! A directory or file this module creates is created atomically with the user as owner
//! and a protected DACL that lets only the user in. An existing one is opened as itself,
//! never through a reparse point (a symlink or junction is refused), must be owned by the
//! user (another owner is refused), and gets that DACL again, replacing whatever it had.
//!
//! The descriptors are pure strings, so their tests run everywhere.

/// The descriptor of a private directory: owned by `sid`, a protected DACL with full
/// access for `sid` only, inherited by everything created inside.
pub fn dir_sddl(sid: &str) -> String {
    format!("O:{sid}D:P(A;OICI;FA;;;{sid})")
}

/// The descriptor of a private file: owned by `sid`, full access for `sid` only.
pub fn file_sddl(sid: &str) -> String {
    format!("O:{sid}D:P(A;;FA;;;{sid})")
}

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use super::*;
    use crate::pipe::{current_user_sid, last_error, os_error, owned, raw, wide, SecDesc, Sid};
    use std::fs::File;
    use std::io;
    use std::os::windows::io::OwnedHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        GetLastError, LocalFree, SetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SetSecurityInfo,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        CREATE_NEW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
    };

    struct Me {
        sid: Sid,
        text: String,
    }

    fn me() -> io::Result<Me> {
        let sid = current_user_sid()?;
        let text = sid.to_string_sid()?;
        Ok(Me { sid, text })
    }

    fn refuse(path: &Path, why: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {why}; refusing it", path.display()),
        )
    }

    /// Open `path` as itself (not what a reparse point names). `None`: it does not exist.
    fn open(
        path: &Path,
        access: u32,
        disposition: u32,
        sd: Option<&SecDesc>,
    ) -> io::Result<Option<OwnedHandle>> {
        let w = wide(path.as_os_str());
        let sa = sd.map(SecDesc::attributes);
        unsafe { SetLastError(0) };
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                sa.as_ref().map_or(std::ptr::null(), |a| a as *const _),
                disposition,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return match unsafe { GetLastError() } {
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(None),
                code => Err(os_error(code)),
            };
        }
        owned(h).map(Some)
    }

    fn attributes(h: &OwnedHandle) -> io::Result<u32> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(raw(h), &mut info) } == 0 {
            return Err(last_error());
        }
        Ok(info.dwFileAttributes)
    }

    fn owner_is(h: &OwnedHandle, sid: &Sid) -> io::Result<bool> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let r = unsafe {
            GetSecurityInfo(
                raw(h),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if r != 0 {
            return Err(os_error(r));
        }
        let same = !owner.is_null() && sid.equals(owner);
        unsafe { LocalFree(sd as _) };
        Ok(same)
    }

    /// Replace the DACL of `h` with the protected one of `sddl`.
    fn set_dacl(h: &OwnedHandle, sddl: &str) -> io::Result<()> {
        let sd = SecDesc::from_sddl(sddl)?;
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) } == 0
        {
            return Err(last_error());
        }
        let r = unsafe {
            SetSecurityInfo(
                raw(h),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if r != 0 {
            return Err(os_error(r));
        }
        Ok(())
    }

    /// An existing object behind `h`: the right kind, not a reparse point, ours; its DACL
    /// made private.
    fn secure(path: &Path, h: &OwnedHandle, dir: bool, me: &Me) -> io::Result<()> {
        let attrs = attributes(h)?;
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(refuse(path, "is a reparse point (a symlink or junction)"));
        }
        if (attrs & FILE_ATTRIBUTE_DIRECTORY != 0) != dir {
            return Err(refuse(
                path,
                if dir {
                    "is not a directory"
                } else {
                    "is not a regular file"
                },
            ));
        }
        if !owner_is(h, &me.sid)? {
            return Err(refuse(path, "is owned by another user"));
        }
        let sddl = if dir {
            dir_sddl(&me.text)
        } else {
            file_sddl(&me.text)
        };
        set_dacl(h, &sddl)
    }

    /// Create `dir` private (its parents as usual), or make an existing one private: refused
    /// when it is a reparse point, not a directory, or owned by someone else.
    pub fn ensure_dir(dir: &Path) -> io::Result<()> {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let me = me()?;
        let sd = SecDesc::from_sddl(&dir_sddl(&me.text))?;
        let sa = sd.attributes();
        let w = wide(dir.as_os_str());
        if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } != 0 {
            return Ok(());
        }
        match unsafe { GetLastError() } {
            ERROR_ALREADY_EXISTS => {}
            code => return Err(os_error(code)),
        }
        let h = open(dir, READ_CONTROL | WRITE_DAC, OPEN_EXISTING, None)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        secure(dir, &h, true, &me)
    }

    /// Open the private file `path` for reading, made private first. `None`: it does not
    /// exist.
    pub fn open_read(path: &Path) -> io::Result<Option<File>> {
        let me = me()?;
        let Some(h) = open(
            path,
            GENERIC_READ | READ_CONTROL | WRITE_DAC,
            OPEN_EXISTING,
            None,
        )?
        else {
            return Ok(None);
        };
        secure(path, &h, false, &me)?;
        Ok(Some(File::from(h)))
    }

    /// Create the new private file `path` for writing; fails when anything exists there.
    pub fn create_new(path: &Path) -> io::Result<File> {
        let me = me()?;
        let sd = SecDesc::from_sddl(&file_sddl(&me.text))?;
        open(path, GENERIC_WRITE, CREATE_NEW, Some(&sd))?
            .map(File::from)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    /// Open the private file `path` for reading and writing, created private when missing
    /// (a lock file).
    pub fn open_or_create(path: &Path) -> io::Result<File> {
        let me = me()?;
        let sd = SecDesc::from_sddl(&file_sddl(&me.text))?;
        let h = open(
            path,
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC,
            OPEN_ALWAYS,
            Some(&sd),
        )?
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            secure(path, &h, false, &me)?;
        }
        Ok(File::from(h))
    }

    /// The owner and DACL of `path` as SDDL (tests).
    #[doc(hidden)]
    pub fn security_sddl(path: &Path) -> io::Result<String> {
        let h = open(path, READ_CONTROL, OPEN_EXISTING, None)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let what = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let r = unsafe {
            GetSecurityInfo(
                raw(&h),
                SE_FILE_OBJECT,
                what,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if r != 0 {
            return Err(os_error(r));
        }
        let mut s: *mut u16 = std::ptr::null_mut();
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                what,
                &mut s,
                std::ptr::null_mut(),
            )
        };
        unsafe { LocalFree(sd as _) };
        if ok == 0 {
            return Err(last_error());
        }
        let len = (0..).take_while(|&i| unsafe { *s.add(i) } != 0).count();
        let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, len) });
        unsafe { LocalFree(s as _) };
        Ok(out)
    }

    /// `sddl` as Windows itself spells it (well-known SIDs as aliases), for comparisons
    /// (tests).
    #[doc(hidden)]
    pub fn canonical_sddl(sddl: &str) -> io::Result<String> {
        let sd = SecDesc::from_sddl(sddl)?;
        let what = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut s: *mut u16 = std::ptr::null_mut();
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd.0,
                SDDL_REVISION_1,
                what,
                &mut s,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(last_error());
        }
        let len = (0..).take_while(|&i| unsafe { *s.add(i) } != 0).count();
        let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, len) });
        unsafe { LocalFree(s as _) };
        Ok(out)
    }

    /// Create `path` (a directory) with the descriptor `sddl` (tests).
    #[doc(hidden)]
    pub fn create_dir_with_sddl(path: &Path, sddl: &str) -> io::Result<()> {
        let sd = SecDesc::from_sddl(sddl)?;
        let sa = sd.attributes();
        let w = wide(path.as_os_str());
        if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// Create `path` (a file) with the descriptor `sddl` (tests).
    #[doc(hidden)]
    pub fn create_file_with_sddl(path: &Path, sddl: &str) -> io::Result<File> {
        let sd = SecDesc::from_sddl(sddl)?;
        open(path, GENERIC_WRITE, CREATE_NEW, Some(&sd))?
            .map(File::from)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    /// This user's SID string (tests).
    #[doc(hidden)]
    pub fn user_sid_string() -> io::Result<String> {
        Ok(me()?.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_descriptors() {
        assert_eq!(
            dir_sddl("S-1-5-21-9"),
            "O:S-1-5-21-9D:P(A;OICI;FA;;;S-1-5-21-9)"
        );
        assert_eq!(
            file_sddl("S-1-5-21-9"),
            "O:S-1-5-21-9D:P(A;;FA;;;S-1-5-21-9)"
        );
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::*;
    use std::io::{Read, Write};

    /// Only the DACL part of an SDDL string, without the auto-inherited flags Windows adds
    /// (`D:PAI(…)` reads as `D:P(…)`).
    fn dacl(sddl: &str) -> String {
        let d = &sddl[sddl.find("D:").expect("a DACL")..];
        let flags_end = d.find('(').unwrap_or(d.len());
        let flags: String = d[2..flags_end].replace("AI", "").replace("AR", "");
        format!("D:{flags}{}", &d[flags_end..])
    }

    /// The DACL `sddl` describes, as Windows spells it.
    fn want(sddl: &str) -> String {
        dacl(&canonical_sddl(sddl).unwrap())
    }

    /// The owner part of an SDDL string.
    fn owner(sddl: &str) -> String {
        let o = &sddl[sddl.find("O:").expect("an owner") + 2..];
        o[..o.find("D:").unwrap_or(o.len())].to_string()
    }

    #[test]
    fn new_dir_and_files_are_private() {
        let t = tempfile::tempdir().unwrap();
        let sid = user_sid_string().unwrap();
        let d = t.path().join("ring");
        ensure_dir(&d).unwrap();
        let got = security_sddl(&d).unwrap();
        assert_eq!(
            owner(&got),
            owner(&canonical_sddl(&dir_sddl(&sid)).unwrap()),
            "{got}"
        );
        assert_eq!(dacl(&got), want(&dir_sddl(&sid)), "{got}");
        let f = d.join("keys.json");
        create_new(&f).unwrap().write_all(b"x").unwrap();
        assert!(create_new(&f).is_err(), "create_new replaced a file");
        let got = security_sddl(&f).unwrap();
        assert_eq!(dacl(&got), want(&file_sddl(&sid)), "{got}");
        let mut s = String::new();
        open_read(&f)
            .unwrap()
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "x");
        assert!(open_read(&d.join("none")).unwrap().is_none());
        // Again on an existing directory: still private.
        ensure_dir(&d).unwrap();
        let lock = d.join("ring.lock");
        open_or_create(&lock).unwrap();
        open_or_create(&lock).unwrap();
        assert_eq!(dacl(&security_sddl(&lock).unwrap()), want(&file_sddl(&sid)));
    }

    /// An existing directory and file that Everyone may read are tightened to the user.
    #[test]
    fn existing_broad_dir_and_file_are_tightened() {
        let t = tempfile::tempdir().unwrap();
        let sid = user_sid_string().unwrap();
        let d = t.path().join("ring");
        create_dir_with_sddl(
            &d,
            &format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FR;;;WD)"),
        )
        .unwrap();
        let f = d.join("roster.json");
        create_file_with_sddl(&f, &format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FR;;;WD)"))
            .unwrap()
            .write_all(b"r")
            .unwrap();
        assert!(dacl(&security_sddl(&f).unwrap()).contains(";;;WD)"));
        ensure_dir(&d).unwrap();
        assert_eq!(dacl(&security_sddl(&d).unwrap()), want(&dir_sddl(&sid)));
        let mut s = String::new();
        open_read(&f)
            .unwrap()
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "r");
        assert_eq!(dacl(&security_sddl(&f).unwrap()), want(&file_sddl(&sid)));
    }

    /// A directory owned by someone else (Administrators, where this token may assign that
    /// owner; CI runs elevated) is refused, its DACL untouched.
    #[test]
    fn foreign_owned_dir_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let sid = user_sid_string().unwrap();
        let d = t.path().join("ring");
        let sddl = format!("O:BAD:P(A;OICI;FA;;;{sid})(A;OICI;FR;;;WD)");
        match create_dir_with_sddl(&d, &sddl) {
            Ok(()) => {}
            Err(e) if std::env::var_os("GITHUB_ACTIONS").is_some() => {
                panic!("cannot create a directory owned by Administrators: {e}")
            }
            Err(e) => {
                eprintln!("skipped: this token cannot make Administrators the owner: {e}");
                return;
            }
        }
        let e = ensure_dir(&d).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
        assert!(dacl(&security_sddl(&d).unwrap()).contains(";;;WD)"));
    }

    /// A junction where the directory should be, and a symlink where a file should be, are
    /// refused, never followed.
    #[test]
    fn reparse_points_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keys.json"), "x").unwrap();
        let d = t.path().join("ring");
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&d)
            .arg(&target)
            .status()
            .unwrap();
        assert!(st.success());
        let e = ensure_dir(&d).unwrap_err();
        assert!(e.to_string().contains("reparse point"), "{e}");
        if std::os::windows::fs::symlink_file(target.join("keys.json"), t.path().join("k")).is_ok()
        {
            let e = open_read(&t.path().join("k")).unwrap_err();
            assert!(e.to_string().contains("reparse point"), "{e}");
        }
    }
}
