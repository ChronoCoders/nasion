//! Checks the crate-level unsafe_code attributes of every target root in the
//! workspace. The file is parsed with syn, so attribute text inside a comment
//! or a string literal never counts.
//!
//! - Build scripts and proc macros run on the build machine, so a Linux build
//!   never compiles their `cfg(windows)` code. Their roots need
//!   `#![forbid(unsafe_code)]`, which no `allow` further down can lift.
//! - Every other target root needs `#![deny(unsafe_code)]` or the stricter
//!   `#![forbid(unsafe_code)]` (D29).
//! - Library roots also need `forbid(unsafe_code)` and `deny(warnings)` inside
//!   a crate-level `doc(test(attr(...)))`, in one attribute or two (D31, D39).
//!   Crate lints do not reach doctests, and rustdoc adds `allow(unused)` to
//!   them.
//! - No doctest may lower a lint level with allow, expect or warn (D44). The
//!   `doctests` module holds that check.
//! - No `doc(test(attr(...)))` may reach doctests from anywhere but the exact
//!   crate-root form, and no attribute may hold a `$` token (D48). The
//!   `doc_attributes` module holds that check.
//! - Every file rustc reads for a library target, on the host and on each
//!   cross target, is a `.rs` file that check scans (D50). The `dep_info`
//!   module holds that check.
//! - The proc macros in the build graph are exactly an allowlist, by name and
//!   version, and no workspace member has a proc-macro target (D52). The
//!   `proc_macros` module holds that check.
//!
//! The target kind comes from cargo metadata, never from the file name.
//!
//! Exit codes, read by `scripts/checks.sh`: 0 clean, 1 a target lacks an
//! attribute or a doctest lowers a lint level, 2 the check could not run.
#![deny(unsafe_code)]

mod dep_info;
mod doc_attributes;
mod doctests;
mod proc_macros;

use std::ffi::OsString;
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

/// The `level(lint)` pairs every library root sets for its doctests.
const DOCTEST_LINTS: [(&str, &str); 2] = [("forbid", "unsafe_code"), ("deny", "warnings")];
const DOCTEST_ATTRIBUTE: &str = "#![doc(test(attr(forbid(unsafe_code), deny(warnings))))]";

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

/// Returns one line per missing attribute or lowered lint level.
fn check_workspace() -> Result<Vec<String>, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let (root, targets) = workspace_targets(&cargo)?;
    if targets.is_empty() {
        return Err(
            "cargo metadata listed no targets. Check the workspace members in Cargo.toml.".into(),
        );
    }
    let mut missing = Vec::new();
    for target in &targets {
        missing.extend(check_target(target)?);
    }
    let library_roots: Vec<PathBuf> = targets
        .iter()
        .filter(|target| target.has_kind(&LIBRARY_KINDS))
        .map(|target| target.src_path.clone())
        .collect();
    missing.extend(doc_attributes::check(&root, &library_roots)?);
    let mut library_packages: Vec<&str> = targets
        .iter()
        .filter(|target| target.has_kind(&LIBRARY_KINDS))
        .map(|target| target.package.as_str())
        .collect();
    library_packages.sort_unstable();
    library_packages.dedup();
    let (dep_info_count, problems) = dep_info::check(&cargo, &root, &library_packages)?;
    missing.extend(problems);
    let (doctest_count, problems) = doctests::check(&cargo, &root)?;
    missing.extend(problems);
    let (proc_macro_count, problems) = proc_macros::check(&cargo, &root)?;
    missing.extend(problems);
    if missing.is_empty() {
        writeln!(
            io::stdout().lock(),
            "note: {} targets, {doctest_count} doctests, {dep_info_count} dep-info files, {proc_macro_count} proc macros",
            targets.len()
        )
        .map_err(|e| format!("writing the counts to stdout failed: {e}"))?;
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
    if target.has_kind(&LIBRARY_KINDS) {
        for (level, lint) in DOCTEST_LINTS {
            if !doctests_set(&file, level, lint).map_err(parse_error)? {
                missing.push(format!(
                    "{described} lacks {level}({lint}) in a crate-level doc(test(attr(...))). Write {DOCTEST_ATTRIBUTE}"
                ));
            }
        }
    }
    Ok(missing)
}

/// Returns the workspace root and every target of every workspace member. Uses
/// the cargo that ran this tool when there is one, so the pinned toolchain
/// answers.
fn workspace_targets(cargo: &OsString) -> Result<(PathBuf, Vec<Target>), String> {
    let output = Command::new(cargo)
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
    let root = metadata["workspace_root"]
        .as_str()
        .map(PathBuf::from)
        .ok_or("cargo metadata output has no workspace_root")?;
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
    Ok((root, targets))
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

/// True when a crate-level `doc(test(attr(...)))` holds `level(lint)`. Only the
/// exact level counts, so `deny(unsafe_code)`, which a doctest can lift with
/// its own allow, does not stand in for `forbid(unsafe_code)`.
fn doctests_set(file: &syn::File, level: &str, lint: &str) -> syn::Result<bool> {
    for attr in &file.attrs {
        if let Meta::List(doc) = &attr.meta
            && doc.path.is_ident("doc")
            && holds_nested(doc, &["test", "attr", level], lint)?
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
    use super::{Level, doctests_set, unsafe_code_level};

    fn level(source: &str) -> Option<Level> {
        unsafe_code_level(&syn::parse_file(source).unwrap()).unwrap()
    }

    fn doctests(source: &str, level: &str, lint: &str) -> bool {
        doctests_set(&syn::parse_file(source).unwrap(), level, lint).unwrap()
    }

    fn doctests_forbid(source: &str) -> bool {
        doctests(source, "forbid", "unsafe_code")
    }

    fn doctests_deny_warnings(source: &str) -> bool {
        doctests(source, "deny", "warnings")
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
            "/* #![doc(test(attr(forbid(unsafe_code), deny(warnings))))] */",
            "pub const MARK: &str = \"#![doc(test(attr(forbid(unsafe_code), deny(warnings))))]\";",
        ] {
            assert!(level(source).is_none(), "{source}");
            assert!(!doctests_forbid(source), "{source}");
            assert!(!doctests_deny_warnings(source), "{source}");
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
    fn doctest_lints_count_in_one_list_two_attributes_or_either_order() {
        for source in [
            "#![doc(test(attr(forbid(unsafe_code), deny(warnings))))]",
            "#![doc(test(attr(deny(warnings), forbid(unsafe_code))))]",
            "#![doc(test(attr(forbid(unsafe_code))))]\n#![doc(test(attr(deny(warnings))))]",
            "#![doc(test(attr(deny(warnings))))]\n#![doc(test(attr(forbid(unsafe_code))))]",
            "#![doc(html_root_url = \"x\", test(no_crate_inject, attr(deny(dead_code, warnings), forbid(unsafe_code))))]",
        ] {
            assert!(doctests_forbid(source), "{source}");
            assert!(doctests_deny_warnings(source), "{source}");
        }
    }

    #[test]
    fn each_doctest_lint_is_required_on_its_own() {
        let forbid_only = "#![doc(test(attr(forbid(unsafe_code))))]";
        assert!(doctests_forbid(forbid_only));
        assert!(!doctests_deny_warnings(forbid_only));
        let deny_only = "#![doc(test(attr(deny(warnings))))]";
        assert!(!doctests_forbid(deny_only));
        assert!(doctests_deny_warnings(deny_only));
    }

    #[test]
    fn doctest_attribute_that_is_weaker_or_misplaced_does_not_count() {
        for source in [
            "#![doc(test(attr(deny(unsafe_code), warn(warnings))))]",
            "#![doc(test(forbid(unsafe_code), deny(warnings)))]",
            "#![doc(attr(forbid(unsafe_code), deny(warnings)))]",
            "#![forbid(unsafe_code)]\n#![deny(warnings)]",
            "#![cfg_attr(all(), doc(test(attr(forbid(unsafe_code), deny(warnings)))))]",
            "#![doc = \"test(attr(forbid(unsafe_code), deny(warnings)))\"]",
            "#[doc(test(attr(forbid(unsafe_code), deny(warnings))))]\nfn f() {}",
        ] {
            assert!(!doctests_forbid(source), "{source}");
            assert!(!doctests_deny_warnings(source), "{source}");
        }
    }

    #[test]
    fn source_that_does_not_parse_is_an_error() {
        assert!(syn::parse_file("fn main( {").is_err());
        let file = syn::parse_file("#![doc(test(attr(forbid(=))))]").unwrap();
        assert!(doctests_set(&file, "forbid", "unsafe_code").is_err());
    }
}
