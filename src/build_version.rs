//! The check `build.rs` makes of `SQLH_VERSION` before it prints anything (R47, decision 44).
//! Shared with the library's tests, which compile this file as a module of their own, so the
//! rule is tested where the build can't be.

/// `given` as the version sqlh reports, or why not: `Cargo.toml`'s version `cargo`, alone or
/// with a semver pre-release part (`0.1.0-dev.99`, `0.1.0-rc.1`). The pre-release is dot-
/// separated identifiers, each non-empty, of ASCII letters, digits and `-` only, and a numeric
/// one has no leading zero (semver 2.0.0, item 9). Build metadata (`+...`) isn't taken: no
/// build of sqlh makes it, and semver ignores it in ordering. Anything else (a space, a control
/// character or line break, which would end Cargo's directive and split the version the code
/// reports from the resource's) is refused before it reaches either.
pub(crate) fn checked(given: &str, cargo: &str) -> Result<(), String> {
    let pre_release = match given.strip_prefix(cargo) {
        Some("") => return Ok(()),
        Some(rest) => rest.strip_prefix('-'),
        None => None,
    };
    let fits = |id: &str| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !(id.len() > 1 && id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit()))
    };
    match pre_release {
        Some(pre) if pre.split('.').all(fits) => Ok(()),
        _ => Err(format!(
            "SQLH_VERSION={given:?} isn't Cargo.toml's version {cargo} or {cargo}-<pre-release> \
             (dot-separated identifiers of ASCII letters, digits and -, no empty one, no \
             leading zero in a number)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::checked;

    #[test]
    fn the_release_workflows_versions_pass_and_malformed_ones_are_refused() {
        for good in [
            "0.1.0",
            "0.1.0-dev.99",
            "0.1.0-rc.1",
            "0.1.0-local",
            "0.1.0-0",
            "0.1.0-x-y.7",
        ] {
            assert_eq!(checked(good, "0.1.0"), Ok(()), "{good}");
        }
        for bad in [
            "0.2.0",
            "0.1.00",
            "0.1.0.1",
            "0.1.0-",
            "0.1.0-dev.",
            "0.1.0-.dev",
            "0.1.0-dev..1",
            "0.1.0-01",
            "0.1.0-dev.007",
            "0.1.0-not a version",
            "0.1.0-dev\ncargo:warning=x",
            "0.1.0-dev\r",
            "0.1.0-dev\t",
            "0.1.0-dév",
            "0.1.0+build.5",
            "0.1.0-dev+5",
            " 0.1.0",
            "",
        ] {
            assert!(checked(bad, "0.1.0").is_err(), "{bad:?}");
        }
    }
}
