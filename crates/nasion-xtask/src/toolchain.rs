//! Reads the cross targets from rust-toolchain.toml, the one place they are
//! written (D59). `scripts/checks.sh` gets the same list from this reader
//! through the `toolchain-targets` subcommand, so the shell and the xtask
//! cannot disagree.
//!
//! The reader is strict: a `[toolchain]` table holding at most the keys the
//! file holds today, each once, with `channel` and `profile` as quoted plain
//! tokens and `components` and `targets` as one-line lists of them. Blank
//! lines and whole-line comments are skipped. Anything else is an error, as
//! is a missing or empty `targets` list. The host target is a requirement of
//! where the gate runs, not a toolchain setting, so it may not be listed.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// The keys rust-toolchain.toml may hold under `[toolchain]`.
const KEYS: [&str; 4] = ["channel", "components", "profile", "targets"];
/// Set by `scripts/checks.sh`, which holds the one copy of the host triple.
pub const HOST_VAR: &str = "NASION_HOST_TARGET";

/// The host target the gate passed in `NASION_HOST_TARGET`.
pub fn host_target(value: Option<String>) -> Result<String, String> {
    value.filter(|host| is_token(host)).ok_or_else(|| {
        format!(
            "{HOST_VAR} is not set to a target triple. scripts/checks.sh sets it, so run this through gate.sh."
        )
    })
}

/// The cross targets listed in `root/rust-toolchain.toml`.
pub fn cross_targets(root: &Path, host: &str) -> Result<Vec<String>, String> {
    let path = root.join("rust-toolchain.toml");
    let text =
        fs::read_to_string(&path).map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
    let targets = parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if targets.iter().any(|target| target == host) {
        return Err(format!(
            "{} lists the host target {host} in targets. The gate runs on {host} without --target, so only cross targets belong there (D59).",
            path.display()
        ));
    }
    Ok(targets)
}

fn parse(text: &str) -> Result<Vec<String>, String> {
    let mut in_table = false;
    let mut seen = BTreeSet::new();
    let mut targets = None;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !in_table {
            if line != "[toolchain]" {
                return Err(format!(
                    "line {number} is {line:?}, expected the [toolchain] table first"
                ));
            }
            in_table = true;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {number} is {line:?}, expected key = value"));
        };
        let (key, value) = (key.trim(), value.trim());
        if !KEYS.contains(&key) {
            return Err(format!(
                "line {number} has the key {key:?}, which is not one of {}",
                KEYS.join(", ")
            ));
        }
        if !seen.insert(key) {
            return Err(format!("line {number} repeats the key {key}"));
        }
        let at = |e: String| format!("line {number}, {key}: {e}");
        match key {
            "channel" | "profile" => {
                quoted_token(value).map_err(at)?;
            }
            _ => {
                let list = token_list(value).map_err(at)?;
                if key == "targets" {
                    targets = Some(list);
                }
            }
        }
    }
    let targets = targets.ok_or("there is no targets list")?;
    if targets.is_empty() {
        return Err("the targets list is empty".into());
    }
    Ok(targets)
}

/// A list such as `["a", "b"]` on one line, with an optional trailing comma.
fn token_list(value: &str) -> Result<Vec<String>, String> {
    let inner = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| format!("{value:?} is not a one-line list"))?;
    let entries: Vec<&str> = inner.split(',').map(str::trim).collect();
    let last = entries.len() - 1;
    let mut tokens = Vec::new();
    for (position, entry) in entries.into_iter().enumerate() {
        if entry.is_empty() && position == last {
            continue;
        }
        tokens.push(quoted_token(entry)?);
    }
    Ok(tokens)
}

fn quoted_token(value: &str) -> Result<String, String> {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .filter(|inner| is_token(inner))
        .map(str::to_owned)
        .ok_or_else(|| format!("{value:?} is not a quoted plain token"))
}

/// Letters, digits, `.`, `_` and `-` only, as in a target triple.
fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{cross_targets, host_target, parse};

    const HOST: &str = "x86_64-unknown-linux-gnu";
    const TODAY: &str = "[toolchain]\nchannel = \"1.100.0\"\ncomponents = [\"rustfmt\", \"clippy\"]\nprofile = \"minimal\"\ntargets = [\"x86_64-pc-windows-msvc\"]\n";

    fn error(text: &str) -> String {
        parse(text).unwrap_err()
    }

    #[test]
    fn the_current_file_gives_its_cross_target() {
        assert_eq!(parse(TODAY).unwrap(), ["x86_64-pc-windows-msvc"]);
        let real = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../rust-toolchain.toml"
        ))
        .unwrap();
        assert!(!parse(&real).unwrap().is_empty());
    }

    #[test]
    fn two_cross_targets_comments_and_blank_lines_are_read() {
        let text = "# pinned\n\n[toolchain]\n  targets = [ \"x86_64-pc-windows-msvc\" , \"aarch64-unknown-linux-gnu\", ]\n";
        assert_eq!(
            parse(text).unwrap(),
            ["x86_64-pc-windows-msvc", "aarch64-unknown-linux-gnu"]
        );
    }

    #[test]
    fn anything_else_is_an_error() {
        for (text, what) in [
            (
                "targets = [\"a\"]\n",
                "expected the [toolchain] table first",
            ),
            ("[toolchain]\n[other]\n", "expected key = value"),
            ("[toolchain]\nflavor = \"x\"\n", "has the key \"flavor\""),
            (
                "[toolchain]\ntargets = [\"a\"]\ntargets = [\"b\"]\n",
                "repeats the key targets",
            ),
            (
                "[toolchain]\nchannel = \"1\"\nchannel = \"2\"\n",
                "repeats the key channel",
            ),
            (
                "[toolchain]\nchannel = \"1.100.0\"\n",
                "there is no targets list",
            ),
            ("", "there is no targets list"),
            ("[toolchain]\ntargets = []\n", "the targets list is empty"),
            (
                "[toolchain]\ntargets = [1]\n",
                "\"1\" is not a quoted plain token",
            ),
            (
                "[toolchain]\ntargets = [\"x86 64\"]\n",
                "is not a quoted plain token",
            ),
            (
                "[toolchain]\ntargets = [\"a\\\"b\"]\n",
                "is not a quoted plain token",
            ),
            (
                "[toolchain]\ntargets = [\"a,b\"]\n",
                "is not a quoted plain token",
            ),
            (
                "[toolchain]\ntargets = [\"a\",, \"b\"]\n",
                "is not a quoted plain token",
            ),
            ("[toolchain]\ntargets = \"a\"\n", "is not a one-line list"),
            (
                "[toolchain]\ntargets = [\n\"a\"]\n",
                "is not a one-line list",
            ),
            ("[toolchain]\nchannel = 1\n", "is not a quoted plain token"),
            (
                "[toolchain]\nprofile = \"\"\n",
                "is not a quoted plain token",
            ),
            (
                "[toolchain]\ncomponents = [\"rust fmt\"]\n",
                "is not a quoted plain token",
            ),
        ] {
            let found = error(text);
            assert!(found.contains(what), "{text:?}: {found}");
        }
        assert!(error("[toolchain]\n\nflavor = 1\n").starts_with("line 3 "));
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("nasion-d59-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_file_is_read_and_the_host_is_refused() {
        let scratch = Scratch::new("file");
        let missing = cross_targets(&scratch.0, HOST).unwrap_err();
        assert!(
            missing.contains("rust-toolchain.toml cannot be read"),
            "{missing}"
        );
        fs::create_dir(scratch.0.join("rust-toolchain.toml")).unwrap();
        let unreadable = cross_targets(&scratch.0, HOST).unwrap_err();
        assert!(unreadable.contains("cannot be read"), "{unreadable}");
        fs::remove_dir(scratch.0.join("rust-toolchain.toml")).unwrap();
        fs::write(scratch.0.join("rust-toolchain.toml"), TODAY).unwrap();
        assert_eq!(
            cross_targets(&scratch.0, HOST).unwrap(),
            ["x86_64-pc-windows-msvc"]
        );
        fs::write(
            scratch.0.join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"x86_64-pc-windows-msvc\", \"x86_64-unknown-linux-gnu\"]\n",
        )
        .unwrap();
        let host = cross_targets(&scratch.0, HOST).unwrap_err();
        assert!(
            host.ends_with("lists the host target x86_64-unknown-linux-gnu in targets. The gate runs on x86_64-unknown-linux-gnu without --target, so only cross targets belong there (D59)."),
            "{host}"
        );
        fs::write(scratch.0.join("rust-toolchain.toml"), "[toolchain]\n").unwrap();
        let broken = cross_targets(&scratch.0, HOST).unwrap_err();
        assert!(
            broken.ends_with("rust-toolchain.toml: there is no targets list"),
            "{broken}"
        );
    }

    #[test]
    fn the_host_comes_from_the_gate() {
        assert_eq!(host_target(Some(HOST.into())).unwrap(), HOST);
        for value in [None, Some(String::new()), Some("x86 64".into())] {
            assert!(host_target(value).is_err());
        }
    }
}
