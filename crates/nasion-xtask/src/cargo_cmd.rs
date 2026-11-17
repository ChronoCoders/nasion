//! Runs the xtask's cargo commands so their warnings reach the gate (D41).
//! Each command runs with `CARGO_TERM_COLOR=never`, and its stderr is written
//! to the xtask's own stderr, on success as well as on failure, unchanged and
//! after one line naming the command. `check_deny_unsafe` in
//! `scripts/checks.sh` reads everything the xtask prints and fails on a
//! warning line, so the warning pattern lives there alone and the xtask only
//! forwards.

use std::io::Write;
use std::process::{Command, Output};

/// Runs `command`, forwards its stderr to `log` and returns its output.
pub fn run(command: &mut Command, shown: &str, log: &mut dyn Write) -> Result<Output, String> {
    let output = command
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .map_err(|e| format!("starting {shown} failed: {e}"))?;
    forward(&output.stderr, shown, log)?;
    Ok(output)
}

/// Writes `stderr` to `log` after a line naming the command, or nothing when
/// the command printed nothing there.
fn forward(stderr: &[u8], shown: &str, log: &mut dyn Write) -> Result<(), String> {
    if stderr.is_empty() {
        return Ok(());
    }
    let failed = |e: std::io::Error| format!("writing the stderr of {shown} to stderr failed: {e}");
    writeln!(log, "stderr of {shown}:").map_err(failed)?;
    log.write_all(stderr).map_err(failed)?;
    if !stderr.ends_with(b"\n") {
        writeln!(log).map_err(failed)?;
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::forward;

    #[test]
    fn stderr_is_forwarded_unchanged_after_a_line_naming_the_command() {
        let mut log = Vec::new();
        forward(b"warning: one\nwarning: two\n", "cargo x", &mut log).unwrap();
        assert_eq!(
            String::from_utf8(log).unwrap(),
            "stderr of cargo x:\nwarning: one\nwarning: two\n"
        );
        let mut log = Vec::new();
        forward(b"warning: no newline", "cargo x", &mut log).unwrap();
        assert_eq!(
            String::from_utf8(log).unwrap(),
            "stderr of cargo x:\nwarning: no newline\n"
        );
        let mut log = Vec::new();
        forward(b"", "cargo x", &mut log).unwrap();
        assert!(log.is_empty());
    }

    /// Fake cargo scripts for the tests of each site that runs cargo. Unix
    /// only, because they are shell scripts.
    #[cfg(unix)]
    pub mod fake {
        use std::fs;
        use std::path::PathBuf;

        /// A directory holding fake cargo scripts, removed when dropped.
        pub struct FakeCargo(pub PathBuf);

        impl FakeCargo {
            pub fn new(name: &str) -> Self {
                let dir = std::env::temp_dir()
                    .join(format!("nasion-fake-cargo-{name}-{}", std::process::id()));
                let _ = fs::remove_dir_all(&dir);
                fs::create_dir_all(&dir).unwrap();
                Self(dir)
            }

            /// A script that runs `body` and, when `warn` is set, prints the
            /// injected warning line on stderr.
            pub fn script(&self, name: &str, body: &str, warn: bool) -> std::ffi::OsString {
                use std::os::unix::fs::PermissionsExt;
                let path = self.0.join(name);
                let warning = if warn {
                    "echo 'warning: injected by test' >&2\n"
                } else {
                    ""
                };
                fs::write(&path, format!("#!/bin/sh\n{body}\n{warning}exit 0\n")).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                path.into_os_string()
            }
        }

        impl Drop for FakeCargo {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        /// Checks what a site forwarded: the injected line after the header naming
        /// `command`, or nothing at all.
        pub fn assert_forwarded(log: &[u8], command: &str, warn: bool) {
            let log = String::from_utf8_lossy(log);
            if warn {
                assert!(
                    log.starts_with(&format!("stderr of cargo {command}"))
                        && log.ends_with(":\nwarning: injected by test\n"),
                    "{log}"
                );
            } else {
                assert!(log.is_empty(), "{log}");
            }
        }
    }
}
