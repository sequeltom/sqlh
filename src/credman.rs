//! Windows Credential Manager. Profile passwords live here as generic credentials named
//! `sqlh:<profile>`, protected by DPAPI for the current Windows user. Before the project was
//! named sqlh they were `sqlharness:<profile>`; `move_legacy` moves one across (`config`'s
//! one-time migration).

const PREFIX: &str = "sqlh:";
const LEGACY_PREFIX: &str = "sqlharness:";

pub fn target(profile: &str) -> String {
    format!("{PREFIX}{profile}")
}

fn legacy_target(profile: &str) -> String {
    format!("{LEGACY_PREFIX}{profile}")
}

pub fn write(profile: &str, user: &str, password: &str) -> std::io::Result<()> {
    imp::write(&target(profile), user, password)
}

/// The stored password, or `None` when the profile has no credential.
pub fn read(profile: &str) -> std::io::Result<Option<String>> {
    imp::read(&target(profile))
}

/// Whether the profile has a stored password, without copying the password anywhere.
pub fn exists(profile: &str) -> std::io::Result<bool> {
    imp::exists(&target(profile))
}

/// Removes the profile's credential. Returns false when there was none.
pub fn delete(profile: &str) -> std::io::Result<bool> {
    imp::delete(&target(profile))
}

/// Moves a password stored under the old name (`sqlharness:<profile>`) to `sqlh:<profile>`,
/// for `user`. A credential already under the new name wins and is left alone. The old one is
/// deleted only after the new one is written, so an interrupted move can be run again.
/// Returns whether there was an old credential.
pub fn move_legacy(profile: &str, user: &str) -> std::io::Result<bool> {
    let legacy = legacy_target(profile);
    let Some(mut password) = imp::read(&legacy)? else {
        return Ok(false);
    };
    let written = if imp::exists(&target(profile))? {
        Ok(())
    } else {
        imp::write(&target(profile), user, &password)
    };
    imp::wipe_string(&mut password);
    written?;
    imp::delete(&legacy)?;
    Ok(true)
}

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

    pub fn write(target: &str, user: &str, password: &str) -> io::Result<()> {
        let mut target = wide(target);
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

    pub fn read(target: &str) -> io::Result<Option<String>> {
        let target = wide(target);
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

    pub fn exists(target: &str) -> io::Result<bool> {
        let target = wide(target);
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

    pub fn delete(target: &str) -> io::Result<bool> {
        let target = wide(target);
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

    /// Zeroes a password copy before it is dropped.
    pub fn wipe_string(text: &mut String) {
        let mut bytes = std::mem::take(text).into_bytes();
        wipe(&mut bytes);
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    pub fn write(_target: &str, _user: &str, _password: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Credential Manager needs Windows",
        ))
    }

    pub fn read(_target: &str) -> io::Result<Option<String>> {
        Ok(None)
    }

    pub fn exists(_target: &str) -> io::Result<bool> {
        Ok(false)
    }

    pub fn delete(_target: &str) -> io::Result<bool> {
        Ok(false)
    }

    pub fn wipe_string(text: &mut String) {
        text.clear();
    }
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

    /// The rename's one-time move, against the real Credential Manager (cleans up):
    /// cargo test -- --ignored credman
    #[test]
    #[ignore]
    fn credman_moves_a_legacy_credential() {
        let name = "__sqlh_selftest_legacy";
        let legacy = super::legacy_target(name);
        let _ = super::delete(name);
        super::imp::write(&legacy, "someone", "old-pässwörd").unwrap();
        assert!(super::move_legacy(name, "someone").unwrap());
        assert_eq!(super::read(name).unwrap().as_deref(), Some("old-pässwörd"));
        assert!(
            !super::imp::exists(&legacy).unwrap(),
            "the old name is gone"
        );
        // Nothing left to move; and a credential already under the new name wins.
        assert!(!super::move_legacy(name, "someone").unwrap());
        super::imp::write(&legacy, "someone", "older").unwrap();
        assert!(super::move_legacy(name, "someone").unwrap());
        assert_eq!(super::read(name).unwrap().as_deref(), Some("old-pässwörd"));
        assert!(!super::imp::exists(&legacy).unwrap());
        assert!(super::delete(name).unwrap());
    }
}
