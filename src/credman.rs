//! Windows Credential Manager. Profile passwords live here as generic credentials named
//! `sqlh:<profile>`, protected by DPAPI for the current Windows user. Before the project was
//! named sqlh they were `sqlharness:<profile>`; `move_legacy` moves one across (`config`'s
//! one-time migration).
//!
//! Each credential is bound to what it was saved for (R20, DESIGN.md §8): its user name is
//! the SQL login, and its comment carries the profile's connection identity
//! (`config::identity`), so a run can tell a password that belongs to the profile it read
//! from one an edit stored for another server.

use crate::secret::Secret;

const PREFIX: &str = "sqlh:";
const LEGACY_PREFIX: &str = "sqlharness:";
/// The comment of every credential sqlh writes; `; identity <hex>` follows it when the
/// credential is bound to an identity.
const COMMENT: &str = "sqlh connection profile password";
const IDENTITY: &str = "; identity ";

pub fn target(profile: &str) -> String {
    format!("{PREFIX}{profile}")
}

fn legacy_target(profile: &str) -> String {
    format!("{LEGACY_PREFIX}{profile}")
}

/// What a stored password was saved for: the SQL login and, for credentials written since
/// R20, the profile's connection identity. `None` is a credential written before, or by
/// another tool (`cmdkey`); only its login can be checked.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub user: String,
    pub identity: Option<String>,
}

/// A stored password with its binding.
pub struct Stored {
    pub password: Secret,
    pub binding: Binding,
}

fn comment(identity: Option<&str>) -> String {
    match identity {
        Some(identity) => format!("{COMMENT}{IDENTITY}{identity}"),
        None => COMMENT.to_owned(),
    }
}

fn identity_in(comment: &str) -> Option<String> {
    comment
        .strip_prefix(COMMENT)?
        .strip_prefix(IDENTITY)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

pub fn write(profile: &str, binding: &Binding, password: &str) -> std::io::Result<()> {
    imp::write(
        &target(profile),
        &binding.user,
        &comment(binding.identity.as_deref()),
        password.as_bytes(),
    )
}

/// The stored password and its binding, or `None` when the profile has no credential.
pub fn read(profile: &str) -> std::io::Result<Option<Stored>> {
    imp::read(&target(profile), true).map(|found| {
        found.map(|(password, user, comment)| Stored {
            password: password.unwrap_or_default(),
            binding: Binding {
                user,
                identity: identity_in(&comment),
            },
        })
    })
}

/// The binding of the profile's stored password, without copying the password anywhere;
/// `None` when the profile has no credential.
pub fn binding(profile: &str) -> std::io::Result<Option<Binding>> {
    imp::read(&target(profile), false).map(|found| {
        found.map(|(_, user, comment)| Binding {
            user,
            identity: identity_in(&comment),
        })
    })
}

/// Whether the profile has a stored password, without copying the password anywhere.
pub fn exists(profile: &str) -> std::io::Result<bool> {
    binding(profile).map(|b| b.is_some())
}

/// Removes the profile's credential. Returns false when there was none.
pub fn delete(profile: &str) -> std::io::Result<bool> {
    imp::delete(&target(profile))
}

/// Moves a password stored under the old name (`sqlharness:<profile>`) to `sqlh:<profile>`,
/// bound to `identity`, the identity of the profile in the file being moved with it. It keeps
/// the old credential's login, so a password the old file's profile no longer matches stays
/// unusable rather than becoming usable (R20). The old file is the one that describes this
/// profile, so its password replaces any under the new name. The old credential is deleted
/// only after the new one is written, so an interrupted move can be run again. Returns
/// whether there was an old credential.
pub fn move_legacy(profile: &str, identity: &str) -> std::io::Result<bool> {
    let legacy = legacy_target(profile);
    let Some((Some(password), user, _)) = imp::read(&legacy, true)? else {
        return Ok(false);
    };
    imp::write(
        &target(profile),
        &user,
        &comment(Some(identity)),
        password.as_bytes(),
    )?;
    imp::delete(&legacy)?;
    Ok(true)
}

/// Held by every test that touches the real Credential Manager. A read right after a write
/// or delete can miss it while another client is changing credentials, and a write can be
/// lost (measured across processes, scratch probe, 2026-11-08: PLAN.md §4), which made
/// `credman_round_trip` fail about one run in eight when these tests ran in parallel.
#[cfg(test)]
pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A password read back from Credential Manager, taken out of `bytes` without a copy. One that
/// isn't UTF-8 is refused, and `from_utf8`'s error hands its bytes back to `bytes`, which
/// wipes them when dropped (F14).
#[cfg_attr(not(windows), allow(dead_code))]
fn password_from(bytes: &mut zeroize::Zeroizing<Vec<u8>>) -> std::io::Result<Secret> {
    match String::from_utf8(std::mem::take(&mut **bytes)) {
        Ok(password) => Ok(Secret::new(password)),
        Err(error) => {
            **bytes = error.into_bytes();
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the stored password is not UTF-8",
            ))
        }
    }
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

    pub fn write(target: &str, user: &str, comment: &str, password: &[u8]) -> io::Result<()> {
        let mut target = wide(target);
        let mut user = wide(user);
        let mut comment = wide(comment);
        let mut blob = password.to_vec();
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

    /// The credential's password (only when `password` is true; otherwise the blob isn't
    /// copied), user name and comment, or `None` when there is no such credential.
    #[allow(clippy::type_complexity)]
    pub fn read(
        target: &str,
        password: bool,
    ) -> io::Result<Option<(Option<super::Secret>, String, String)>> {
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
        // SAFETY: CredReadW succeeded, so `credential` is valid: its blob holds
        // CredentialBlobSize bytes (the pointer may be null when the size is 0), and its user
        // name and comment are NUL-terminated or null. The block is ours until CredFree, so
        // the blob may be written. Everything is copied before CredFree.
        let (bytes, user, comment) = unsafe {
            let c = &*credential;
            let mut bytes = zeroize::Zeroizing::new(Vec::new());
            if c.CredentialBlobSize != 0 && !c.CredentialBlob.is_null() {
                let blob =
                    std::slice::from_raw_parts_mut(c.CredentialBlob, c.CredentialBlobSize as usize);
                if password {
                    // One allocation of the blob's size, so no reallocation leaves a copy.
                    *bytes = blob.to_vec();
                }
                // CredFree releases the block without zeroing it (F14).
                wipe(blob);
            }
            let user = from_wide(c.UserName);
            let comment = from_wide(c.Comment);
            CredFree(credential as *const _);
            (bytes, user, comment)
        };
        if !password {
            return Ok(Some((None, user, comment)));
        }
        let mut bytes = bytes;
        super::password_from(&mut bytes).map(|s| Some((Some(s), user, comment)))
    }

    /// A NUL-terminated UTF-16 string from the system, or "" for null.
    ///
    /// # Safety
    /// `p` is null or points to a NUL-terminated UTF-16 string that stays valid for the call.
    unsafe fn from_wide(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0;
        // SAFETY: the caller guarantees a NUL terminator, so every read up to it is in bounds.
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` u16s from `p` were just read, so the slice is valid.
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
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
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    pub fn write(_target: &str, _user: &str, _comment: &str, _password: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Credential Manager needs Windows",
        ))
    }

    #[allow(clippy::type_complexity)]
    pub fn read(
        _target: &str,
        _password: bool,
    ) -> io::Result<Option<(Option<super::Secret>, String, String)>> {
        Ok(None)
    }

    pub fn delete(_target: &str) -> io::Result<bool> {
        Ok(false)
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
        let bound = super::Binding {
            user: "someone".into(),
            identity: Some("0123456789abcdef".into()),
        };
        let _serial = super::serial();
        super::write(name, &bound, "pässwörd").unwrap();
        assert!(super::exists(name).unwrap());
        assert_eq!(super::binding(name).unwrap().as_ref(), Some(&bound));
        let stored = super::read(name).unwrap().unwrap();
        assert_eq!(stored.password.as_str(), "pässwörd");
        assert_eq!(stored.binding, bound);
        // A credential without an identity, as sqlh wrote them before R20.
        let unbound = super::Binding {
            user: "someone".into(),
            identity: None,
        };
        super::write(name, &unbound, "pässwörd").unwrap();
        assert_eq!(super::binding(name).unwrap(), Some(unbound));
        assert!(super::delete(name).unwrap());
        assert!(!super::exists(name).unwrap());
        assert!(super::read(name).unwrap().is_none());
        assert!(!super::delete(name).unwrap());
    }

    /// The rename's one-time move, against the real Credential Manager (cleans up):
    /// cargo test -- --ignored credman
    #[test]
    #[ignore]
    fn credman_moves_a_legacy_credential() {
        let name = "__sqlh_selftest_legacy";
        let legacy = super::legacy_target(name);
        let _serial = super::serial();
        let _ = super::delete(name);
        super::imp::write(
            &legacy,
            "someone",
            super::COMMENT,
            "old-pässwörd".as_bytes(),
        )
        .unwrap();
        assert!(super::move_legacy(name, "feedface").unwrap());
        let stored = super::read(name).unwrap().unwrap();
        assert_eq!(stored.password.as_str(), "old-pässwörd");
        assert_eq!(
            stored.binding,
            super::Binding {
                user: "someone".into(),
                identity: Some("feedface".into())
            }
        );
        assert!(
            super::imp::read(&legacy, false).unwrap().is_none(),
            "the old name is gone"
        );
        // Nothing left to move; and the old file's password replaces one under the new name.
        assert!(!super::move_legacy(name, "feedface").unwrap());
        super::imp::write(&legacy, "someone", super::COMMENT, b"older").unwrap();
        assert!(super::move_legacy(name, "feedface").unwrap());
        assert_eq!(
            super::read(name).unwrap().unwrap().password.as_str(),
            "older"
        );
        assert!(super::imp::read(&legacy, false).unwrap().is_none());
        assert!(super::delete(name).unwrap());
    }

    /// A stored password that isn't UTF-8, as another tool (`cmdkey`) could leave it, against
    /// the real Credential Manager (cleans up): cargo test -- --ignored credman
    #[test]
    #[ignore]
    fn credman_refuses_a_password_that_is_not_utf8() {
        let name = "__sqlh_selftest_not_utf8";
        let _serial = super::serial();
        super::imp::write(&super::target(name), "someone", super::COMMENT, b"p\xffss").unwrap();
        let refused = super::read(name).err().expect("refused");
        assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(refused.to_string(), "the stored password is not UTF-8");
        assert!(super::exists(name).unwrap(), "the binding is still read");
        assert!(super::delete(name).unwrap());
    }

    // F14: a refused password's bytes go back to the wiping buffer, and an accepted one is
    // taken out of it, not copied, so no copy outlives a wipe.
    #[test]
    fn a_password_is_taken_from_the_wiping_buffer_or_left_in_it() {
        let mut bytes = zeroize::Zeroizing::new(b"p\xffss".to_vec());
        // Not `expect_err`, which would print the password it got.
        let Err(refused) = super::password_from(&mut bytes) else {
            panic!("accepted");
        };
        assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(bytes.as_slice(), b"p\xffss");

        let mut bytes = zeroize::Zeroizing::new("pässwörd".as_bytes().to_vec());
        let at = bytes.as_ptr();
        let password = super::password_from(&mut bytes).unwrap();
        assert_eq!(password.as_str(), "pässwörd");
        assert_eq!(password.as_ptr(), at, "the same allocation");
        assert_eq!((bytes.len(), bytes.capacity()), (0, 0));
    }

    #[test]
    fn the_identity_is_read_back_from_the_comment() {
        use super::{comment, identity_in};
        assert_eq!(identity_in(&comment(Some("abc"))).as_deref(), Some("abc"));
        assert_eq!(identity_in(&comment(None)), None);
        assert_eq!(identity_in(""), None);
        assert_eq!(identity_in("someone else's; identity abc"), None);
        assert_eq!(identity_in(&format!("{}; identity ", super::COMMENT)), None);
    }
}
