//! Windows Credential Manager. Profile passwords live here as generic credentials named
//! `sqlharness:<profile>`, protected by DPAPI for the current Windows user.

const PREFIX: &str = "sqlharness:";

pub fn target(profile: &str) -> String {
    format!("{PREFIX}{profile}")
}

#[cfg(windows)]
pub use imp::{delete, exists, read, write};

#[cfg(windows)]
mod imp {
    use std::io;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, FILETIME};
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn write(profile: &str, user: &str, password: &str) -> io::Result<()> {
        let mut target = wide(&super::target(profile));
        let mut user = wide(user);
        let mut comment = wide("sqlh connection profile password");
        let mut blob = password.as_bytes().to_vec();
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: comment.as_mut_ptr(),
            LastWritten: FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: null_mut(),
            TargetAlias: null_mut(),
            UserName: user.as_mut_ptr(),
        };
        // SAFETY: every pointer in `credential` points into a live, NUL-terminated (or
        // length-counted) buffer that outlives the call; CredWriteW copies what it keeps.
        let ok = unsafe { CredWriteW(&credential, 0) };
        wipe(&mut blob);
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// The stored password, or `None` when the profile has no credential.
    pub fn read(profile: &str) -> io::Result<Option<String>> {
        let target = wide(&super::target(profile));
        let mut credential: *mut CREDENTIALW = null_mut();
        // SAFETY: `target` is NUL-terminated; on success `credential` points to a buffer
        // owned by the system that we release with CredFree below.
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if ok == 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(code) if code == ERROR_NOT_FOUND as i32 => Ok(None),
                _ => Err(error),
            };
        }
        // SAFETY: CredReadW succeeded, so `credential` is valid and its blob holds
        // CredentialBlobSize bytes (the blob pointer may be null when the size is 0).
        let bytes = unsafe {
            let c = &*credential;
            let bytes = if c.CredentialBlobSize == 0 || c.CredentialBlob.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
            };
            CredFree(credential as *const _);
            bytes
        };
        String::from_utf8(bytes).map(Some).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the stored password is not UTF-8",
            )
        })
    }

    /// Whether the profile has a stored password, without copying the password anywhere.
    pub fn exists(profile: &str) -> io::Result<bool> {
        let target = wide(&super::target(profile));
        let mut credential: *mut CREDENTIALW = null_mut();
        // SAFETY: `target` is NUL-terminated; on success `credential` points to a buffer
        // owned by the system, released at once below without reading the blob.
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if ok == 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(code) if code == ERROR_NOT_FOUND as i32 => Ok(false),
                _ => Err(error),
            };
        }
        // SAFETY: CredReadW succeeded, so `credential` is a valid buffer that must be freed.
        unsafe { CredFree(credential as *const _) };
        Ok(true)
    }

    /// Removes the profile's credential. Returns false when there was none.
    pub fn delete(profile: &str) -> io::Result<bool> {
        let target = wide(&super::target(profile));
        // SAFETY: `target` is NUL-terminated and outlives the call.
        let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == ERROR_NOT_FOUND as i32 => Ok(false),
            _ => Err(error),
        }
    }

    fn wipe(bytes: &mut [u8]) {
        for b in bytes.iter_mut() {
            // SAFETY: `b` is a valid, aligned &mut u8; volatile keeps the wipe from being elided.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

#[cfg(not(windows))]
use std::io;

#[cfg(not(windows))]
pub fn write(_profile: &str, _user: &str, _password: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Credential Manager needs Windows",
    ))
}

#[cfg(not(windows))]
pub fn read(_profile: &str) -> io::Result<Option<String>> {
    Ok(None)
}

#[cfg(not(windows))]
pub fn exists(_profile: &str) -> io::Result<bool> {
    Ok(false)
}

#[cfg(not(windows))]
pub fn delete(_profile: &str) -> io::Result<bool> {
    Ok(false)
}

#[cfg(all(test, windows))]
mod tests {
    /// Touches the real Credential Manager (and cleans up), so it only runs on request:
    /// cargo test -- --ignored credman
    #[test]
    #[ignore]
    fn credman_round_trip() {
        let name = "__sqlh_selftest";
        super::write(name, "someone", "pässwörd").unwrap();
        assert!(super::exists(name).unwrap());
        assert_eq!(super::read(name).unwrap().as_deref(), Some("pässwörd"));
        assert!(super::delete(name).unwrap());
        assert!(!super::exists(name).unwrap());
        assert_eq!(super::read(name).unwrap(), None);
        assert!(!super::delete(name).unwrap());
    }
}
