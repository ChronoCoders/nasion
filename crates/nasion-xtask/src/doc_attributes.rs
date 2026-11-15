//! Keeps doctest attributes inside the crate root (D48). rustdoc adds the
//! attributes of a `doc(test(attr(...)))` to doctests, at a crate root and
//! also on a module or item, and through `cfg_attr`, so an `allow` there lifts
//! the `deny(warnings)` the root sets. Two token rules hold over every `.rs`
//! file under the workspace root, read from disk, tracked or not:
//!
//! - An ident `doc` followed by a parenthesized group fails anywhere, except a
//!   top-level `#![doc(test(attr(...)))]` in the source file of a library
//!   target, whose attr list may hold only `forbid(unsafe_code)` and
//!   `deny(warnings)`.
//! - An attribute whose content holds a `$` token fails, so a macro cannot
//!   assemble `doc(test(...))` from pieces.
//!
//!
//! rustc reads a file that is not `.rs` only through `#[path = "..."]` or
//! `include!`, so three more rules close those ways in (D49):
//!
//! - An attribute whose content holds the ident `path` followed by `=` fails.
//! - The ident `include` followed by `!` fails anywhere, whatever path leads
//!   to it.
//! - A use item that names `include` fails, so `include!` cannot be rebound.
//!
//! `include_str!` and `include_bytes!` also make rustc read a file that is not
//! `.rs`, so both rules cover them the same way (D51).
//!
//! A macro could still be handed `include` and call it as `$name!`, so a
//! `$` followed by an ident and `!` fails too (D50). With these rules every
//! workspace file rustc reads is a `.rs` file found by mod resolution, which
//! the walk covers. The `dep_info` module checks that against rustc's own
//! record of the files it read, which also sees what a token scan cannot.
//!
//! The files are tokenized, so comments and string literals never count, and
//! every group is scanned at any depth, macro bodies included. `#[doc = "..."]`
//! and doc comments are not affected.

use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, Group, Spacing, TokenStream, TokenTree};
use syn::ext::IdentExt;

/// Directories under the workspace root that hold no workspace source.
pub const SKIPPED_DIRS: [&str; 3] = ["target", ".venv", ".git"];
/// Macros that make rustc read a file, each with the decision that bans it:
/// `include` (D49), `include_str` and `include_bytes` (D51).
const READING_MACROS: [(&str, &str); 3] = [
    ("include", "D49"),
    ("include_str", "D51"),
    ("include_bytes", "D51"),
];
/// The `level(lint)` entries a crate-root `doc(test(attr(...)))` may hold.
const ROOT_ENTRIES: [(&str, &str); 2] = [("forbid", "unsafe_code"), ("deny", "warnings")];

/// One line per D48, D49 or D50 problem in the `.rs` files under `root`. `library_roots`
/// are the source files of the targets whose kinds include a library kind.
pub fn check(root: &Path, library_roots: &[PathBuf]) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort();
    let mut problems = Vec::new();
    for path in files {
        let code = fs::read_to_string(&path)
            .map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
        let shown = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let library_root = library_roots.iter().any(|lib| lib == &path);
        problems.extend(
            scan(&code, library_root)
                .into_iter()
                .map(|problem| match problem.line {
                    Some(line) => {
                        format!("{shown}:{line} {} ({}).", problem.text, problem.decision)
                    }
                    None => format!("{shown} {} ({}).", problem.text, problem.decision),
                }),
        );
    }
    Ok(problems)
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("{} cannot be listed: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{} cannot be listed: {e}", dir.display()))?;
        let path = entry.path();
        let kind =
            fs::metadata(&path).map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
        if kind.is_dir() {
            let skipped = dir == root && SKIPPED_DIRS.iter().any(|name| entry.file_name() == *name);
            if !skipped {
                if entry
                    .file_type()
                    .map_err(|e| format!("{} cannot be read: {e}", path.display()))?
                    .is_symlink()
                {
                    return Err(format!(
                        "{} is a symbolic link to a directory, which the D48 scan does not follow. Replace it with the directory itself.",
                        path.display()
                    ));
                }
                walk(root, &path, files)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

struct Problem {
    line: Option<usize>,
    text: String,
    decision: &'static str,
}

/// The D48, D49 and D50 problems in one file's code. `library_root` allows the crate-root
/// form at the top level.
fn scan(code: &str, library_root: bool) -> Vec<Problem> {
    match code.parse::<TokenStream>() {
        Ok(stream) => {
            let mut problems = Vec::new();
            scan_stream(stream, library_root, &mut problems);
            problems
        }
        Err(e) => vec![Problem {
            line: None,
            text: format!("does not tokenize, so its attributes cannot be checked: {e}"),
            decision: "D48",
        }],
    }
}

fn scan_stream(stream: TokenStream, root_level: bool, problems: &mut Vec<Problem>) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if let Some((bracket, inner)) = attribute_at(&tokens, index) {
            if holds_dollar(bracket.stream()) {
                problems.push(Problem {
                    line: Some(token.span().start().line),
                    text: format!(
                        "has an attribute holding a $ token, {}, which no attribute may hold",
                        render(&tokens[index..=index + 1 + usize::from(inner)])
                    ),
                    decision: "D48",
                });
            }
            if sets_path(bracket.stream()) {
                problems.push(Problem {
                    line: Some(token.span().start().line),
                    text: format!(
                        "has an attribute that sets path, {}, which makes rustc read a file the scan does not",
                        render(&tokens[index..=index + 1 + usize::from(inner)])
                    ),
                    decision: "D49",
                });
            }
            if root_level
                && inner
                && let Some(content) = root_doc_content(bracket)
            {
                if !root_form_allowed(&content) {
                    problems.push(Problem {
                        line: Some(token.span().start().line),
                        text: format!(
                            "has #![doc ({content})] at a library root, where doc(test(attr(...))) may hold only forbid(unsafe_code) and deny(warnings)"
                        ),
                        decision: "D48",
                    });
                }
                index += 3;
                continue;
            }
        }
        match token {
            TokenTree::Ident(ident) if ident.unraw() == "doc" => {
                if let Some(TokenTree::Group(group)) = tokens.get(index + 1)
                    && group.delimiter() == Delimiter::Parenthesis
                {
                    problems.push(Problem {
                        line: Some(ident.span().start().line),
                        text: format!(
                            "uses doc with a list, {}, which only the crate-root #![doc(test(attr(...)))] of a library target may",
                            render(&tokens[index..=index + 1])
                        ),
                        decision: "D48",
                    });
                }
            }
            TokenTree::Ident(ident) if let Some((name, decision)) = reading_macro(ident) => {
                if is_macro_bang(&tokens, index + 1) {
                    let end = if matches!(tokens.get(index + 2), Some(TokenTree::Group(_))) {
                        index + 2
                    } else {
                        index + 1
                    };
                    problems.push(Problem {
                        line: Some(ident.span().start().line),
                        text: format!(
                            "calls {name}!, {}, which makes rustc read a file the scan does not",
                            render(&tokens[index..=end])
                        ),
                        decision,
                    });
                }
            }
            TokenTree::Ident(ident)
                if ident.unraw() == "use" && !is_punct(tokens.get(index + 1), '<') =>
            {
                let end = tokens[index..]
                    .iter()
                    .position(|token| is_punct(Some(token), ';'))
                    .map_or(tokens.len(), |offset| index + offset);
                for (name, decision) in READING_MACROS {
                    if names(&tokens[index + 1..end], name) {
                        problems.push(Problem {
                            line: Some(ident.span().start().line),
                            text: format!(
                                "names {name} in use {}, which could rebind {name}! to another name",
                                render(&tokens[index + 1..end])
                            ),
                            decision,
                        });
                    }
                }
            }
            TokenTree::Punct(dollar) if dollar.as_char() == '$' => {
                if let Some(TokenTree::Ident(_)) = tokens.get(index + 1)
                    && is_macro_bang(&tokens, index + 2)
                {
                    let end = if matches!(tokens.get(index + 3), Some(TokenTree::Group(_))) {
                        index + 3
                    } else {
                        index + 2
                    };
                    problems.push(Problem {
                        line: Some(dollar.span().start().line),
                        text: format!(
                            "calls a macro passed in as {}, which can be handed include! or any other macro",
                            render(&tokens[index..=end])
                        ),
                        decision: "D50",
                    });
                }
            }
            TokenTree::Group(group) => scan_stream(group.stream(), false, problems),
            _ => {}
        }
        index += 1;
    }
}

/// The bracket group of an attribute starting at `index`, and whether it is an
/// inner attribute.
fn attribute_at(tokens: &[TokenTree], index: usize) -> Option<(&Group, bool)> {
    if !is_punct(tokens.get(index), '#') {
        return None;
    }
    let inner = is_punct(tokens.get(index + 1), '!');
    match tokens.get(index + 1 + usize::from(inner)) {
        Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Bracket => {
            Some((group, inner))
        }
        _ => None,
    }
}

fn is_punct(token: Option<&TokenTree>, ch: char) -> bool {
    matches!(token, Some(TokenTree::Punct(punct)) if punct.as_char() == ch)
}

/// True when the token at `index` is the `!` of a macro call, not of `!=`.
fn is_macro_bang(tokens: &[TokenTree], index: usize) -> bool {
    match tokens.get(index) {
        Some(TokenTree::Punct(bang)) if bang.as_char() == '!' => {
            bang.spacing() == Spacing::Alone || !is_punct(tokens.get(index + 1), '=')
        }
        _ => false,
    }
}

/// True when the ident `path` is followed by `=` at any depth.
fn sets_path(stream: TokenStream) -> bool {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    tokens.iter().enumerate().any(|(index, token)| match token {
        TokenTree::Ident(ident) => ident.unraw() == "path" && is_punct(tokens.get(index + 1), '='),
        TokenTree::Group(group) => sets_path(group.stream()),
        _ => false,
    })
}

/// The entry of `READING_MACROS` an ident names, if any.
fn reading_macro(ident: &proc_macro2::Ident) -> Option<(&'static str, &'static str)> {
    let bare = ident.unraw();
    READING_MACROS.into_iter().find(|(name, _)| bare == name)
}

/// True when a use tree holds the ident `name` at any depth.
fn names(tree: &[TokenTree], name: &str) -> bool {
    tree.iter().any(|token| match token {
        TokenTree::Ident(ident) => ident.unraw() == name,
        TokenTree::Group(group) => names(&group.stream().into_iter().collect::<Vec<_>>(), name),
        _ => false,
    })
}

fn holds_dollar(stream: TokenStream) -> bool {
    stream.into_iter().any(|token| match token {
        TokenTree::Punct(punct) => punct.as_char() == '$',
        TokenTree::Group(group) => holds_dollar(group.stream()),
        _ => false,
    })
}

/// The content of `doc(...)` when the bracket holds exactly `doc` and one
/// parenthesized group.
fn root_doc_content(bracket: &Group) -> Option<TokenStream> {
    let tokens: Vec<TokenTree> = bracket.stream().into_iter().collect();
    match tokens.as_slice() {
        [TokenTree::Ident(ident), TokenTree::Group(group)]
            if ident.unraw() == "doc" && group.delimiter() == Delimiter::Parenthesis =>
        {
            Some(group.stream())
        }
        _ => None,
    }
}

/// True when the content is exactly `test(attr(...))` and every entry of the
/// attr list is one of `ROOT_ENTRIES`, with an optional trailing comma.
fn root_form_allowed(content: &TokenStream) -> bool {
    let Some(test) = single_call(content, "test") else {
        return false;
    };
    let Some(attr) = single_call(&test, "attr") else {
        return false;
    };
    let tokens: Vec<TokenTree> = attr.into_iter().collect();
    let entries: Vec<&[TokenTree]> = tokens.split(|token| is_punct(Some(token), ',')).collect();
    let last = entries.len() - 1;
    entries
        .iter()
        .enumerate()
        .all(|(position, entry)| match entry {
            [] => position > 0 && position == last,
            [TokenTree::Ident(level), TokenTree::Group(group)]
                if group.delimiter() == Delimiter::Parenthesis =>
            {
                let lint: Vec<TokenTree> = group.stream().into_iter().collect();
                matches!(lint.as_slice(), [TokenTree::Ident(lint)]
                    if ROOT_ENTRIES.iter().any(|(l, n)| level == l && lint == n))
            }
            _ => false,
        })
}

/// The content of `name(...)` when the stream holds exactly that.
fn single_call(stream: &TokenStream, name: &str) -> Option<TokenStream> {
    let tokens: Vec<TokenTree> = stream.clone().into_iter().collect();
    match tokens.as_slice() {
        [TokenTree::Ident(ident), TokenTree::Group(group)]
            if ident == name && group.delimiter() == Delimiter::Parenthesis =>
        {
            Some(group.stream())
        }
        _ => None,
    }
}

fn render(tokens: &[TokenTree]) -> String {
    tokens.iter().cloned().collect::<TokenStream>().to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{check, scan};

    const ROOT_FORM: &str = "#![doc(test(attr(forbid(unsafe_code), deny(warnings))))]";

    fn texts(code: &str, library_root: bool) -> Vec<String> {
        scan(code, library_root)
            .into_iter()
            .map(|problem| format!("{:?} {} ({})", problem.line, problem.text, problem.decision))
            .collect()
    }

    fn rejected_with(code: &str, library_root: bool, text: &str) {
        let found = texts(code, library_root);
        assert!(
            found.iter().any(|problem| problem.contains(text)),
            "{code}: expected a problem containing {text:?}, got {found:?}"
        );
    }

    fn accepted(code: &str, library_root: bool) {
        assert!(
            texts(code, library_root).is_empty(),
            "{code}: {:?}",
            texts(code, library_root)
        );
    }

    #[test]
    fn the_root_form_is_accepted_at_a_library_root() {
        for code in [
            ROOT_FORM,
            "#![doc(test(attr(deny(warnings), forbid(unsafe_code))))]",
            "#![doc(test(attr(forbid(unsafe_code), deny(warnings),)))]",
            "#![doc(test(attr(forbid(unsafe_code))))]\n#![doc(test(attr(deny(warnings))))]",
            "#![doc(test(attr(deny(warnings))))]\n#![doc(test(attr(forbid(unsafe_code))))]",
            "#![deny(unsafe_code)]\n#![doc(test(attr(forbid(unsafe_code), deny(warnings))))]\nfn f() {}",
        ] {
            accepted(code, true);
        }
    }

    #[test]
    fn the_root_form_with_anything_else_is_rejected() {
        let extra =
            "where doc(test(attr(...))) may hold only forbid(unsafe_code) and deny(warnings)";
        for code in [
            "#![doc(test(attr(forbid(unsafe_code), deny(warnings), allow(unused))))]",
            "#![doc(test(attr(deny(unsafe_code), deny(warnings))))]",
            "#![doc(test(attr(forbid(unsafe_code), forbid(warnings))))]",
            "#![doc(test(attr(forbid(unsafe_code), warn(warnings))))]",
            "#![doc(test(no_crate_inject, attr(forbid(unsafe_code), deny(warnings))))]",
            "#![doc(test(attr(forbid(unsafe_code), deny(warnings)), no_crate_inject))]",
            "#![doc(test(attr(forbid(unsafe_code), deny(warnings))), html_root_url = \"x\")]",
            "#![doc(html_root_url = \"x\")]",
            "#![doc(test(attr(forbid(unsafe_code), deny(warnings, unused))))]",
            "#![doc(test(attr(forbid(unsafe_code), deny(warnings) x)))]",
            "#![doc(test(attr(forbid(unsafe_code), deny(\"warnings\"))))]",
            "#![doc(test(attr(forbid(unsafe_code), deny[warnings])))]",
            "#![doc(test(attr(, forbid(unsafe_code))))]",
            "#![doc(test(attr(forbid(unsafe_code),, deny(warnings))))]",
            "#![doc(test(attr()))]",
            "#![doc(test(attr[forbid(unsafe_code)]))]",
            "#![doc(test(cfg_attr(all(), attr(forbid(unsafe_code)))))]",
            "#![doc(tests(attr(forbid(unsafe_code))))]",
        ] {
            rejected_with(code, true, extra);
        }
    }

    #[test]
    fn the_root_form_is_rejected_anywhere_else() {
        let list = "uses doc with a list";
        rejected_with(ROOT_FORM, false, list);
        for code in [
            "#![cfg_attr(all(), doc(test(attr(allow(unused)))))]",
            "#![cfg_attr(all(), doc(test(attr(forbid(unsafe_code), deny(warnings)))))]",
            "#[doc(test(attr(allow(unused))))]\nfn f() {}",
            "#[doc(test(attr(allow(unused))))]\nstruct S;",
            "#[doc(test(attr(allow(unused))))]\nimpl S {}",
            "#[doc(test(attr(allow(unused))))]\npub mod m {}",
            "pub mod m {\n    #![doc(test(attr(forbid(unsafe_code), deny(warnings))))]\n}",
            "macro_rules! m { () => { #[doc(test(attr(allow(unused))))] pub mod g {} }; }",
            "#[doc(alias = \"x\")]\nfn f() {}",
            "#[doc(hidden)]\nfn f() {}",
            "#[r#doc(hidden)]\nfn f() {}",
            "fn f() { doc(1); }",
        ] {
            rejected_with(code, true, list);
        }
        let found = texts("\n#[doc(test(attr(allow(unused))))]\npub mod m {}", false);
        assert_eq!(
            found,
            [
                "Some(2) uses doc with a list, doc (test (attr (allow (unused)))), which only the crate-root #![doc(test(attr(...)))] of a library target may (D48)"
            ]
        );
    }

    #[test]
    fn attributes_holding_a_dollar_token_are_rejected() {
        let dollar = "has an attribute holding a $ token";
        for code in [
            "macro_rules! m { ($m:meta) => { #[$m] fn f() {} }; }",
            "macro_rules! m { ($d:ident) => { #[$d(x)] fn f() {} }; }",
            "macro_rules! m { ($x:meta) => { #![cfg_attr(all(), $x)] }; }",
            "macro_rules! m { ($t:ident) => { #[doc($t(attr(allow(unused))))] pub mod g {} }; }",
            "macro_rules! m { ($l:ident) => { #[cfg_attr(all(), deny(x, y(z($l))))] fn f() {} }; }",
            "macro_rules! m { ($l:ident) => { #[$d($t(attr(allow(unused))))] fn f() {} }; }",
        ] {
            rejected_with(code, false, dollar);
        }
        assert_eq!(
            texts("macro_rules! m { ($m:meta) => { #![$m] }; }", true),
            [
                "Some(1) has an attribute holding a $ token, #! [$ m], which no attribute may hold (D48)"
            ]
        );
    }

    #[test]
    fn plain_doc_text_and_dollars_outside_attributes_are_accepted() {
        for code in [
            "#[doc = \"x\"]\nfn f() {}",
            "#![doc = \"x\"]",
            "/// doc(test(attr(allow(unused))))\nfn f() {}",
            "//! doc(test(attr(allow(unused))))",
            "/** block */\nfn f() {}",
            "const S: &str = \"#[doc(test(attr(allow(unused))))]\";",
            "const S: &str = r#\"#![doc(test(attr(allow(unused))))]\"#;",
            "// #[doc(test(attr(allow(unused))))]\nfn f() {}",
            "/* #[$m] */\nfn f() {}",
            "macro_rules! m { ($($x:ident),*) => { $(let $x = 1;)* }; }",
            "macro_rules! m { () => { # $x }; }",
            "let doc = 1; let x = doc;",
            "let x = a # [b];",
        ] {
            accepted(code, false);
            accepted(code, true);
        }
    }

    #[test]
    fn attributes_that_set_path_are_rejected() {
        for code in [
            "#[path = \"x\"]\nmod m;",
            "#![path = \"x\"]",
            "#[cfg_attr(all(), path = \"x\")]\nmod m;",
            "#[cfg_attr(all(), cfg_attr(all(), path = \"x\"))]\nmod m;",
            "#[r#path = \"x\"]\nmod m;",
            "mod outer {\n    #[path = \"x\"]\n    mod m;\n}",
            "macro_rules! m { () => { #[path = \"x\"] mod m; }; }",
        ] {
            rejected_with(code, false, "has an attribute that sets path");
            rejected_with(code, false, "(D49)");
        }
        assert_eq!(
            texts("\n#[path = \"m.txt\"]\npub mod m;", false),
            [
                "Some(2) has an attribute that sets path, # [path = \"m.txt\"], which makes rustc read a file the scan does not (D49)"
            ]
        );
    }

    #[test]
    fn include_calls_are_rejected_by_any_path() {
        for code in [
            "include!(\"x\");",
            "std::include!(\"x\");",
            "core::include!(\"x\");",
            "::core::include!(\"x\");",
            "r#include!(\"x\");",
            "include! { \"x\" }",
            "include![\"x\"];",
            "fn f() { let v = vec![include!(\"x\")]; }",
            "macro_rules! m { () => { include!(\"x\"); }; }",
            "macro_rules! m { ($f:literal) => { include!($f) }; }",
            "include!",
            "include!;",
        ] {
            rejected_with(code, false, "calls include!");
            rejected_with(code, false, "(D49)");
        }
        assert_eq!(
            texts("\ninclude!(\"inc.in\");", false),
            [
                "Some(2) calls include!, include ! (\"inc.in\"), which makes rustc read a file the scan does not (D49)"
            ]
        );
        assert_eq!(
            texts("include!", false),
            [
                "Some(1) calls include!, include !, which makes rustc read a file the scan does not (D49)"
            ]
        );
    }

    #[test]
    fn use_items_naming_include_are_rejected() {
        for code in [
            "use std::include as inc;",
            "use std::{include};",
            "use std::{fmt, macros::{r#include as i}};",
            "pub use core::include;",
            "use core::include",
            "mod m {\n    use std::include as inc;\n}",
        ] {
            rejected_with(code, false, "names include in use");
            rejected_with(code, false, "(D49)");
        }
        assert_eq!(
            texts("use std::include as inc;", false),
            [
                "Some(1) names include in use std :: include as inc, which could rebind include! to another name (D49)"
            ]
        );
    }

    #[test]
    fn path_and_include_outside_those_forms_are_accepted() {
        for code in [
            "let path = 1;",
            "fn f(path: u8) {}",
            "let s = S { path: 1 };",
            "#[doc = \"path = x\"]\nfn f() {}",
            "#[cfg(feature = \"path\")]\nfn f() {}",
            "#[cfg(path)]\nfn f() {}",
            "let s = \"include!(x) use std::include as inc; #[path = x]\";",
            "// include!(\"x\"); #[path = \"x\"]\nfn f() {}",
            "use std::path::Path;",
            "use std::{path::{Path, PathBuf}, fs};",
            "fn f<'a>(x: &'a u8) -> impl Sized + use<'a> { x }",
            "let include = 1; let same = include != 2;",
            "let include = true; let other = !include;",
            "let s = \"include_str!(x) include_bytes!(x) use std::include_str;\";",
            "// include_str!(\"x\"); include_bytes!(\"x\");\nfn f() {}",
            "/* use std::include_bytes as b; */\nfn f() {}",
            "let include_str = 1; let same = include_str != 2;",
            "let include_bytes = 1; let same = include_bytes != 2;",
            "use std::include_strings;",
            "fn include_str() {}",
        ] {
            accepted(code, false);
        }
    }

    #[test]
    fn include_str_and_include_bytes_are_rejected_like_include() {
        for name in ["include_str", "include_bytes"] {
            for code in [
                format!("let s = {name}!(\"x\");"),
                format!("let s = {name}![\"x\"];"),
                format!("let s = {name}! {{ \"x\" }};"),
                format!("let s = std::{name}!(\"x\");"),
                format!("let s = core::{name}!(\"x\");"),
                format!("let s = r#{name}!(\"x\");"),
                format!("macro_rules! m {{ () => {{ {name}!(\"x\") }}; }}"),
                format!("let v = vec![{name}!(\"x\")];"),
            ] {
                let found = texts(&code, false);
                assert_eq!(found.len(), 1, "{code}: {found:?}");
                assert!(
                    found[0].contains(&format!("calls {name}!, "))
                        && found[0]
                            .ends_with("which makes rustc read a file the scan does not (D51)"),
                    "{code}: {found:?}"
                );
            }
        }
        for (code, name) in [
            ("use std::include_str;", "include_str"),
            ("use std::include_bytes as b;", "include_bytes"),
            ("use std::{include_bytes};", "include_bytes"),
            ("use std::{fmt, r#include_str as s};", "include_str"),
        ] {
            let found = texts(code, false);
            assert_eq!(found.len(), 1, "{code}: {found:?}");
            assert!(
                found[0].contains(&format!("names {name} in use "))
                    && found[0]
                        .ends_with(&format!("which could rebind {name}! to another name (D51)")),
                "{code}: {found:?}"
            );
        }
        assert_eq!(
            texts("\nconst NOTE: &str = include_str!(\"note.txt\");", false),
            [
                "Some(2) calls include_str!, include_str ! (\"note.txt\"), which makes rustc read a file the scan does not (D51)"
            ]
        );
        assert_eq!(
            texts("use std::{include, include_bytes};", false),
            [
                "Some(1) names include in use std :: { include , include_bytes }, which could rebind include! to another name (D49)",
                "Some(1) names include_bytes in use std :: { include , include_bytes }, which could rebind include_bytes! to another name (D51)",
            ]
        );
    }

    #[test]
    fn calling_a_macro_passed_in_is_rejected() {
        for code in [
            "macro_rules! m { ($i:ident) => { $i!(x) }; }",
            "macro_rules! m { ($i:ident) => { $i![x] }; }",
            "macro_rules! m { ($i:ident) => { $i!{x} }; }",
            "macro_rules! m { ($i:ident) => { $r#i!(x) }; }",
            "macro_rules! m { ($i:ident) => { $i!; }; }",
            "macro_rules! outer { () => { macro_rules! inner { ($i:ident) => { { $i!(x) } }; } }; }",
            "macro_rules! m { ($i:ident) => { fn f() { let v = vec![$i!(x)]; } }; }",
        ] {
            rejected_with(code, false, "calls a macro passed in as");
            rejected_with(code, false, "(D50)");
        }
        assert_eq!(
            texts(
                "macro_rules! m {\n    ($i:ident) => { $i!(\"inc.in\"); };\n}",
                false
            ),
            [
                "Some(2) calls a macro passed in as $ i ! (\"inc.in\"), which can be handed include! or any other macro (D50)"
            ]
        );
    }

    #[test]
    fn dollars_that_do_not_call_a_macro_are_accepted() {
        for code in [
            "macro_rules! m { () => { $crate::m!() }; }",
            "macro_rules! m { ($i:ident) => { let $i = 1; }; }",
            "macro_rules! m { ($i:ident) => { $i != 1 }; }",
            "macro_rules! m { ($($x:tt)*) => { $($x)* }; }",
            "macro_rules! m { ($i:ident) => { fn $i() {} }; }",
            "macro_rules! m { () => { other!() }; }",
            "m!();",
            "let s = \"$i!(x)\";",
        ] {
            accepted(code, false);
        }
    }

    #[test]
    fn code_that_does_not_tokenize_is_a_problem_without_a_line() {
        let found = texts("let s = \"open;", false);
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("None does not tokenize"), "{found:?}");
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("nasion-d48-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, path: &str, text: &str) -> PathBuf {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const BAD: &str = "#[doc(test(attr(allow(unused))))]\npub mod m {}\n";

    #[test]
    fn the_walk_reads_every_rs_file_except_the_skipped_root_dirs() {
        let scratch = Scratch::new("walk");
        let lib = scratch.write("crates/a/src/lib.rs", &format!("{ROOT_FORM}\n"));
        scratch.write("crates/a/src/untracked.rs", BAD);
        scratch.write("crates/a/target/nested.rs", BAD);
        scratch.write("crates/a/src/notes.txt", BAD);
        scratch.write("target/debug/build.rs", BAD);
        scratch.write(".venv/lib/x.rs", BAD);
        scratch.write(".git/x.rs", BAD);
        let found = check(&scratch.0, &[lib]).unwrap();
        let files: Vec<&str> = found
            .iter()
            .map(|line| line.split(':').next().unwrap())
            .collect();
        assert_eq!(
            files,
            ["crates/a/src/untracked.rs", "crates/a/target/nested.rs"]
        );
        assert!(
            found.iter().all(|line| line.ends_with(" (D48).")),
            "{found:?}"
        );
    }

    #[test]
    fn the_root_form_counts_only_at_a_listed_library_root() {
        let scratch = Scratch::new("roots");
        let lib = scratch.write("src/lib.rs", ROOT_FORM);
        let bin = scratch.write("src/main.rs", ROOT_FORM);
        let found = check(&scratch.0, std::slice::from_ref(&lib)).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("src/main.rs:1 uses doc with a list"),
            "{found:?}"
        );
        assert!(check(&scratch.0, &[lib, bin]).unwrap().is_empty());
    }

    #[test]
    fn a_file_that_does_not_tokenize_is_reported_by_name() {
        let scratch = Scratch::new("lex");
        scratch.write("a.rs", "let s = \"open;");
        let found = check(&scratch.0, &[]).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("a.rs does not tokenize"), "{found:?}");
    }

    #[test]
    fn walk_and_read_failures_stop_the_check() {
        assert!(check(Path::new("/nonexistent/nasion-d48"), &[]).is_err());
        let scratch = Scratch::new("fail");
        fs::write(scratch.0.join("bad.rs"), [0xff, 0xfe]).unwrap();
        let read = check(&scratch.0, &[]).unwrap_err();
        assert!(read.contains("bad.rs cannot be read"), "{read}");
    }

    #[cfg(unix)]
    #[test]
    fn broken_links_and_linked_directories_stop_the_check() {
        let scratch = Scratch::new("links");
        std::os::unix::fs::symlink("/nonexistent/nasion-d48", scratch.0.join("broken.rs")).unwrap();
        let broken = check(&scratch.0, &[]).unwrap_err();
        assert!(broken.contains("broken.rs cannot be read"), "{broken}");
        fs::remove_file(scratch.0.join("broken.rs")).unwrap();
        let target = scratch.write("real/x.rs", BAD);
        std::os::unix::fs::symlink(target.parent().unwrap(), scratch.0.join("linked")).unwrap();
        let linked = check(&scratch.0, &[]).unwrap_err();
        assert!(
            linked.contains("linked is a symbolic link to a directory"),
            "{linked}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_to_a_file_is_read() {
        let scratch = Scratch::new("filelink");
        let real = scratch.write("real.txt", BAD);
        std::os::unix::fs::symlink(&real, scratch.0.join("link.rs")).unwrap();
        let found = check(&scratch.0, &[]).unwrap();
        assert_eq!(found.len(), 1);
        assert!(
            found[0].starts_with("link.rs:1 uses doc with a list"),
            "{found:?}"
        );
    }
}
