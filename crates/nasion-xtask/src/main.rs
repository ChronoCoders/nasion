//! Checks every target root in the workspace for a crate-level
//! `#![deny(unsafe_code)]`. The file is parsed with syn, so the attribute text
//! inside a comment or a string literal never counts.
//!
//! Exit codes, read by `scripts/checks.sh`: 0 clean, 1 a target lacks the
//! attribute, 2 the check could not run.
#![deny(unsafe_code)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const CLEAN: u8 = 0;
const VIOLATION: u8 = 1;
const CANNOT_RUN: u8 = 2;

struct Target {
    package: String,
    src_path: PathBuf,
}

fn main() -> ExitCode {
    let (code, message) = match check_workspace() {
        Ok(missing) if missing.is_empty() => (CLEAN, None),
        Ok(missing) => (VIOLATION, Some(missing.join("\n"))),
        Err(message) => (CANNOT_RUN, Some(message)),
    };
    let Some(message) = message else {
        return ExitCode::from(code);
    };
    match writeln!(io::stdout().lock(), "{message}") {
        Ok(()) => ExitCode::from(code),
        Err(_) => ExitCode::from(CANNOT_RUN),
    }
}

/// Returns one line per target root that lacks the attribute.
fn check_workspace() -> Result<Vec<String>, String> {
    let targets = workspace_targets()?;
    if targets.is_empty() {
        return Err(
            "cargo metadata listed no targets. Check the workspace members in Cargo.toml.".into(),
        );
    }
    let mut missing = Vec::new();
    for target in &targets {
        if !denies_unsafe_code(&target.src_path)? {
            missing.push(format!(
                "{} (target of {}) lacks a crate-level #![deny(unsafe_code)]",
                target.src_path.display(),
                target.package
            ));
        }
    }
    if missing.is_empty() {
        writeln!(io::stdout().lock(), "note: {} targets", targets.len())
            .map_err(|e| format!("writing the target count to stdout failed: {e}"))?;
    }
    Ok(missing)
}

/// Lists every target of every workspace member. Uses the cargo that ran this
/// tool when there is one, so the pinned toolchain answers.
fn workspace_targets() -> Result<Vec<Target>, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(&cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|e| format!("starting {} failed: {e}", cargo.to_string_lossy()))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed ({}). Check that every Cargo.toml in the workspace parses.\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("cargo metadata output is not JSON: {e}"))?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata output has no packages list")?;
    let mut targets = Vec::new();
    for package in packages {
        let name = package["name"]
            .as_str()
            .ok_or("a package in cargo metadata has no name")?;
        let package_targets = package["targets"]
            .as_array()
            .ok_or_else(|| format!("package {name} in cargo metadata has no targets list"))?;
        for target in package_targets {
            let src_path = target["src_path"]
                .as_str()
                .ok_or_else(|| format!("a target of {name} in cargo metadata has no src_path"))?;
            targets.push(Target {
                package: name.to_owned(),
                src_path: PathBuf::from(src_path),
            });
        }
    }
    Ok(targets)
}

fn denies_unsafe_code(path: &Path) -> Result<bool, String> {
    let source =
        fs::read_to_string(path).map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
    crate_denies_unsafe_code(&source).map_err(|e| format!("{} does not parse: {e}", path.display()))
}

/// True when the crate-level inner attributes hold `deny(unsafe_code)` or
/// `forbid(unsafe_code)`, alone or with other lints. forbid counts because it
/// is stricter: no `allow` further down can lift it. An attribute inside
/// `cfg_attr`, on an item or on a module does not count, because it does not
/// cover the whole crate in every build.
fn crate_denies_unsafe_code(source: &str) -> syn::Result<bool> {
    let file = syn::parse_file(source)?;
    for attr in &file.attrs {
        if !(attr.path().is_ident("deny") || attr.path().is_ident("forbid")) {
            continue;
        }
        let mut found = false;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("unsafe_code") {
                found = true;
            } else if meta.path.is_ident("reason") {
                meta.value()?.parse::<syn::LitStr>()?;
            }
            Ok(())
        })?;
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::crate_denies_unsafe_code;

    #[test]
    fn plain_deny_counts() {
        assert!(crate_denies_unsafe_code("#![deny(unsafe_code)]\nfn main() {}").unwrap());
    }

    #[test]
    fn combined_lint_list_with_reason_counts() {
        let source = "#![deny(dead_code, unsafe_code, reason = \"spec\")]";
        assert!(crate_denies_unsafe_code(source).unwrap());
    }

    #[test]
    fn forbid_counts() {
        assert!(crate_denies_unsafe_code("#![forbid(unsafe_code)]").unwrap());
    }

    #[test]
    fn text_in_comments_and_strings_does_not_count() {
        for source in [
            "/* #![deny(unsafe_code)] */",
            "// #![deny(unsafe_code)]",
            "pub const MARK: &str = \"#![deny(unsafe_code)]\";",
        ] {
            assert!(!crate_denies_unsafe_code(source).unwrap(), "{source}");
        }
    }

    #[test]
    fn non_crate_level_or_conditional_does_not_count() {
        for source in [
            "mod inner {\n    #![deny(unsafe_code)]\n}",
            "#[deny(unsafe_code)]\nfn f() {}",
            "#![cfg_attr(not(test), deny(unsafe_code))]",
            "#![warn(unsafe_code)]",
            "#![deny(unsafe_op_in_unsafe_fn)]",
        ] {
            assert!(!crate_denies_unsafe_code(source).unwrap(), "{source}");
        }
    }

    #[test]
    fn source_that_does_not_parse_is_an_error() {
        assert!(crate_denies_unsafe_code("fn main( {").is_err());
    }
}
