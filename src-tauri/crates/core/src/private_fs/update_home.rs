//! Read-only authorization of the update home's anchor, not of private descendants.
use super::win::{attributes, open, owner_is, refuse};
use crate::pipe::{current_user_sid, last_error, os_error, owned, raw, SecDesc, Sid};
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GENERIC_EXECUTE, GENERIC_READ};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, IsValidAcl, IsValidSecurityDescriptor,
    IsValidSid, IsWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid, ACL,
    DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION, PSID, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, OPEN_EXISTING,
    READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    volume: u32,
    index_high: u32,
    index_low: u32,
}

fn identity(h: &OwnedHandle) -> io::Result<DirectoryIdentity> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(raw(h), &mut info) } == 0 {
        return Err(last_error());
    }
    Ok(DirectoryIdentity {
        volume: info.dwVolumeSerialNumber,
        index_high: info.nFileIndexHigh,
        index_low: info.nFileIndexLow,
    })
}

fn process_profile() -> io::Result<PathBuf> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error());
    }
    let token = owned(token)?;
    let mut len = 0;
    if unsafe { GetUserProfileDirectoryW(raw(&token), std::ptr::null_mut(), &mut len) } != 0
        || unsafe { windows_sys::Win32::Foundation::GetLastError() } != ERROR_INSUFFICIENT_BUFFER
        || len == 0
    {
        return Err(io::Error::other(
            "cannot size the process-token profile path",
        ));
    }
    let mut path = vec![0u16; len as usize];
    if unsafe { GetUserProfileDirectoryW(raw(&token), path.as_mut_ptr(), &mut len) } == 0 {
        return Err(last_error());
    }
    let end = path
        .iter()
        .position(|&c| c == 0)
        .filter(|&n| n > 0)
        .ok_or_else(|| io::Error::other("invalid process-token profile path"))?;
    Ok(PathBuf::from(OsString::from_wide(&path[..end])))
}

fn real_directory(path: &Path) -> io::Result<OwnedHandle> {
    crate::server::no_reparse(path)?;
    let h = open(path, READ_CONTROL, OPEN_EXISTING, None)?
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    let attrs = attributes(&h)?;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 || attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(refuse(path, "is not a real directory"));
    }
    Ok(h)
}

fn privileged(sid: PSID) -> bool {
    !sid.is_null()
        && unsafe {
            IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0
        }
}

/// Conservative grant scan, not effective-access evaluation. A deny never excuses a
/// foreign mutation allow; unusual applicable ACE forms deliberately fail closed.
fn isolated_profile_dacl(sd: &SecDesc, me: &Sid) -> bool {
    if unsafe { IsValidSecurityDescriptor(sd.0) } == 0 {
        return false;
    }
    let (mut present, mut defaulted) = (0, 0);
    let mut acl: *mut ACL = std::ptr::null_mut();
    if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) } == 0
        || present == 0
        || acl.is_null()
        || unsafe { IsValidAcl(acl) } == 0
    {
        return false;
    }
    // The descriptor is OS-validated and owns this allocation. Parse bounded bytes
    // before exposing any ACE SID to Windows (SID length is encoded in byte 1).
    let bytes = unsafe { std::slice::from_raw_parts(acl.cast::<u8>(), (*acl).AclSize as usize) };
    if bytes.len() < std::mem::size_of::<ACL>() {
        return false;
    }
    let mut offset = std::mem::size_of::<ACL>();
    for _ in 0..unsafe { (*acl).AceCount } {
        let Some(header) = bytes.get(offset..offset + 4) else {
            return false;
        };
        let size = u16::from_le_bytes([header[2], header[3]]) as usize;
        if size < 4 || !size.is_multiple_of(4) {
            return false;
        }
        let Some(ace) = bytes.get(offset..offset + size) else {
            return false;
        };
        offset += size;
        if header[1] as u32 & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        // ACCESS_ALLOWED_ACE / ACCESS_DENIED_ACE have header, mask, then SID.
        if !matches!(header[0], 0 | 1) || ace.len() < 16 {
            return false;
        }
        let sid_len = 8 + 4 * ace[9] as usize;
        if sid_len != ace.len() - 8 {
            return false;
        }
        let sid = unsafe { ace.as_ptr().add(8) } as PSID;
        if unsafe { IsValidSid(sid) } == 0 {
            return false;
        }
        if header[0] == 1 {
            continue;
        }
        let mask = u32::from_le_bytes(ace[4..8].try_into().expect("bounded mask"));
        let read_only = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | GENERIC_READ | GENERIC_EXECUTE;
        if !me.equals(sid) && !privileged(sid) && mask & !read_only != 0 {
            return false;
        }
    }
    true
}

fn privileged_profile_policy(
    sd: &SecDesc,
    me: &Sid,
    candidate: DirectoryIdentity,
    profile: DirectoryIdentity,
) -> bool {
    if candidate != profile || unsafe { IsValidSecurityDescriptor(sd.0) } == 0 {
        return false;
    }
    let (mut owner, mut defaulted) = (std::ptr::null_mut(), 0);
    (unsafe { GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) } != 0)
        && privileged(owner)
        && isolated_profile_dacl(sd, me)
}

/// Current-user-owned homes retain their existing policy. A privileged-owned home is
/// accepted only when it is the actual process-token OS profile and has no observable
/// foreign mutation grants. This never repairs the home or authorizes private children.
pub fn check_update_home_anchor(path: &Path) -> io::Result<()> {
    let me = current_user_sid()?;
    let candidate = real_directory(path)?;
    if owner_is(&candidate, &me)? {
        return Ok(());
    }
    let profile = real_directory(&process_profile()?)?;
    let mut descriptor = std::ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            raw(&candidate),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(os_error(result));
    }
    let sd = SecDesc(descriptor);
    if !privileged_profile_policy(&sd, &me, identity(&candidate)?, identity(&profile)?) {
        return Err(refuse(
            path,
            "is not an isolated current-token profile or user-owned home",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::private_fs::{
        check_owned_dir, create_dir_with_sddl, create_file_with_sddl, ensure_dir, open_read,
        security_sddl, user_sid_string,
    };
    use std::process::Command;

    const CAPABILITY: &str = "S-1-15-3-65536-3852923496-3228631905-2361761901-1328441031-2492177081-392074989-583479735-4232454157";
    const FOREIGN: &str = "S-1-5-21-111-222-333-444";
    const ID: DirectoryIdentity = DirectoryIdentity {
        volume: 7,
        index_high: 8,
        index_low: 9,
    };

    fn policy(sddl: &str, candidate: DirectoryIdentity, profile: DirectoryIdentity) -> bool {
        privileged_profile_policy(
            &SecDesc::from_sddl(sddl).unwrap(),
            &current_user_sid().unwrap(),
            candidate,
            profile,
        )
    }

    fn admin_directory(path: &Path) -> bool {
        let sid = user_sid_string().unwrap();
        match create_dir_with_sddl(path, &format!("O:BAD:P(A;OICI;FA;;;{sid})")) {
            Ok(()) => true,
            Err(e) if std::env::var_os("GITHUB_ACTIONS").is_some() => {
                panic!("cannot create an Administrators-owned fixture: {e}")
            }
            Err(e) => {
                eprintln!("skipped: token cannot assign Administrators ownership: {e}");
                false
            }
        }
    }

    #[test]
    fn actual_process_profile_is_checked_without_security_mutation() {
        let profile = process_profile().unwrap();
        let before = security_sddl(&profile).unwrap();
        let h = real_directory(&profile).unwrap();
        eprintln!(
            "actual profile privileged-owner branch: {}",
            !owner_is(&h, &current_user_sid().unwrap()).unwrap()
        );
        let result = check_update_home_anchor(&profile);
        assert_eq!(security_sddl(&profile).unwrap(), before);
        result.unwrap();
    }

    #[test]
    fn arbitrary_user_owned_home_is_unchanged() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let sid = user_sid_string().unwrap();
        create_dir_with_sddl(&home, &format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FA;;;WD)")).unwrap();
        let before = security_sddl(&home).unwrap();
        check_update_home_anchor(&home).unwrap();
        assert_eq!(security_sddl(&home).unwrap(), before);
    }

    #[test]
    fn arbitrary_admin_owned_home_is_refused_without_mutation() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        if !admin_directory(&home) {
            return;
        }
        let before = security_sddl(&home).unwrap();
        let candidate = real_directory(&home).unwrap();
        let profile = real_directory(&process_profile().unwrap()).unwrap();
        assert_ne!(identity(&candidate).unwrap(), identity(&profile).unwrap());
        assert!(check_update_home_anchor(&home).is_err());
        assert!(!home.join(".xshell").exists());
        assert_eq!(security_sddl(&home).unwrap(), before);
    }

    #[test]
    fn environment_profile_spoof_cannot_authorize_foreign_home() {
        const CHILD: &str = "XSHELL_PROFILE_ANCHOR_TEST_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = PathBuf::from(path);
            assert_ne!(process_profile().unwrap(), path);
            assert!(check_update_home_anchor(&path).is_err());
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("spoof");
        if !admin_directory(&home) {
            return;
        }
        let before = security_sddl(&home).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "private_fs::update_home::tests::environment_profile_spoof_cannot_authorize_foreign_home", "--nocapture"])
            .env(CHILD, &home).env("HOME", &home).env("USERPROFILE", &home)
            .output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(security_sddl(&home).unwrap(), before);
    }

    #[test]
    fn privileged_profile_requires_owner_and_complete_handle_identity() {
        let safe = "O:BAD:P(A;;FA;;;BA)(A;;FA;;;SY)";
        assert!(policy(safe, ID, ID));
        assert!(policy("O:SYD:P(A;;FA;;;SY)", ID, ID));
        assert!(!policy("O:BUD:P(A;;FA;;;BA)", ID, ID));
        for other in [
            DirectoryIdentity { volume: 6, ..ID },
            DirectoryIdentity {
                index_high: 6,
                ..ID
            },
            DirectoryIdentity { index_low: 6, ..ID },
        ] {
            assert!(!policy(safe, other, ID), "{other:?}");
        }
        // Real disposable handles, not a path-string comparison.
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a");
        let b = t.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let ha = real_directory(&a).unwrap();
        let hb = real_directory(&b).unwrap();
        assert!(!policy(
            safe,
            identity(&ha).unwrap(),
            identity(&hb).unwrap()
        ));
    }

    #[test]
    fn profile_dacl_accepts_trusted_control_and_observed_capability_read() {
        let sid = user_sid_string().unwrap();
        assert!(policy(
            &format!(
                "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{sid})(A;;0x00100020;;;{CAPABILITY})"
            ),
            ID,
            ID
        ));
        for trustee in ["WD", "BU", "AU", FOREIGN, CAPABILITY] {
            for mask in ["FR", "FX", "GRGX", "0x001200a9"] {
                assert!(
                    policy(&format!("O:BAD:P(A;;{mask};;;{trustee})"), ID, ID),
                    "{trustee} {mask}"
                );
            }
        }
        assert!(policy("O:BAD:P", ID, ID)); // Empty is not NULL.
        assert!(policy("O:BAD:P(A;OICIIO;FA;;;WD)", ID, ID));
    }

    #[test]
    fn profile_dacl_rejects_every_foreign_mutation_bit_and_generic_write() {
        for trustee in ["WD", "BU", "AU", FOREIGN, CAPABILITY] {
            // add file/subdir, write EA/attrs, delete child/object, write DACL/owner,
            // generic write/all, and an unknown bit (not just a mutation blacklist).
            for mask in [
                2u32, 4, 0x10, 0x100, 0x40, 0x10000, 0x40000, 0x80000, 0x40000000, 0x10000000,
                0x00800000,
            ] {
                assert!(
                    !policy(&format!("O:BAD:P(A;;0x{mask:08x};;;{trustee})"), ID, ID),
                    "{trustee} {mask:x}"
                );
            }
        }
    }

    #[test]
    fn profile_dacl_deny_does_not_excuse_allow_and_inherited_write_applies() {
        assert!(!policy("O:BAD:P(D;;FA;;;WD)(A;;FW;;;WD)", ID, ID));
        assert!(!policy("O:BAD:P(A;ID;FW;;;WD)", ID, ID));
        assert!(policy("O:BAD:P(D;;FA;;;WD)(A;;FA;;;BA)", ID, ID));
        assert!(!policy("O:BAD:NO_ACCESS_CONTROL", ID, ID));
        assert!(!policy("O:BA", ID, ID));
        assert!(!policy("O:BAD:P(OA;;FR;;;WD)", ID, ID));
    }

    #[test]
    fn malformed_ace_layout_and_sid_are_refused() {
        for (byte, value) in [(2, 4), (9, 255)] {
            let sd = SecDesc::from_sddl("O:BAD:P(A;;FR;;;WD)").unwrap();
            let (mut present, mut defaulted) = (0, 0);
            let mut acl = std::ptr::null_mut();
            assert_ne!(
                unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) },
                0
            );
            // Only disposable allocated descriptors are corrupted, never filesystem ACLs.
            let ace = unsafe { acl.cast::<u8>().add(std::mem::size_of::<ACL>()) };
            unsafe { *ace.add(byte) = value };
            assert!(!isolated_profile_dacl(&sd, &current_user_sid().unwrap()));
        }
    }

    #[test]
    fn privileged_private_descendants_remain_refused_and_unchanged() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        ensure_dir(&home).unwrap();
        let private = home.join(".xshell");
        if !admin_directory(&private) {
            return;
        }
        let before = security_sddl(&private).unwrap();
        assert!(check_owned_dir(&private).is_err());
        assert!(crate::server::windows_server_root(&home).is_err());
        assert_eq!(security_sddl(&private).unwrap(), before);
        assert!(!private.join("server").exists());
        let sid = user_sid_string().unwrap();
        let file = home.join("selected");
        create_file_with_sddl(&file, &format!("O:BAD:P(A;;FA;;;{sid})")).unwrap();
        let before = security_sddl(&file).unwrap();
        assert!(open_read(&file).is_err());
        assert_eq!(security_sddl(&file).unwrap(), before);
        assert_eq!(std::fs::read(&file).unwrap(), b"");
    }

    #[test]
    fn final_and_ancestor_profile_junctions_are_refused_without_touching_target() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("target");
        ensure_dir(&target).unwrap();
        ensure_dir(&target.join("home")).unwrap();
        let before = security_sddl(&target).unwrap();
        let junction = t.path().join("junction");
        assert!(Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .unwrap()
            .success());
        assert!(check_update_home_anchor(&junction).is_err());
        assert!(check_update_home_anchor(&junction.join("home")).is_err());
        assert!(crate::server::windows_server_root(&junction.join("home")).is_err());
        assert_eq!(security_sddl(&target).unwrap(), before);
        assert!(!target.join("home/.xshell").exists());
    }
}
