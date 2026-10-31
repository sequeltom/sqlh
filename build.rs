//! The version sqlh reports (decision 44, DESIGN.md §4), and the Windows version resource that
//! Explorer's Properties > Details shows: file and product version, name, description and
//! copyright.
//!
//! The version is `SQLH_VERSION` when the release workflow sets it (`0.1.0`, or
//! `0.1.0-dev.<run>`), otherwise `Cargo.toml`'s with `-local`, so a build made anywhere else
//! never passes for a release. It reaches the code as `SQLH_BUILT_VERSION`.
//!
//! The resource is written here as a compiled .res file, which the MSVC linker takes as an
//! input like an object file, so neither the resource compiler (rc.exe) nor a crate is needed.
//! Formats: RESOURCEHEADER and VS_VERSIONINFO, with VS_FIXEDFILEINFO, StringFileInfo and
//! VarFileInfo (Win32 documentation, "Resource file formats" and "Version information").

use std::env;
use std::path::PathBuf;

/// The text fields, in the US English, Unicode string table (`040904B0`). The version fields
/// are added from the build.
const STRINGS: [(&str, &str); 6] = [
    ("CompanyName", "Tom Willwerth"),
    ("FileDescription", "sqlh - SQL Server query tool for agents"),
    ("InternalName", "sqlh"),
    ("LegalCopyright", "Copyright (c) 2026 Tom Willwerth"),
    ("OriginalFilename", "sqlh.exe"),
    ("ProductName", "sqlh"),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SQLH_VERSION");
    println!("cargo:rerun-if-env-changed=SQLH_BUILD");

    let cargo = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let version = match env::var("SQLH_VERSION") {
        // The release workflow's: Cargo.toml's version, or it with a pre-release part. Any
        // other value would give the text and the numeric versions different numbers.
        Ok(given) if given == cargo || given.starts_with(&format!("{cargo}-")) => given,
        Ok(given) => panic!(
            "SQLH_VERSION={given} isn't Cargo.toml's version {cargo} or {cargo}-<pre-release>"
        ),
        Err(env::VarError::NotPresent) => format!("{cargo}-local"),
        Err(env::VarError::NotUnicode(_)) => panic!("SQLH_VERSION isn't valid Unicode"),
    };
    println!("cargo:rustc-env=SQLH_BUILT_VERSION={version}");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    // Windows keeps four 16-bit numbers: major.minor.patch, then the release workflow's run
    // number (SQLH_BUILD), which only grows, so a release built after its pre-releases is
    // newer than them too; 0 for any other build.
    let part = |name: &str, value: String| -> u16 {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value} isn't a number from 0 to 65535"))
    };
    let numbers = [
        part(
            "major",
            env::var("CARGO_PKG_VERSION_MAJOR").unwrap_or_default(),
        ),
        part(
            "minor",
            env::var("CARGO_PKG_VERSION_MINOR").unwrap_or_default(),
        ),
        part(
            "patch",
            env::var("CARGO_PKG_VERSION_PATCH").unwrap_or_default(),
        ),
        match env::var("SQLH_BUILD") {
            Ok(build) => part("SQLH_BUILD", build),
            Err(_) => 0,
        },
    ];
    let res = resource_file(&version_info(&version, numbers));
    let path = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("sqlh.res");
    std::fs::write(&path, res).expect("the version resource is written");
    println!("cargo:rustc-link-arg-bins={}", path.display());
}

/// VS_VERSIONINFO: the fixed numbers, then the strings and the language they are in.
fn version_info(version: &str, numbers: [u16; 4]) -> Vec<u8> {
    let [major, minor, patch, build] = numbers.map(u32::from);
    let ms = (major << 16) | minor;
    let ls = (patch << 16) | build;
    // VS_FF_PRERELEASE for a version with a pre-release part (-dev.N, -local).
    let flags: u32 = if version.contains('-') { 0x2 } else { 0 };
    let fixed: Vec<u8> = [
        0xFEEF_04BD, // signature
        0x0001_0000, // structure version 1.0
        ms,          // file version
        ls,
        ms, // product version
        ls,
        0x3F,        // the flags that are valid
        flags,       //
        0x0004_0004, // VOS_NT_WINDOWS32
        0x1,         // VFT_APP
        0,           // subtype
        0,           // date
        0,
    ]
    .iter()
    .flat_map(|n: &u32| n.to_le_bytes())
    .collect();

    let mut strings: Vec<Vec<u8>> = STRINGS.iter().map(|(k, v)| string(k, v)).collect();
    strings.push(string("FileVersion", version));
    strings.push(string("ProductVersion", version));
    let table = block("040904B0", 1, 0, &[], &strings);
    let string_file_info = block("StringFileInfo", 1, 0, &[], &[table]);
    // US English (0x0409) in Unicode (1200, 0x04B0).
    let translation = block("Translation", 0, 4, &[0x09, 0x04, 0xB0, 0x04], &[]);
    let var_file_info = block("VarFileInfo", 1, 0, &[], &[translation]);
    block(
        "VS_VERSION_INFO",
        0,
        52,
        &fixed,
        &[string_file_info, var_file_info],
    )
}

/// A String: its value in UTF-16 with its terminator, its length counted in characters.
fn string(key: &str, value: &str) -> Vec<u8> {
    let text = utf16z(value);
    let characters = u16::try_from(text.len() / 2).expect("a short value");
    block(key, 1, characters, &text, &[])
}

/// One block of the version tree: `wLength`, `wValueLength`, `wType`, the key in UTF-16 with
/// its terminator, then the value and each child, each starting on a 4-byte boundary.
/// `wLength` covers the block up to the end of its last child.
fn block(key: &str, kind: u16, value_length: u16, value: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0, 0];
    out.extend(value_length.to_le_bytes());
    out.extend(kind.to_le_bytes());
    out.extend(utf16z(key));
    align(&mut out);
    out.extend(value);
    for child in children {
        align(&mut out);
        out.extend(child);
    }
    let length = u16::try_from(out.len()).expect("a version block under 64 KB");
    out[..2].copy_from_slice(&length.to_le_bytes());
    out
}

/// A .res file: the empty entry every 32-bit .res file starts with, then the version resource
/// (type RT_VERSION 16, name 1, US English), its data padded to 4 bytes.
fn resource_file(data: &[u8]) -> Vec<u8> {
    let header = |size: u32, kind: u16, name: u16, flags: u16, language: u16| {
        let mut h = Vec::with_capacity(32);
        h.extend(size.to_le_bytes()); // DataSize
        h.extend(32u32.to_le_bytes()); // HeaderSize
        h.extend(
            [0xFFFF, kind, 0xFFFF, name]
                .iter()
                .flat_map(|n: &u16| n.to_le_bytes()),
        );
        h.extend(0u32.to_le_bytes()); // DataVersion
        h.extend(flags.to_le_bytes()); // MemoryFlags
        h.extend(language.to_le_bytes()); // LanguageId
        h.extend(0u32.to_le_bytes()); // Version
        h.extend(0u32.to_le_bytes()); // Characteristics
        h
    };
    let size = u32::try_from(data.len()).expect("a small resource");
    let mut out = header(0, 0, 0, 0, 0);
    // MOVEABLE | PURE, as rc.exe marks a version resource.
    out.extend(header(size, 16, 1, 0x0030, 0x0409));
    out.extend(data);
    align(&mut out);
    out
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn align(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}
