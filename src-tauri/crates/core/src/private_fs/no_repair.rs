//! Non-repairing validation of existing private state (BINDING AMENDMENT 1 §2): a broad,
//! foreign or reparse-point object is refused with its DACL and content unchanged. Only a
//! missing object is created, atomically with the private descriptor.
use super::update_home::{dacl_grants_within, descriptor_of, privileged_profile, real_directory};
use super::win::{attributes, open, owner_is, refuse};
use super::{dir_sddl, file_sddl};
use crate::pipe::{current_user_sid, last_error, os_error, wide, SecDesc, Sid};
use std::fs::File;
use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_ALREADY_EXISTS, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, MoveFileExW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    OPEN_ALWAYS, OPEN_EXISTING, READ_CONTROL,
};

/// Traverse/synchronize only: what Windows itself grants service capabilities on a
/// private directory's ancestors. Never list, read, write or change permissions.
const PRIVATE_FOREIGN: u32 = 0x0010_0000; // SYNCHRONIZE
/// Shared ancestry may be readable by others but must not grant them mutation/rename.
const SHARED_FOREIGN: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | GENERIC_READ | GENERIC_EXECUTE;

/// How much of an existing directory others may see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exposure {
    /// Home and `.xshell`: readable, never writable, by others. The owner is the user or,
    /// for the actual process-token profile only, a privileged system principal.
    Shared,
    /// Private Store directories: owned by the user, no access for others.
    Private,
}

fn dacl_ok(h: &OwnedHandle, me: &Sid, exposure: Exposure) -> io::Result<bool> {
    let sd = descriptor_of(h)?;
    Ok(dacl_grants_within(
        &sd,
        me,
        match exposure {
            Exposure::Shared => SHARED_FOREIGN,
            Exposure::Private => PRIVATE_FOREIGN,
        },
    ))
}

/// Validate the existing directory `path` without changing it: no reparse point on the way,
/// a real directory, an accepted owner and no grant beyond `exposure`.
pub fn check_dir_no_repair(path: &Path, exposure: Exposure) -> io::Result<()> {
    let me = current_user_sid()?;
    let h = real_directory(path)?;
    if owner_is(&h, &me)? {
        if !dacl_ok(&h, &me, exposure)? {
            return Err(refuse(path, "grants other users access"));
        }
        return Ok(());
    }
    if exposure == Exposure::Shared && privileged_profile(&h, &me)? {
        return Ok(());
    }
    Err(refuse(path, "is owned by another user"))
}

/// An existing private file behind `h`: regular, not a reparse point, the user's, private.
fn check_file(path: &Path, h: &OwnedHandle, me: &Sid) -> io::Result<()> {
    let attrs = attributes(h)?;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(refuse(path, "is a reparse point (a symlink or junction)"));
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(refuse(path, "is not a regular file"));
    }
    if !owner_is(h, me)? {
        return Err(refuse(path, "is owned by another user"));
    }
    if !dacl_ok(h, me, Exposure::Private)? {
        return Err(refuse(path, "grants other users access"));
    }
    Ok(())
}

/// Open the existing private file `path` for reading, refusing (never repairing) a broad,
/// foreign or reparse-point file. `None`: it does not exist.
pub fn open_read_no_repair(path: &Path) -> io::Result<Option<File>> {
    let me = current_user_sid()?;
    let Some(h) = open(path, GENERIC_READ | READ_CONTROL, OPEN_EXISTING, None)? else {
        return Ok(None);
    };
    check_file(path, &h, &me)?;
    Ok(Some(File::from(h)))
}

/// Open the lock file `path` for reading and writing: created private when missing, an
/// existing one validated without repair.
pub fn open_lock_no_repair(path: &Path) -> io::Result<File> {
    let me = current_user_sid()?;
    let sd = SecDesc::from_sddl(&file_sddl(&me.to_string_sid()?))?;
    unsafe { SetLastError(0) };
    let h = open(
        path,
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
        OPEN_ALWAYS,
        Some(&sd),
    )?
    .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    // OPEN_ALWAYS reports an existing file through the last error; validate it either way.
    let _existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    check_file(path, &h, &me)?;
    Ok(File::from(h))
}

/// Create the directory `path` with the private descriptor. `false`: something already
/// exists there (validate it with [`check_dir_no_repair`]; nothing is changed).
pub fn create_dir_private(path: &Path) -> io::Result<bool> {
    let me = current_user_sid()?;
    let sd = SecDesc::from_sddl(&dir_sddl(&me.to_string_sid()?))?;
    let sa = sd.attributes();
    let w = wide(path.as_os_str());
    if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } != 0 {
        return Ok(true);
    }
    match unsafe { GetLastError() } {
        ERROR_ALREADY_EXISTS => Ok(false),
        code => Err(os_error(code)),
    }
}

/// Atomically replace `to` with the flushed private file `from`, the rename itself written
/// through to disk before returning (`MOVEFILE_WRITE_THROUGH`).
pub fn replace_write_through(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (wide(from.as_os_str()), wide(to.as_os_str()));
    if unsafe {
        MoveFileExW(
            f.as_ptr(),
            t.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(())
}

/// A file identity (volume serial, file index) for replacement checks.
pub fn file_identity(file: &File) -> io::Result<(u32, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        return Err(last_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// Create the private directory `dir` (parents as usual) or, when it exists, validate it
/// without repair first and only then normalize it to the exact protected descriptor.
/// The order is the point: a broad or foreign directory is refused with its DACL unchanged.
pub fn ensure_dir_validated(dir: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => check_dir_no_repair(dir, Exposure::Private)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    super::ensure_dir(dir)
}
