//! Keeps cargo from taking package code from anywhere but the lock file says
//! (D53). A `[patch]` or `[replace]` table in a workspace manifest, or a cargo
//! config that replaces a source, serves other code while the package id can
//! still name crates.io, so the pinned checksums alone do not cover it.
//!
//! Where cargo reads config, read in rust-lang/cargo at commit
//! 3acc79c0c841844675bc53a605922dbfc441e750 (cargo 1.100.0):
//!
//! - `walk_tree` and `get_file_path` in src/context/mod.rs: `.cargo/config`
//!   and `.cargo/config.toml` in the current directory and every ancestor,
//!   then the same two names in `CARGO_HOME`. The file without an extension
//!   wins when both exist, so both are read here.
//! - doc/book/src/reference/config.md: `source` (source replacement),
//!   `patch`, `paths` (path overrides), `include` (pulls in other files),
//!   `registry.index` and `registries.<name>.index` decide where package code
//!   comes from. The page marks `source.*`, `paths` and `include` as not
//!   settable from the environment.
//! - `ConfigMapAccess::new_map` in src/context/de.rs: tables such as
//!   `source.<name>` and `patch.<registry>` take entries from the environment
//!   only under the unstable `advanced-env` flag, so `CARGO_UNSTABLE_ADVANCED_ENV`
//!   counts as a source key.
//! - `SourceConfigMap::empty` in src/sources/config.rs: the
//!   `__CARGO_TEST_CRATES_IO_URL_DO_NOT_USE_THIS` variable replaces crates.io.
//! - `--config` takes the same keys on the command line. The gate never passes
//!   it, and a running process cannot see another's arguments.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Top-level manifest tables that override where dependencies come from.
const MANIFEST_TABLES: [&str; 2] = ["patch", "replace"];
/// Top-level cargo config keys that decide where package code comes from.
const CONFIG_KEYS: [&str; 4] = ["source", "patch", "paths", "include"];
/// Environment variables cargo reads for those keys, as exact names.
const ENV_NAMES: [&str; 5] = [
    "CARGO_PATHS",
    "CARGO_INCLUDE",
    "CARGO_REGISTRY_INDEX",
    "CARGO_UNSTABLE_ADVANCED_ENV",
    "__CARGO_TEST_CRATES_IO_URL_DO_NOT_USE_THIS",
];
/// Environment variable prefixes cargo maps onto those keys.
const ENV_PREFIXES: [&str; 2] = ["CARGO_SOURCE_", "CARGO_PATCH_"];

/// What the check found: lines for manifest tables, and lines for config
/// files and environment variables. The second kind changes what every later
/// cargo run builds, so the caller stops before running cargo again.
pub struct Found {
    pub manifests: Vec<String>,
    pub configs: Vec<String>,
}

/// One line per manifest table, config key or environment variable that could
/// change where package code comes from.
pub fn check(root: &Path, manifests: &[PathBuf]) -> Result<Found, String> {
    let mut found = Found {
        manifests: Vec::new(),
        configs: Vec::new(),
    };
    for manifest in manifests {
        found
            .manifests
            .extend(manifest_problems(manifest, &read(manifest)?)?);
    }
    let home = cargo_home(std::env::var_os("CARGO_HOME"), std::env::var_os("HOME"))?;
    found.configs.extend(config_file_problems(root, &home)?);
    let names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    found
        .configs
        .extend(env_problems(names.iter().map(String::as_str)));
    Ok(found)
}

/// Where cargo looks for its home config: `CARGO_HOME`, else `$HOME/.cargo`.
fn cargo_home(cargo_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf, String> {
    match (cargo_home, home) {
        (Some(cargo_home), _) => Ok(PathBuf::from(cargo_home)),
        (None, Some(home)) => Ok(PathBuf::from(home).join(".cargo")),
        (None, None) => {
            Err("neither CARGO_HOME nor HOME is set, so cargo's home config cannot be found".into())
        }
    }
}

fn config_file_problems(root: &Path, home: &Path) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    for file in config_files(root, home) {
        if file.exists() {
            problems.extend(config_problems(&file, &read(&file)?)?);
        }
    }
    Ok(problems)
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{} cannot be read: {e}", path.display()))
}

/// The config files cargo reads when run in `root`, in cargo's order.
fn config_files(root: &Path, home: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in root
        .ancestors()
        .map(|dir| dir.join(".cargo"))
        .chain([home.to_path_buf()])
    {
        for name in ["config", "config.toml"] {
            let file = dir.join(name);
            if !files.contains(&file) {
                files.push(file);
            }
        }
    }
    files
}

fn manifest_problems(path: &Path, text: &str) -> Result<Vec<String>, String> {
    let keys = key_paths(text).map_err(|e| format!("{}: {e}", path.display()))?;
    let found: BTreeSet<&str> = keys
        .iter()
        .filter_map(|key| key.first())
        .map(String::as_str)
        .filter(|first| MANIFEST_TABLES.contains(first))
        .collect();
    Ok(found
        .into_iter()
        .map(|table| {
            format!(
                "{} has a [{table}] table, which no workspace manifest may have (D53).",
                path.display()
            )
        })
        .collect())
}

fn config_problems(path: &Path, text: &str) -> Result<Vec<String>, String> {
    let keys = key_paths(text).map_err(|e| format!("{}: {e}", path.display()))?;
    let found: BTreeSet<String> = keys
        .iter()
        .filter_map(|key| match key.as_slice() {
            [first, ..] if CONFIG_KEYS.contains(&first.as_str()) => Some(first.clone()),
            [registry, index, ..] if registry == "registry" && index == "index" => {
                Some("registry.index".to_owned())
            }
            [registries, name, index, ..] if registries == "registries" && index == "index" => {
                Some(format!("registries.{name}.index"))
            }
            _ => None,
        })
        .collect();
    Ok(found
        .into_iter()
        .map(|key| {
            format!(
                "{} sets {key}, which can change where cargo takes package code from (D53).",
                path.display()
            )
        })
        .collect())
}

fn env_problems<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut found: Vec<&str> = names
        .filter(|name| {
            ENV_NAMES.contains(name)
                || ENV_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
                || (name.starts_with("CARGO_REGISTRIES_") && name.ends_with("_INDEX"))
        })
        .collect();
    found.sort_unstable();
    found
        .into_iter()
        .map(|name| {
            format!(
                "the environment sets {name}, which can change where cargo takes package code from (D53)."
            )
        })
        .collect()
}

/// Every key path a TOML document defines: table headers, keys (dotted or
/// not) and the keys of inline tables, each with the table path in front.
/// Values are skipped but read fully, so a string or array spanning lines
/// never looks like a key. Anything that is not valid TOML in this sense is an
/// error.
fn key_paths(text: &str) -> Result<Vec<Vec<String>>, String> {
    let mut reader = Reader {
        chars: text.chars().collect(),
        at: 0,
        line: 1,
        keys: Vec::new(),
    };
    reader.document()?;
    Ok(reader.keys)
}

/// True for a TOML local date such as 1979-05-27, the only value form that
/// may be followed by a space and a time.
fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

struct Reader {
    chars: Vec<char>,
    at: usize,
    line: usize,
    keys: Vec<Vec<String>>,
}

impl Reader {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.at + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.at += 1;
        if ch == '\n' {
            self.line += 1;
        }
        Some(ch)
    }

    fn error(&self, what: &str) -> String {
        format!("line {}: {what}", self.line)
    }

    fn expect(&mut self, ch: char) -> Result<(), String> {
        if self.bump() == Some(ch) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {ch:?}")))
        }
    }

    fn starts_with(&self, text: &str) -> bool {
        text.chars()
            .enumerate()
            .all(|(i, ch)| self.peek_at(i) == Some(ch))
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.bump();
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.bump();
            }
        }
    }

    /// Skips spaces, comments and line breaks.
    fn skip_blank(&mut self) {
        loop {
            self.skip_spaces();
            self.skip_comment();
            match self.peek() {
                Some('\n') => {
                    self.bump();
                }
                Some('\r') if self.peek_at(1) == Some('\n') => {
                    self.bump();
                }
                _ => return,
            }
        }
    }

    fn end_of_line(&mut self) -> Result<(), String> {
        self.skip_spaces();
        self.skip_comment();
        if self.peek() == Some('\r') {
            self.bump();
        }
        match self.bump() {
            None | Some('\n') => Ok(()),
            Some(_) => Err(self.error("expected the end of the line")),
        }
    }

    fn document(&mut self) -> Result<(), String> {
        let mut table = Vec::new();
        loop {
            self.skip_blank();
            match self.peek() {
                None => return Ok(()),
                Some('[') => {
                    self.bump();
                    let array = self.peek() == Some('[');
                    if array {
                        self.bump();
                    }
                    self.skip_spaces();
                    table = self.key()?;
                    self.skip_spaces();
                    self.expect(']')?;
                    if array {
                        self.expect(']')?;
                    }
                    self.keys.push(table.clone());
                }
                Some(_) => {
                    let mut path = table.clone();
                    path.extend(self.key()?);
                    self.skip_spaces();
                    self.expect('=')?;
                    self.keys.push(path.clone());
                    self.value(&path)?;
                }
            }
            self.end_of_line()?;
        }
    }

    /// A key of one or more segments joined by dots.
    fn key(&mut self) -> Result<Vec<String>, String> {
        let mut segments = Vec::new();
        loop {
            self.skip_spaces();
            let segment = match self.peek() {
                Some('"') => self.basic_string()?,
                Some('\'') => self.literal_string()?,
                _ => {
                    let mut bare = String::new();
                    while let Some(ch) = self
                        .peek()
                        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
                    {
                        bare.push(ch);
                        self.bump();
                    }
                    if bare.is_empty() {
                        return Err(self.error("expected a key"));
                    }
                    bare
                }
            };
            segments.push(segment);
            self.skip_spaces();
            if self.peek() != Some('.') {
                return Ok(segments);
            }
            self.bump();
        }
    }

    fn value(&mut self, path: &[String]) -> Result<(), String> {
        self.skip_spaces();
        match self.peek() {
            Some('"') if self.starts_with("\"\"\"") => self.multiline('"'),
            Some('\'') if self.starts_with("'''") => self.multiline('\''),
            Some('"') => self.basic_string().map(drop),
            Some('\'') => self.literal_string().map(drop),
            Some('[') => {
                self.bump();
                loop {
                    self.skip_blank();
                    if self.peek() == Some(']') {
                        self.bump();
                        return Ok(());
                    }
                    self.value(path)?;
                    self.skip_blank();
                    match self.bump() {
                        Some(',') => {}
                        Some(']') => return Ok(()),
                        _ => return Err(self.error("expected ',' or ']' in an array")),
                    }
                }
            }
            Some('{') => {
                self.bump();
                loop {
                    self.skip_blank();
                    if self.peek() == Some('}') {
                        self.bump();
                        return Ok(());
                    }
                    let mut inner = path.to_vec();
                    inner.extend(self.key()?);
                    self.skip_spaces();
                    self.expect('=')?;
                    self.keys.push(inner.clone());
                    self.value(&inner)?;
                    self.skip_blank();
                    match self.bump() {
                        Some(',') => {}
                        Some('}') => return Ok(()),
                        _ => return Err(self.error("expected ',' or '}' in an inline table")),
                    }
                }
            }
            _ => {
                let mut scalar = String::new();
                while let Some(ch) = self
                    .peek()
                    .filter(|ch| !matches!(ch, ',' | ']' | '}' | '\n' | '\r' | '#'))
                {
                    scalar.push(ch);
                    self.bump();
                }
                let scalar = scalar.trim();
                let plain = |part: &str| {
                    !part.is_empty()
                        && part.chars().all(|ch| {
                            ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '_' | '.' | ':')
                        })
                };
                let valid = match scalar.split_once(' ') {
                    None => plain(scalar),
                    Some((date, time)) => is_date(date) && plain(time),
                };
                if !valid {
                    return Err(self.error("expected a value"));
                }
                Ok(())
            }
        }
    }

    fn basic_string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut text = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(self.error("unterminated string")),
                Some('"') => return Ok(text),
                Some('\\') => text.push(self.escape()?),
                Some(ch) => text.push(ch),
            }
        }
    }

    fn escape(&mut self) -> Result<char, String> {
        let hex = |reader: &mut Self, digits: usize| -> Result<char, String> {
            let mut code = String::new();
            for _ in 0..digits {
                code.extend(reader.bump());
            }
            u32::from_str_radix(&code, 16)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| reader.error("invalid escape"))
        };
        match self.bump() {
            Some('b') => Ok('\u{8}'),
            Some('t') => Ok('\t'),
            Some('n') => Ok('\n'),
            Some('f') => Ok('\u{c}'),
            Some('r') => Ok('\r'),
            Some('e') => Ok('\u{1b}'),
            Some('"') => Ok('"'),
            Some('\\') => Ok('\\'),
            Some('x') => hex(self, 2),
            Some('u') => hex(self, 4),
            Some('U') => hex(self, 8),
            _ => Err(self.error("invalid escape")),
        }
    }

    fn literal_string(&mut self) -> Result<String, String> {
        self.expect('\'')?;
        let mut text = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(self.error("unterminated string")),
                Some('\'') => return Ok(text),
                Some(ch) => text.push(ch),
            }
        }
    }

    /// A multiline string. In the basic form a backslash escapes the next
    /// character, and up to two extra quotes may sit before the closing three.
    fn multiline(&mut self, quote: char) -> Result<(), String> {
        for _ in 0..3 {
            self.bump();
        }
        let close: String = [quote; 3].iter().collect();
        loop {
            if self.starts_with(&close) {
                for _ in 0..3 {
                    self.bump();
                }
                for _ in 0..2 {
                    if self.peek() == Some(quote) {
                        self.bump();
                    }
                }
                return Ok(());
            }
            match self.bump() {
                None => return Err(self.error("unterminated multiline string")),
                Some('\\') if quote == '"' => {
                    if self.bump().is_none() {
                        return Err(self.error("unterminated multiline string"));
                    }
                }
                Some(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{
        cargo_home, config_file_problems, config_files, config_problems, env_problems, key_paths,
        manifest_problems,
    };

    fn paths(text: &str) -> Vec<String> {
        key_paths(text)
            .unwrap()
            .into_iter()
            .map(|key| key.join("."))
            .collect()
    }

    #[test]
    fn keys_come_from_headers_dotted_keys_and_inline_tables() {
        let text = "# top\nname = \"x\" # trailing\n[a.b]\nc = 1\n\"q.d\".'e' = true\n[[arr]]\nf = { g = 1, h = { i = [1, 2] } }\n[ spaced . key ]\nj = 1979-05-27 07:32:00\n";
        assert_eq!(
            paths(text),
            [
                "name",
                "a.b",
                "a.b.c",
                "a.b.q.d.e",
                "arr",
                "arr.f",
                "arr.f.g",
                "arr.f.h",
                "arr.f.h.i",
                "spaced.key",
                "spaced.key.j",
            ]
        );
    }

    #[test]
    fn values_that_span_lines_never_look_like_keys() {
        let text = concat!(
            "a = \"\"\"\n[patch.crates-io]\nx = \\\"\"\"\"\"\n",
            "b = '''\n[replace]\n'''''\n",
            "c = [\n  1, # comment\n  [2, 3],\n  { d = 4 },\n]\n",
            "e = { f = 1,\n g = 2 }\r\n",
            "h = 'lit'\n",
        );
        assert_eq!(paths(text), ["a", "b", "c", "c.d", "e", "e.f", "e.g", "h"]);
    }

    #[test]
    fn quoted_keys_are_unescaped() {
        assert_eq!(paths("\"\\u0070atch\".x = 1"), ["patch.x"]);
        assert_eq!(paths("\"pa\\x74ch\" = 1"), ["patch"]);
        assert_eq!(paths("\"\\U00000070atch\" = 1"), ["patch"]);
        assert_eq!(
            paths("\"\\b\\t\\n\\f\\r\\e\\\"\\\\\" = 1"),
            ["\u{8}\t\n\u{c}\r\u{1b}\"\\"]
        );
    }

    #[test]
    fn text_that_is_not_toml_is_an_error() {
        for text in [
            "a = \"open",
            "a = 'open",
            "a = \"\\q\"",
            "a = \"\\u12\"",
            "a = \"\"\"open",
            "a = \"\"\"\\",
            "a 1",
            "= 1",
            "a = ",
            "a = @",
            "a = 1 b",
            "a = 1979-05-27 07:32:00 x",
            "a = 19790527 07:32:00",
            "[a",
            "[[a]",
            "a = [1 2]",
            "a = {b = 1 c = 2}",
            "a = {b 1}",
            "a. = 1",
        ] {
            assert!(key_paths(text).is_err(), "{text:?}");
        }
        assert!(
            key_paths("a = 1\nb = \"x")
                .unwrap_err()
                .starts_with("line 2: ")
        );
    }

    #[test]
    fn manifests_with_patch_or_replace_are_rejected() {
        let path = Path::new("Cargo.toml");
        for (text, table) in [
            (
                "[patch.crates-io]\nserde_derive = { path = \"/tmp/x\" }\n",
                "patch",
            ),
            ("[replace]\n\"serde:1.0.0\" = { path = \"x\" }\n", "replace"),
            ("patch.crates-io.syn = { path = \"x\" }\n", "patch"),
            (
                "patch = { crates-io = { syn = { path = \"x\" } } }\n",
                "patch",
            ),
            ("\"patch\".crates-io.syn.path = \"x\"\n", "patch"),
            ("[[patch.unused]]\nname = \"x\"\n", "patch"),
        ] {
            assert_eq!(
                manifest_problems(path, text).unwrap(),
                [format!(
                    "Cargo.toml has a [{table}] table, which no workspace manifest may have (D53)."
                )],
                "{text}"
            );
        }
        for text in [
            "[package]\nname = \"x\"\n[dependencies]\npatch = \"1\"\nreplace = { version = \"1\" }\n",
            "[workspace]\nmembers = [\"crates/*\"]\n",
            "",
        ] {
            assert!(manifest_problems(path, text).unwrap().is_empty(), "{text}");
        }
        assert!(manifest_problems(path, "[patch").is_err());
    }

    #[test]
    fn config_keys_that_move_package_code_are_rejected() {
        let path = Path::new("config.toml");
        for (text, key) in [
            ("[source.crates-io]\nreplace-with = \"v\"\n", "source"),
            ("[source.v]\ndirectory = \"vendor\"\n", "source"),
            ("[patch.crates-io]\nsyn = { path = \"x\" }\n", "patch"),
            ("paths = [\"../x\"]\n", "paths"),
            ("include = [\"other.toml\"]\n", "include"),
            ("[registry]\nindex = \"https://x\"\n", "registry.index"),
            (
                "registries.mine.index = \"https://x\"\n",
                "registries.mine.index",
            ),
            (
                "[registries]\ncrates-io = { index = \"https://x\" }\n",
                "registries.crates-io.index",
            ),
        ] {
            assert_eq!(
                config_problems(path, text).unwrap(),
                [format!(
                    "config.toml sets {key}, which can change where cargo takes package code from (D53)."
                )],
                "{text}"
            );
        }
        for text in [
            "[build]\njobs = 1\nrustc-wrapper = \"sccache\"\n",
            "[registries.crates-io]\nprotocol = \"sparse\"\n",
            "[registry]\ndefault = \"crates-io\"\n",
            "[net]\noffline = true\n",
        ] {
            assert!(config_problems(path, text).unwrap().is_empty(), "{text}");
        }
        assert!(config_problems(path, "[source").is_err());
    }

    #[test]
    fn config_files_follow_cargo_order_and_read_both_names() {
        let files = config_files(Path::new("/w/s"), Path::new("/h/.cargo"));
        let expected: Vec<PathBuf> = [
            "/w/s/.cargo/config",
            "/w/s/.cargo/config.toml",
            "/w/.cargo/config",
            "/w/.cargo/config.toml",
            "/.cargo/config",
            "/.cargo/config.toml",
            "/h/.cargo/config",
            "/h/.cargo/config.toml",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(files, expected);
        assert_eq!(
            config_files(Path::new("/h/w"), Path::new("/h/.cargo")).len(),
            6
        );
    }

    #[test]
    fn the_home_comes_from_cargo_home_then_home() {
        let some = |text: &str| Some(OsString::from(text));
        assert_eq!(
            cargo_home(some("/c"), some("/h")).unwrap(),
            PathBuf::from("/c")
        );
        assert_eq!(
            cargo_home(None, some("/h")).unwrap(),
            PathBuf::from("/h/.cargo")
        );
        assert!(cargo_home(None, None).is_err());
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("nasion-d53-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn every_config_location_is_read() {
        let scratch = Scratch::new("locations");
        let root = scratch.0.join("parent/ws");
        fs::create_dir_all(&root).unwrap();
        let home = scratch.0.join("home");
        assert!(config_file_problems(&root, &home).unwrap().is_empty());
        scratch.write("parent/ws/.cargo/config.toml", "[build]\njobs = 1\n");
        scratch.write(
            "parent/ws/.cargo/config",
            "[source.crates-io]\nreplace-with = \"v\"\n",
        );
        scratch.write("parent/.cargo/config.toml", "paths = [\"x\"]\n");
        scratch.write("home/config", "include = [\"x.toml\"]\n");
        scratch.write(
            "home/config.toml",
            "[patch.crates-io]\nsyn = { path = \"x\" }\n",
        );
        let found = config_file_problems(&root, &home).unwrap();
        let shown: Vec<String> = found
            .iter()
            .map(|line| line.replace(&scratch.0.display().to_string(), "S"))
            .collect();
        assert_eq!(
            shown,
            [
                "S/parent/ws/.cargo/config sets source, which can change where cargo takes package code from (D53).",
                "S/parent/.cargo/config.toml sets paths, which can change where cargo takes package code from (D53).",
                "S/home/config sets include, which can change where cargo takes package code from (D53).",
                "S/home/config.toml sets patch, which can change where cargo takes package code from (D53).",
            ]
        );
    }

    #[test]
    fn an_unreadable_or_broken_config_stops_the_check() {
        let scratch = Scratch::new("unreadable");
        fs::create_dir_all(scratch.0.join("ws/.cargo/config.toml")).unwrap();
        let error =
            config_file_problems(&scratch.0.join("ws"), &scratch.0.join("home")).unwrap_err();
        assert!(error.contains("config.toml cannot be read"), "{error}");
        scratch.write("ws2/.cargo/config.toml", "[source");
        assert!(config_file_problems(&scratch.0.join("ws2"), &scratch.0.join("home")).is_err());
    }

    #[test]
    fn environment_variables_that_move_package_code_are_rejected() {
        let names = [
            "CARGO_SOURCE_CRATES_IO_REPLACE_WITH",
            "CARGO_PATCH_CRATES_IO_SYN_PATH",
            "CARGO_PATHS",
            "CARGO_INCLUDE",
            "CARGO_REGISTRY_INDEX",
            "CARGO_REGISTRIES_MINE_INDEX",
            "CARGO_UNSTABLE_ADVANCED_ENV",
            "__CARGO_TEST_CRATES_IO_URL_DO_NOT_USE_THIS",
            "CARGO_HOME",
            "CARGO_TERM_COLOR",
            "CARGO_REGISTRIES_CRATES_IO_PROTOCOL",
            "CARGO_SOURCE",
            "PATH",
        ];
        let found = env_problems(names.into_iter());
        let named: Vec<&str> = found
            .iter()
            .map(|line| line.split(' ').nth(3).unwrap().trim_end_matches(','))
            .collect();
        assert_eq!(
            named,
            [
                "CARGO_INCLUDE",
                "CARGO_PATCH_CRATES_IO_SYN_PATH",
                "CARGO_PATHS",
                "CARGO_REGISTRIES_MINE_INDEX",
                "CARGO_REGISTRY_INDEX",
                "CARGO_SOURCE_CRATES_IO_REPLACE_WITH",
                "CARGO_UNSTABLE_ADVANCED_ENV",
                "__CARGO_TEST_CRATES_IO_URL_DO_NOT_USE_THIS",
            ]
        );
        assert!(found.iter().all(|line| {
            line.ends_with("which can change where cargo takes package code from (D53).")
        }));
    }
}
