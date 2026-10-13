//! A password as sqlh holds it (F14, DESIGN.md §8): a `String` whose bytes are wiped when it
//! is dropped, including the spare capacity, so a password doesn't linger in freed memory.
//! Every copy sqlh makes is one of these. What it can't wipe is listed in DESIGN.md §8: the
//! driver's own copy, the console's input buffer, the process environment's SQLCMDPASSWORD.

pub type Secret = zeroize::Zeroizing<String>;
