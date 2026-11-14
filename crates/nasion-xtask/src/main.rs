//! Checks the crate-level unsafe_code attributes of every target root in the
//! workspace. The file is parsed with syn, so attribute text inside a comment
//! or a string literal never counts.
//!
//! - Build scripts and proc macros run on the build machine, so a Linux build
//!   never compiles their `cfg(windows)` code. Their roots need
//!   `#![forbid(unsafe_code)]`, which no `allow` further down can lift.
//! - Every other target root needs `#![deny(unsafe_code)]` or the stricter
//!   `#![forbid(unsafe_code)]` (D29).
//! - Library roots also need `#![doc(test(attr(forbid(unsafe_code))))]`,
//!   because crate lints do not reach doctests.
//!
//! The target kind comes from cargo metadata, never from the file name.
//!
//! Exit codes, read by `scripts/checks.sh`: 0 clean, 1 a target lacks an
//! attribute, 2 the check could not run.
#![deny(unsafe_code)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use syn::punctuated::Punctuated;
use syn::{Meta, MetaList, Token};

const CLEAN: u8 = 0;
const VIOLATION: u8 = 1;
const CANNOT_RUN: u8 = 2;

/// Target kinds that run on the build machine during the build.
const BUILD_MACHINE_KINDS: [&str; 2] = ["custom-build", "proc-macro"];
/// Target kinds rustdoc can run doctests for, now or after a crate-type change.
const LIBRARY_KINDS: [&str; 6] = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

const DOCTEST_FORBID: &str = "#![doc(test(attr(forbid(unsafe_code))))]";

#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Level {
    Deny,
    Forbid,
}

struct Target {
    package: String,
    kinds: Vec<String>,
    src_path: PathBuf,
}

impl Target {
    fn has_kind(&self, kinds: &[&str]) -> bool {
        self.kinds.iter().any(|kind| kinds.contains(&kind.as_str()))
    }
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

/// Returns one line per missing attribute.
fn check_workspace() -> Result<Vec<String>, String> {
    let targets = workspace_targets()?;
    if targets.is_empty() {
        return Err(
            "cargo metadata listed no targets. Check the workspace members in Cargo.toml.".into(),
        );
    }
    let mut missing = Vec::new();
    for target in &targets {
        missing.extend(check_target(target)?);
    }
    if missing.is_empty() {
        writeln!(io::stdout().lock(), "note: {} targets", targets.len())
            .map_err(|e| format!("writing the target count to stdout failed: {e}"))?;
    }
    Ok(missing)
}

fn check_target(target: &Target) -> Result<Vec<String>, String> {
    let path = &target.src_path;
    let file = parse_root(path)?;
    let parse_error = |e: syn::Error| format!("{} does not parse: {e}", path.display());
    let described = format!(
        "{} ({} target of {})",
        path.display(),
        target.kinds.join(", "),
        target.package
    );
    let mut missing = Vec::new();
    let level = unsafe_code_level(&file).map_err(parse_error)?;
    if target.has_kind(&BUILD_MACHINE_KINDS) {
        if level != Some(Level::Forbid) {
            missing.push(format!(
                "{described} lacks a crate-level #![forbid(unsafe_code)], required because it runs on the build machine"
            ));
        }
    } else if level.is_none() {
        missing.push(format!(
            "{described} lacks a crate-level #![deny(unsafe_code)]"
        ));
    }
    if target.has_kind(&LIBRARY_KINDS) && !forbids_unsafe_in_doctests(&file).map_err(parse_error)? {
        missing.push(format!("{described} lacks a crate-level {DOCTEST_FORBID}"));
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
            let kinds = target["kind"]
                .as_array()
                .filter(|kinds| !kinds.is_empty())
                .and_then(|kinds| {
                    kinds
                        .iter()
                        .map(|kind| kind.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| {
                    format!(
                        "target {src_path} of {name} in cargo metadata has no list of kind names"
                    )
                })?;
            targets.push(Target {
                package: name.to_owned(),
                kinds,
                src_path: PathBuf::from(src_path),
            });
        }
    }
    Ok(targets)
}

fn parse_root(path: &Path) -> Result<syn::File, String> {
    let source =
        fs::read_to_string(path).map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
    syn::parse_file(&source).map_err(|e| format!("{} does not parse: {e}", path.display()))
}

/// The strongest crate-level `deny(unsafe_code)` or `forbid(unsafe_code)`,
/// alone or with other lints. An attribute inside `cfg_attr`, on an item or on
/// a module does not count, because it does not cover the whole crate in every
/// build.
fn unsafe_code_level(file: &syn::File) -> syn::Result<Option<Level>> {
    let mut strongest = None;
    for attr in &file.attrs {
        let level = if attr.path().is_ident("forbid") {
            Level::Forbid
        } else if attr.path().is_ident("deny") {
            Level::Deny
        } else {
            continue;
        };
        let mut found = false;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("unsafe_code") {
                found = true;
            } else if meta.path.is_ident("reason") {
                meta.value()?.parse::<syn::LitStr>()?;
            }
            Ok(())
        })?;
        if found && strongest < Some(level) {
            strongest = Some(level);
        }
    }
    Ok(strongest)
}

/// True when a crate-level `doc(test(attr(...)))` holds `forbid(unsafe_code)`.
/// deny does not count, because a doctest can lift it with its own allow.
fn forbids_unsafe_in_doctests(file: &syn::File) -> syn::Result<bool> {
    for attr in &file.attrs {
        if let Meta::List(doc) = &attr.meta
            && doc.path.is_ident("doc")
            && holds_nested(doc, &["test", "attr", "forbid"], "unsafe_code")?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True when the arguments of `list` hold `names[0](names[1](...(leaf)))`.
fn holds_nested(list: &MetaList, names: &[&str], leaf: &str) -> syn::Result<bool> {
    let items = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
    for item in &items {
        let found = match (names.split_first(), item) {
            (None, Meta::Path(path)) => path.is_ident(leaf),
            (Some((name, rest)), Meta::List(inner)) if inner.path.is_ident(name) => {
                holds_nested(inner, rest, leaf)?
            }
            _ => false,
        };
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{Level, forbids_unsafe_in_doctests, unsafe_code_level};

    fn level(source: &str) -> Option<Level> {
        unsafe_code_level(&syn::parse_file(source).unwrap()).unwrap()
    }

    fn doctests_forbid(source: &str) -> bool {
        forbids_unsafe_in_doctests(&syn::parse_file(source).unwrap()).unwrap()
    }

    #[test]
    fn plain_deny_is_deny() {
        assert!(level("#![deny(unsafe_code)]\nfn main() {}") == Some(Level::Deny));
    }

    #[test]
    fn combined_lint_list_with_reason_counts() {
        let source = "#![deny(dead_code, unsafe_code, reason = \"spec\")]";
        assert!(level(source) == Some(Level::Deny));
    }

    #[test]
    fn forbid_is_forbid_whatever_the_order() {
        assert!(level("#![forbid(unsafe_code)]") == Some(Level::Forbid));
        assert!(level("#![forbid(unsafe_code)]\n#![deny(unsafe_code)]") == Some(Level::Forbid));
        assert!(level("#![deny(unsafe_code)]\n#![forbid(unsafe_code)]") == Some(Level::Forbid));
    }

    #[test]
    fn text_in_comments_and_strings_does_not_count() {
        for source in [
            "/* #![forbid(unsafe_code)] */",
            "// #![deny(unsafe_code)]",
            "pub const MARK: &str = \"#![forbid(unsafe_code)]\";",
            "/* #![doc(test(attr(forbid(unsafe_code))))] */",
            "pub const MARK: &str = \"#![doc(test(attr(forbid(unsafe_code))))]\";",
        ] {
            assert!(level(source).is_none(), "{source}");
            assert!(!doctests_forbid(source), "{source}");
        }
    }

    #[test]
    fn non_crate_level_or_conditional_does_not_count() {
        for source in [
            "mod inner {\n    #![deny(unsafe_code)]\n}",
            "#[forbid(unsafe_code)]\nfn f() {}",
            "#![cfg_attr(not(test), forbid(unsafe_code))]",
            "#![warn(unsafe_code)]",
            "#![deny(unsafe_op_in_unsafe_fn)]",
        ] {
            assert!(level(source).is_none(), "{source}");
        }
    }

    #[test]
    fn doctest_forbid_counts_alone_or_with_other_settings() {
        assert!(doctests_forbid("#![doc(test(attr(forbid(unsafe_code))))]"));
        assert!(doctests_forbid(
            "#![doc(html_root_url = \"x\", test(no_crate_inject, attr(deny(warnings), forbid(dead_code, unsafe_code))))]"
        ));
    }

    #[test]
    fn doctest_attribute_that_is_weaker_or_misplaced_does_not_count() {
        for source in [
            "#![doc(test(attr(deny(unsafe_code))))]",
            "#![doc(test(forbid(unsafe_code)))]",
            "#![doc(attr(forbid(unsafe_code)))]",
            "#![forbid(unsafe_code)]",
            "#![cfg_attr(all(), doc(test(attr(forbid(unsafe_code)))))]",
            "#![doc = \"test(attr(forbid(unsafe_code)))\"]",
            "#[doc(test(attr(forbid(unsafe_code))))]\nfn f() {}",
        ] {
            assert!(!doctests_forbid(source), "{source}");
        }
    }

    #[test]
    fn source_that_does_not_parse_is_an_error() {
        assert!(syn::parse_file("fn main( {").is_err());
        let file = syn::parse_file("#![doc(test(attr(forbid(=))))]").unwrap();
        assert!(forbids_unsafe_in_doctests(&file).is_err());
    }
}
