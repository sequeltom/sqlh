//! A name's canonical DNS name, for Windows auth's SPN (owner decision 79, `db::spn`). Through
//! `GetAddrInfoW`, the wide API, so the answer arrives as UTF-16 and is decoded fallibly: an
//! answer that doesn't decode is no answer, never a panic (R67). dns-lookup's `getaddrinfo`
//! calls the ANSI API and unwraps the canonical name's conversion to UTF-8, so a name in another
//! code page ended the whole process through the panic hook, `sqlh mcp` included.

use std::ptr;
use std::sync::OnceLock;

use windows_sys::Win32::Networking::WinSock::{
    ADDRINFOW, AI_CANONNAME, FreeAddrInfoW, GetAddrInfoW, WSADATA, WSAStartup,
};

/// A DNS name is at most 255 octets; the bound only stops a read past a missing terminator.
const MOST_UNITS: usize = 1024;

/// The canonical name a forward lookup of `host` gives, or `None` when the lookup fails, gives
/// no name, or gives one that isn't valid UTF-16. Blocking: `db::spn` runs it on a thread of its
/// own, within its budget.
pub(crate) fn canonical_name(host: &str) -> Option<String> {
    if !winsock() || host.contains('\0') {
        return None;
    }
    let host: Vec<u16> = host.encode_utf16().chain(Some(0)).collect();
    let hints = ADDRINFOW {
        ai_flags: AI_CANONNAME as i32,
        ai_family: 0,
        ai_socktype: 0,
        ai_protocol: 0,
        ai_addrlen: 0,
        ai_canonname: ptr::null_mut(),
        ai_addr: ptr::null_mut(),
        ai_next: ptr::null_mut(),
    };
    let mut found: *mut ADDRINFOW = ptr::null_mut();
    // SAFETY: `host` is NUL-terminated UTF-16 that outlives the call, the service is null (none
    // asked for), `hints` is a valid ADDRINFOW with only the flags set, and `found` is a valid
    // place for the result's pointer.
    let status = unsafe { GetAddrInfoW(host.as_ptr(), ptr::null(), &hints, &mut found) };
    if status != 0 || found.is_null() {
        return None;
    }
    let found = List(found);
    // SAFETY: `found.0` is the head of the list GetAddrInfoW just returned, freed only when
    // `found` drops, after this read; its `ai_canonname` is null or a NUL-terminated wide string
    // owned by the list. The canonical name comes with the first entry only (AI_CANONNAME).
    unsafe { decode((*found.0).ai_canonname) }
}

/// The result list, freed once, however the read of it ends.
struct List(*mut ADDRINFOW);

impl Drop for List {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful GetAddrInfoW and is freed only here.
        unsafe { FreeAddrInfoW(self.0) };
    }
}

/// A NUL-terminated wide string as a `String`: `None` for a null pointer, an empty string, no
/// terminator within `MOST_UNITS`, or UTF-16 that doesn't decode (an unpaired surrogate).
///
/// # Safety
/// `name` is null, or points to readable `u16`s up to a NUL or `MOST_UNITS` of them.
unsafe fn decode(name: *const u16) -> Option<String> {
    if name.is_null() {
        return None;
    }
    let mut units = 0;
    // SAFETY: the caller's promise: each unit up to the NUL, within the bound, is readable.
    while units < MOST_UNITS && unsafe { *name.add(units) } != 0 {
        units += 1;
    }
    if units == 0 || units == MOST_UNITS {
        return None;
    }
    // SAFETY: the `units` units just read are readable and stay so for this borrow.
    let name = unsafe { std::slice::from_raw_parts(name, units) };
    String::from_utf16(name).ok()
}

/// WinSock started in this process (`GetAddrInfoW` refuses before that). The TCP connect has
/// started it before any SPN is built; this makes the function stand on its own (its tests). It
/// is never cleaned up: it lasts the process, as std's does.
fn winsock() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        // SAFETY: WSADATA is plain data that WSAStartup fills; all zeros is a valid start.
        let mut data: WSADATA = unsafe { std::mem::zeroed() };
        // SAFETY: version 2.2 and a valid place for its data.
        unsafe { WSAStartup(0x0202, &mut data) == 0 }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    // R67: the canonical name is decoded fallibly. ASCII and non-ASCII names decode; a null
    // pointer, an empty name, an unpaired surrogate (the wide API's undecodable answer) and a
    // name with no terminator within the bound are no answer, never a panic.
    #[test]
    fn a_canonical_name_decodes_or_is_no_answer() {
        let ascii = wide("db01.corp.example");
        let accented = wide("café.corp.example");
        let empty = wide("");
        let lone_surrogate = [0x0064, 0xD800, 0x002E, 0x0078, 0];
        let unterminated = vec![0x0061u16; MOST_UNITS + 8];
        // SAFETY: each buffer is NUL-terminated, or (the last) readable past the bound.
        unsafe {
            assert_eq!(decode(ascii.as_ptr()).as_deref(), Some("db01.corp.example"));
            assert_eq!(
                decode(accented.as_ptr()).as_deref(),
                Some("café.corp.example")
            );
            assert_eq!(decode(ptr::null()), None);
            assert_eq!(decode(empty.as_ptr()), None);
            assert_eq!(decode(lone_surrogate.as_ptr()), None);
            assert_eq!(decode(unterminated.as_ptr()), None);
        }
    }

    // The real lookup: a name with a NUL is refused before the call, and one that can't resolve
    // is no answer. `.invalid` is reserved (RFC 2606): no DNS answers it, and offline the lookup
    // fails too, so the result doesn't depend on this machine's network.
    #[test]
    fn a_lookup_that_cannot_answer_is_no_answer() {
        assert_eq!(canonical_name("db01\0.corp.example"), None);
        assert_eq!(canonical_name("sqlh-no-such-host.invalid"), None);
    }
}
