//! Rejects lint-lowering attributes in doctests (D44). A doctest's own
//! `allow`, `expect` or `warn` lifts the `deny(warnings)` that the crate root
//! sets for doctests, and no lint level at the root can stop that. A macro
//! can emit such an attribute after expansion, where a token scan never sees
//! it, so every compiled doctest is also held to closed allowlists of macros,
//! attributes and use items (D46). A use item may not bind a name on the
//! derive allowlist either, because an import can rebind any derive macro to
//! one of those names (D47).
//!
//! The doctests come from rustdoc's own list, never from walking source files.
//! rustdoc reports each one at the line of the item's first doc fragment plus
//! the offset of the opening fence in the joined doc text, so the code blocks
//! are rebuilt the same way from each item's literal doc attributes.

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use proc_macro2::{Delimiter, Ident, LexError, TokenStream, TokenTree};
use syn::ext::IdentExt;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ExprLit, Lit, Meta};

/// Attributes that lower a lint level below what the crate root sets (D44).
const LOWERING: [&str; 3] = ["allow", "expect", "warn"];

/// The only macros a doctest may call, by bare name (D46). Any other macro,
/// and these through a path, could expand to a lint attribute the scan never
/// sees.
const MACROS: [&str; 13] = [
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "format",
    "vec",
    "matches",
    "panic",
    "unreachable",
    "write",
    "writeln",
];

/// The only derives a doctest may use, by bare name (D46).
const DERIVES: [&str; 9] = [
    "Debug",
    "Clone",
    "Copy",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "Hash",
    "Default",
];

struct Listed {
    file: String,
    item: String,
    line: usize,
}

struct CodeBlock {
    line: usize,
    info: String,
    code: String,
}

impl CodeBlock {
    /// rustdoc lists `ignore` blocks but never compiles them.
    fn ignored(&self) -> bool {
        self.info
            .split([',', ' ', '\t'])
            .any(|word| word == "ignore" || word.starts_with("ignore-"))
    }
}

/// Returns the number of doctests rustdoc lists and one line per problem.
pub fn check(
    cargo: &OsStr,
    root: &Path,
    packages: &[&str],
    log: &mut dyn std::io::Write,
) -> Result<(usize, Vec<String>), String> {
    let listed = list(cargo, root, packages, log)?;
    let mut parsed: HashMap<PathBuf, Vec<CodeBlock>> = HashMap::new();
    let mut problems = Vec::new();
    for doctest in &listed {
        let path = root.join(&doctest.file);
        let at = format!(
            "{}:{} (doctest \"{}\")",
            doctest.file, doctest.line, doctest.item
        );
        let blocks = match parsed.entry(path) {
            Entry::Occupied(known) => known.into_mut(),
            Entry::Vacant(new) => {
                let blocks = blocks_in_file(new.key())?;
                new.insert(blocks)
            }
        };
        let found: Vec<&CodeBlock> = blocks.iter().filter(|b| b.line == doctest.line).collect();
        let block = match found.as_slice() {
            [] => {
                problems.push(format!(
                    "{at} is not in a literal doc comment or #[doc = \"...\"] attribute, so its code cannot be read. It may come from include_str!, concat! or a macro. Write it as a literal doc comment (D44)."
                ));
                continue;
            }
            [block] => block,
            _ => {
                return Err(format!(
                    "{at}: more than one doc code block maps to this line. Check the doc comments in {}.",
                    doctest.file
                ));
            }
        };
        if block.ignored() {
            continue;
        }
        match scan(&block.code) {
            Ok(found) => {
                problems.extend(found.into_iter().map(|problem| format!("{at} {problem}")))
            }
            Err(e) => problems.push(format!("{at} does not tokenize: {e}")),
        }
    }
    Ok((listed.len(), problems))
}

/// Lists the doctests of `packages`, the packages whose library target
/// rustdoc can doc-test. Naming them, not passing --workspace, keeps cargo from
/// warning about a cdylib it cannot doc-test.
fn list(
    cargo: &OsStr,
    root: &Path,
    packages: &[&str],
    log: &mut dyn std::io::Write,
) -> Result<Vec<Listed>, String> {
    if packages.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["test", "--locked", "--all-features", "--doc"];
    for package in packages {
        args.extend(["-p", package]);
    }
    args.extend(["--", "--list"]);
    let output = crate::cargo_cmd::run(
        Command::new(cargo).current_dir(root).args(&args),
        &format!("cargo {}", args.join(" ")),
        log,
    )?;
    if !output.status.success() {
        return Err(format!(
            "cargo test --doc -- --list failed ({}). Check that the workspace and its doctests build. Its stderr is above.",
            output.status
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("cargo test --doc -- --list printed output that is not UTF-8: {e}"))?;
    let mut listed = Vec::new();
    for line in stdout.lines() {
        if let Some(doctest) = parse_list_line(line)? {
            listed.push(doctest);
        }
    }
    Ok(listed)
}

/// Parses one line of the list. Returns None for blank and summary lines and
/// an error for any line in another format, so a change in rustdoc's output
/// stops the check instead of hiding doctests.
fn parse_list_line(line: &str) -> Result<Option<Listed>, String> {
    if line.trim().is_empty() || is_summary(line) {
        return Ok(None);
    }
    let unknown = || {
        format!(
            "cargo test --doc -- --list printed a line in an unknown format: {line:?}. Check the rustdoc output of the pinned toolchain."
        )
    };
    let rest = line.strip_suffix("): test").ok_or_else(unknown)?;
    let (head, number) = rest.rsplit_once("(line ").ok_or_else(unknown)?;
    let line_number = number.parse::<usize>().map_err(|_| unknown())?;
    let (file, item) = head.split_once(" - ").ok_or_else(unknown)?;
    if file.is_empty() {
        return Err(unknown());
    }
    Ok(Some(Listed {
        file: file.to_owned(),
        item: item.trim().to_owned(),
        line: line_number,
    }))
}

/// "8 tests, 0 benchmarks" and "all doctests ran in 0.04s; merged doctests
/// compilation took 0.03s".
fn is_summary(line: &str) -> bool {
    let counts = line.split_once(", ").is_some_and(|(tests, benchmarks)| {
        is_count(tests, "test") && is_count(benchmarks, "benchmark")
    });
    counts || line.starts_with("all doctests ran in ")
}

fn is_count(text: &str, noun: &str) -> bool {
    text.split_once(' ').is_some_and(|(number, word)| {
        number.parse::<u64>().is_ok() && word.strip_suffix('s').unwrap_or(word) == noun
    })
}

fn blocks_in_file(path: &Path) -> Result<Vec<CodeBlock>, String> {
    let source = fs::read_to_string(path)
        .map_err(|e| format!("{}, listed by rustdoc, cannot be read: {e}", path.display()))?;
    if path.extension() != Some(OsStr::new("rs")) {
        return Ok(Vec::new());
    }
    let file = syn::parse_file(&source)
        .map_err(|e| format!("{}, listed by rustdoc, does not parse: {e}", path.display()))?;
    let mut collector = Collector::default();
    collector.group(&file.attrs);
    collector.visit_file(&file);
    Ok(collector.blocks)
}

#[derive(Default)]
struct Collector {
    blocks: Vec<CodeBlock>,
}

impl Collector {
    /// Joins the literal doc attributes of one item the way rustdoc does and
    /// records its fenced code blocks. A group with a doc attribute that is not
    /// a string literal records nothing, because the lines rustdoc reports for
    /// it cannot be rebuilt.
    fn group(&mut self, attrs: &[Attribute]) {
        let mut first_line = None;
        let mut text = String::new();
        for attr in attrs {
            if !attr.path().is_ident("doc") {
                continue;
            }
            let Meta::NameValue(pair) = &attr.meta else {
                continue;
            };
            let Expr::Lit(ExprLit {
                lit: Lit::Str(value),
                ..
            }) = &pair.value
            else {
                return;
            };
            if first_line.is_some() {
                text.push('\n');
            }
            first_line.get_or_insert(attr.pound_token.span.start().line);
            text.push_str(&value.value());
        }
        if let Some(first_line) = first_line {
            self.blocks.extend(fenced_blocks(&text, first_line));
        }
    }
}

macro_rules! doc_groups {
    ($($method:ident: $node:ty),* $(,)?) => {
        impl<'ast> Visit<'ast> for Collector {
            $(
                fn $method(&mut self, node: &'ast $node) {
                    self.group(&node.attrs);
                    visit::$method(self, node);
                }
            )*
        }
    };
}

doc_groups!(
    visit_item_const: syn::ItemConst,
    visit_item_enum: syn::ItemEnum,
    visit_item_extern_crate: syn::ItemExternCrate,
    visit_item_fn: syn::ItemFn,
    visit_item_foreign_mod: syn::ItemForeignMod,
    visit_item_impl: syn::ItemImpl,
    visit_item_macro: syn::ItemMacro,
    visit_item_mod: syn::ItemMod,
    visit_item_static: syn::ItemStatic,
    visit_item_struct: syn::ItemStruct,
    visit_item_trait: syn::ItemTrait,
    visit_item_trait_alias: syn::ItemTraitAlias,
    visit_item_type: syn::ItemType,
    visit_item_union: syn::ItemUnion,
    visit_item_use: syn::ItemUse,
    visit_impl_item_const: syn::ImplItemConst,
    visit_impl_item_fn: syn::ImplItemFn,
    visit_impl_item_macro: syn::ImplItemMacro,
    visit_impl_item_type: syn::ImplItemType,
    visit_trait_item_const: syn::TraitItemConst,
    visit_trait_item_fn: syn::TraitItemFn,
    visit_trait_item_macro: syn::TraitItemMacro,
    visit_trait_item_type: syn::TraitItemType,
    visit_foreign_item_fn: syn::ForeignItemFn,
    visit_foreign_item_macro: syn::ForeignItemMacro,
    visit_foreign_item_static: syn::ForeignItemStatic,
    visit_foreign_item_type: syn::ForeignItemType,
    visit_field: syn::Field,
    visit_variant: syn::Variant,
);

/// The fenced code blocks in joined doc text whose first line is source line
/// `first_line`. An unclosed block runs to the end of the text, as in rustdoc.
fn fenced_blocks(text: &str, first_line: usize) -> Vec<CodeBlock> {
    let mut blocks = Vec::new();
    let mut open: Option<(CodeBlock, String)> = None;
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim_start();
        match open.take() {
            None => {
                if let Some((fence, info)) = opening_fence(trimmed) {
                    let block = CodeBlock {
                        line: first_line + index,
                        info,
                        code: String::new(),
                    };
                    open = Some((block, fence));
                }
            }
            Some((mut block, fence)) => {
                if closes(trimmed, &fence) {
                    blocks.push(block);
                } else {
                    block.code.push_str(raw);
                    block.code.push('\n');
                    open = Some((block, fence));
                }
            }
        }
    }
    blocks.extend(open.map(|(block, _)| block));
    blocks
}

/// The fence and info string of a line that opens a fenced code block.
fn opening_fence(trimmed: &str) -> Option<(String, String)> {
    let mark = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = trimmed.chars().take_while(|c| *c == mark).count();
    if length < 3 {
        return None;
    }
    let (fence, info) = trimmed.split_at(length);
    Some((fence.to_owned(), info.trim().to_owned()))
}

fn closes(trimmed: &str, fence: &str) -> bool {
    let Some(mark) = fence.chars().next() else {
        return false;
    };
    let run = trimmed.chars().take_while(|c| *c == mark).count();
    run >= fence.len() && trimmed[run..].trim().is_empty()
}

/// rustdoc's hidden-line rule, so the scan sees the code rustdoc compiles.
/// Read from `map_line` and `Line::for_code` in src/librustdoc/html/markdown.rs
/// of rust-lang/rust at tag 1.100.0 (commit
/// 0ef1bc386059596dada8fb65b50425aab2ea637a): a line whose trimmed start is
/// `##` has its first `##` replaced by `#`, a trimmed line starting with `# `
/// keeps what follows that prefix, a trimmed line equal to `#` becomes empty,
/// and every other line stays as it is.
fn compiled_line(line: &str) -> Cow<'_, str> {
    let trimmed = line.trim();
    if trimmed.starts_with("##") {
        Cow::Owned(line.replacen("##", "#", 1))
    } else if let Some(rest) = trimmed.strip_prefix("# ") {
        Cow::Borrowed(rest)
    } else if trimmed == "#" {
        Cow::Borrowed("")
    } else {
        Cow::Borrowed(line)
    }
}

fn compiled_code(code: &str) -> String {
    code.lines()
        .map(compiled_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per D44 or D46 problem in a doctest's code. The code is tokenized
/// after hidden lines are restored, so comments and string literals never
/// count and nothing rustdoc compiles is skipped.
fn scan(code: &str) -> Result<Vec<String>, LexError> {
    let stream: TokenStream = compiled_code(code).parse()?;
    let mut problems = Vec::new();
    scan_stream(stream, &mut problems);
    Ok(problems)
}

fn scan_stream(stream: TokenStream, problems: &mut Vec<String>) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut index = 0;
    while index < tokens.len() {
        if let Some((content, inner, next)) = attribute_at(&tokens, index) {
            problems.extend(attribute_problem(content, inner));
            index = next;
            continue;
        }
        match &tokens[index] {
            TokenTree::Ident(ident) if is_punct(tokens.get(index + 1), '!') => {
                if ident.unraw() == "macro_rules" {
                    let name = tokens
                        .get(index + 2)
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    problems.push(format!(
                        "defines a macro with macro_rules! {name}, which doctests may not do (D46)."
                    ));
                } else if matches!(tokens.get(index + 2), Some(TokenTree::Group(_))) {
                    problems.extend(macro_call_problem(&tokens, index, ident));
                }
            }
            TokenTree::Ident(ident) if ident == "use" => {
                let end = (index + 1..tokens.len())
                    .find(|i| is_punct(tokens.get(*i), ';'))
                    .unwrap_or(tokens.len());
                problems.extend(use_problems(&tokens[index + 1..end]));
            }
            TokenTree::Group(group) => scan_stream(group.stream(), problems),
            _ => {}
        }
        index += 1;
    }
}

fn is_punct(token: Option<&TokenTree>, mark: char) -> bool {
    matches!(token, Some(TokenTree::Punct(p)) if p.as_char() == mark)
}

/// An attribute starting at `index`: its content, whether it is inner, and
/// the index after it.
fn attribute_at(tokens: &[TokenTree], index: usize) -> Option<(TokenStream, bool, usize)> {
    if !is_punct(tokens.get(index), '#') {
        return None;
    }
    let inner = is_punct(tokens.get(index + 1), '!');
    let body = index + 1 + usize::from(inner);
    match tokens.get(body) {
        Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Bracket => {
            Some((group.stream(), inner, body + 1))
        }
        _ => None,
    }
}

fn attribute_problem(content: TokenStream, inner: bool) -> Option<String> {
    let text = format!("#{}[{content}]", if inner { "!" } else { "" });
    if lowers(content.clone()) {
        return Some(format!(
            "sets a lint level with {text}. Doctests may not use allow, expect or warn, because they lift deny(warnings) (D44)."
        ));
    }
    if attribute_allowed(content) {
        return None;
    }
    Some(format!(
        "uses attribute {text}, which is not on the doctest attribute allowlist (D46)."
    ))
}

/// True for `derive(...)` of allowlisted derives, `cfg(...)` and
/// `doc = "..."` with a string literal.
fn attribute_allowed(content: TokenStream) -> bool {
    let tokens: Vec<TokenTree> = content.into_iter().collect();
    match tokens.as_slice() {
        [TokenTree::Ident(name), TokenTree::Group(args)]
            if args.delimiter() == Delimiter::Parenthesis =>
        {
            let name = name.unraw();
            name == "cfg" || (name == "derive" && derives_allowed(args.stream()))
        }
        [
            TokenTree::Ident(name),
            TokenTree::Punct(eq),
            TokenTree::Literal(text),
        ] => name.unraw() == "doc" && eq.as_char() == '=' && is_string_literal(&text.to_string()),
        _ => false,
    }
}

/// True when every item is a bare allowlisted derive name, separated by
/// commas, with an optional trailing comma.
fn derives_allowed(args: TokenStream) -> bool {
    let tokens: Vec<TokenTree> = args.into_iter().collect();
    tokens.chunks(2).all(|pair| {
        let name_allowed = matches!(&pair[0], TokenTree::Ident(name) if DERIVES.contains(&name.unraw().to_string().as_str()));
        let separated = pair.get(1).is_none_or(|comma| is_punct(Some(comma), ','));
        name_allowed && separated
    })
}

fn is_string_literal(text: &str) -> bool {
    text.starts_with('"')
        || text
            .strip_prefix('r')
            .is_some_and(|raw| raw.trim_start_matches('#').starts_with('"'))
}

/// A problem for a macro call at `index` that is not a bare allowlisted name.
fn macro_call_problem(tokens: &[TokenTree], index: usize, name: &Ident) -> Option<String> {
    let mut start = index;
    while start >= 2 && is_punct(tokens.get(start - 1), ':') && is_punct(tokens.get(start - 2), ':')
    {
        start -= 2;
        if start >= 1 && matches!(tokens[start - 1], TokenTree::Ident(_)) {
            start -= 1;
        } else {
            break;
        }
    }
    let path: String = tokens[start..=index]
        .iter()
        .map(ToString::to_string)
        .collect();
    let bare_allowed = start == index && MACROS.contains(&name.unraw().to_string().as_str());
    if bare_allowed {
        return None;
    }
    Some(format!(
        "calls macro {path}!, which is not on the doctest macro allowlist (D46)."
    ))
}

/// Problems in the tokens of one use item after `use`. A glob fails (D46), and
/// so does any name on the macro allowlist (D46) or the derive allowlist (D47)
/// anywhere in the tree, so nothing can shadow an allowlisted macro or derive.
/// Tokens that are not a use tree, such as the `use<'a>` of precise capturing,
/// are skipped.
fn use_problems(tree: &[TokenTree]) -> Vec<String> {
    if is_punct(tree.first(), '<') {
        return Vec::new();
    }
    let mut flat = Vec::new();
    flatten_use_tree(tree, &mut flat);
    let text = tree.iter().cloned().collect::<TokenStream>().to_string();
    let mut problems = Vec::new();
    if flat.iter().any(|token| is_punct(Some(token), '*')) {
        problems.push(format!(
            "has a glob import, use {text}, which doctests may not use (D46)."
        ));
    }
    for token in &flat {
        let TokenTree::Ident(name) = token else {
            continue;
        };
        let bare = name.unraw().to_string();
        if MACROS.contains(&bare.as_str()) {
            problems.push(format!(
                "imports or renames {name} in use {text}, a name on the doctest macro allowlist (D46)."
            ));
        }
        if DERIVES.contains(&bare.as_str()) {
            problems.push(format!(
                "imports or renames {name} in use {text}, a name on the doctest derive allowlist (D47)."
            ));
        }
    }
    problems
}

fn flatten_use_tree(tokens: &[TokenTree], flat: &mut Vec<TokenTree>) {
    for token in tokens {
        match token {
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                flatten_use_tree(&inner, flat);
            }
            _ => flat.push(token.clone()),
        }
    }
}

/// True when attribute content holds allow, expect or warn followed by a
/// parenthesized list, at any depth, including inside cfg_attr.
fn lowers(stream: TokenStream) -> bool {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    tokens.iter().enumerate().any(|(index, token)| match token {
        TokenTree::Ident(ident) => {
            LOWERING.contains(&ident.unraw().to_string().as_str())
                && matches!(tokens.get(index + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
        }
        TokenTree::Group(group) => lowers(group.stream()),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn the_doctest_list_run_forwards_cargo_stderr() {
        use crate::cargo_cmd::tests::fake::{FakeCargo, assert_forwarded};
        let fake = FakeCargo::new("test-doc-list");
        for warn in [true, false] {
            let cargo = fake.script(if warn { "warn" } else { "clean" }, "true", warn);
            let mut log = Vec::new();
            assert!(
                super::list(&cargo, &fake.0, &["core"], &mut log)
                    .unwrap()
                    .is_empty()
            );
            assert_forwarded(
                &log,
                "test --locked --all-features --doc -p core -- --list",
                warn,
            );
        }
        let cargo = fake.script("unused", "touch ran", false);
        let mut log = Vec::new();
        assert!(
            super::list(&cargo, &fake.0, &[], &mut log)
                .unwrap()
                .is_empty()
        );
        assert!(log.is_empty() && !fake.0.join("ran").exists());
    }

    use super::{
        DERIVES, MACROS, compiled_code, compiled_line, fenced_blocks, parse_list_line, scan,
    };

    #[test]
    fn list_lines_parse_with_and_without_an_item_name() {
        let doctest = parse_list_line("crates/a/src/lib.rs - Thing::run (line 12): test")
            .unwrap()
            .unwrap();
        assert_eq!(
            (doctest.file.as_str(), doctest.item.as_str(), doctest.line),
            ("crates/a/src/lib.rs", "Thing::run", 12)
        );
        let crate_doc = parse_list_line("crates/a/src/lib.rs - (line 3): test")
            .unwrap()
            .unwrap();
        assert_eq!((crate_doc.item.as_str(), crate_doc.line), ("", 3));
    }

    #[test]
    fn summary_and_blank_lines_are_skipped() {
        for line in [
            "",
            "8 tests, 0 benchmarks",
            "1 test, 0 benchmarks",
            "all doctests ran in 0.04s; merged doctests compilation took 0.03s",
        ] {
            assert!(parse_list_line(line).unwrap().is_none(), "{line}");
        }
    }

    #[test]
    fn lines_in_another_format_are_errors() {
        for line in [
            "running 3 tests",
            "src/lib.rs - f (line x): test",
            "src/lib.rs - f (line 3): bench",
            " - f (line 3): test",
        ] {
            assert!(parse_list_line(line).is_err(), "{line}");
        }
    }

    #[test]
    fn block_lines_count_from_the_first_fragment() {
        let text = "Summary.\n\n```\nlet a = 1;\n```\n\n~~~~ignore\nx\n~~~~";
        let blocks = fenced_blocks(text, 10);
        let found: Vec<(usize, &str, &str)> = blocks
            .iter()
            .map(|b| (b.line, b.info.as_str(), b.code.as_str()))
            .collect();
        assert_eq!(found, [(12, "", "let a = 1;\n"), (16, "ignore", "x\n")]);
        assert!(!blocks[0].ignored());
        assert!(blocks[1].ignored());
    }

    fn problems(code: &str) -> Vec<String> {
        scan(code).unwrap()
    }

    fn rejected_with(code: &str, text: &str) {
        let found = problems(code);
        assert!(
            found.iter().any(|problem| problem.contains(text)),
            "{code}: expected a problem containing {text:?}, got {found:?}"
        );
    }

    #[test]
    fn hidden_lines_are_restored_as_rustdoc_restores_them() {
        assert_eq!(compiled_line("#"), "");
        assert_eq!(compiled_line("  #  "), "");
        assert_eq!(compiled_line("# let x = 1;"), "let x = 1;");
        assert_eq!(compiled_line("   # let x = 1;"), "let x = 1;");
        assert_eq!(compiled_line("##[derive(Debug)]"), "#[derive(Debug)]");
        assert_eq!(compiled_line("  ## x"), "  # x");
        assert_eq!(compiled_line("#[derive(Debug)]"), "#[derive(Debug)]");
        assert_eq!(compiled_line("let x = 1;"), "let x = 1;");
    }

    #[test]
    fn a_macro_call_split_by_a_hidden_line_is_seen() {
        let code = "let s = concat\n# !(\"a\", \"b\");";
        rejected_with(code, "calls macro concat!");
        assert!(compiled_code(code).contains("concat\n!(\"a\", \"b\");"));
    }

    #[test]
    fn every_allowlisted_macro_is_accepted_by_bare_name() {
        for name in MACROS {
            let code = format!("{name}!(x);\n{name}![x];\n{name}! {{ x }}");
            assert!(problems(&code).is_empty(), "{code}");
        }
    }

    #[test]
    fn every_allowlisted_derive_is_accepted_alone_and_together() {
        for name in DERIVES {
            let code = format!("#[derive({name})]\nstruct S;");
            assert!(problems(&code).is_empty(), "{code}");
        }
        let together = format!("#[derive({},)]\nstruct S;", DERIVES.join(", "));
        assert!(problems(&together).is_empty(), "{together}");
        assert!(problems("#[derive()]\nstruct S;").is_empty());
    }

    #[test]
    fn cfg_doc_comments_and_plain_code_are_accepted() {
        for code in [
            "#[cfg(all())]\nfn f() {}",
            "#![cfg(all())]",
            "/// A doc comment.\nfn f() {}",
            "//! An inner doc comment.",
            "#[doc = \"text\"]\nfn f() {}",
            "#[doc = r\"raw\"]\nfn f() {}",
            "#[doc = r#\"raw\"#]\nfn f() {}",
            "// #![allow(unused)] macro_rules! m {} use x::*;\nlet a = 1;",
            "let s = \"#![allow(unused)] include!(x) use x::*;\";",
            "use std::fmt::Write as _;\nlet mut out = String::new();\nwrite!(out, \"a\");",
            "fn f<'a>(x: &'a u8) -> impl Sized + use<'a> { x }",
            "let b = a != c;",
            "let n = a # b;",
        ] {
            assert!(problems(code).is_empty(), "{code}: {:?}", problems(code));
        }
    }

    #[test]
    fn lowering_attributes_keep_their_d44_message() {
        for code in [
            "#![allow(unused)]",
            "#![allow(warnings)]",
            "#![expect(unused)]",
            "#![warn(unused)]",
            "#![cfg_attr(all(), allow(unused))]",
            "fn main() {\n    #[allow(unused_variables)]\n    let x = 1;\n}",
            "# #![allow(unused)]\nlet x = 1;",
            "#[r#allow(dead_code)]\nfn f() {}",
        ] {
            rejected_with(code, "sets a lint level with");
            rejected_with(code, "(D44).");
        }
    }

    #[test]
    fn macros_off_the_allowlist_are_rejected() {
        let lvl = "macro_rules! lvl { ($l:ident) => { #[$l(unused_variables)] fn f() { let x = 1; } }; }\nlvl!(allow);\nf();";
        rejected_with(lvl, "defines a macro with macro_rules! lvl");
        rejected_with(lvl, "calls macro lvl!");
        for (code, text) in [
            ("include!(\"x.rs\");", "calls macro include!"),
            (
                "let s = include_str!(\"x.md\");",
                "calls macro include_str!",
            ),
            ("let s = concat!(\"a\", \"b\");", "calls macro concat!"),
            ("unknown_macro!();", "calls macro unknown_macro!"),
            ("std::assert_eq!(1, 1);", "calls macro std::assert_eq!"),
            ("core::panic!(\"x\");", "calls macro core::panic!"),
            ("::core::panic!(\"x\");", "calls macro ::core::panic!"),
            (
                "let v = vec![unknown_macro!()];",
                "calls macro unknown_macro!",
            ),
        ] {
            rejected_with(code, text);
            rejected_with(code, "(D46).");
        }
    }

    #[test]
    fn macro_rules_is_rejected_at_any_depth() {
        rejected_with(
            "macro_rules! m { () => {} }",
            "defines a macro with macro_rules! m",
        );
        rejected_with(
            "fn main() {\n    macro_rules! m { () => {} }\n}",
            "defines a macro with macro_rules! m",
        );
        rejected_with("macro_rules!", "defines a macro with macro_rules! ,");
    }

    #[test]
    fn attributes_off_the_allowlist_are_rejected() {
        for code in [
            "#[macro_use]\nextern crate foo;",
            "#[cfg_attr(all(), derive(Debug))]\nstruct S;",
            "#[derive(Serialize)]\nstruct S;",
            "#[derive(std::fmt::Debug)]\nstruct S;",
            "#[derive(Debug Clone)]\nstruct S;",
            "#[derive[Debug]]\nstruct S;",
            "#[tokio::main]\nasync fn main() {}",
            "#[test]\nfn t() {}",
            "#[path = \"x.rs\"]\nmod m;",
            "#[doc(hidden)]\nfn f() {}",
            "#[doc = concat!(\"a\")]\nfn f() {}",
            "#[doc = b\"bytes\"]\nfn f() {}",
            "#[doc = 1]\nfn f() {}",
            "#[doc + \"x\"]\nfn f() {}",
            "#[cfg = \"x\"]\nfn f() {}",
            "#[cfg]\nfn f() {}",
            "# #[derive(Serialize)]\nstruct S;",
            "##[test]\nfn t() {}",
        ] {
            rejected_with(
                code,
                "which is not on the doctest attribute allowlist (D46).",
            );
        }
    }

    #[test]
    fn use_items_that_glob_or_shadow_are_rejected() {
        rejected_with("use foo::*;", "has a glob import, use foo ::*");
        rejected_with("use foo::{bar, *};", "has a glob import");
        rejected_with("use foo::bar as assert_eq;", "imports or renames assert_eq");
        rejected_with("use foo::assert_eq;", "imports or renames assert_eq");
        rejected_with("use foo::{bar, vec};", "imports or renames vec");
        rejected_with("use foo::r#format;", "imports or renames r#format");
        rejected_with("use foo::vec", "imports or renames vec");
    }

    #[test]
    fn use_items_naming_an_allowlisted_derive_are_rejected() {
        for name in DERIVES {
            for code in [
                format!("use std::x::{name};"),
                format!("use a::b as {name};"),
                format!("use a::{{b, c::{{{name}}}}};"),
            ] {
                let found = problems(&code);
                let expected = format!(
                    "imports or renames {name} in use {}, a name on the doctest derive allowlist (D47).",
                    code.trim_start_matches("use ")
                        .trim_end_matches(';')
                        .parse::<proc_macro2::TokenStream>()
                        .unwrap()
                );
                assert_eq!(found, [expected], "{code}");
            }
        }
        rejected_with("use a::r#Debug;", "imports or renames r#Debug");
        rejected_with("use std::hash::Hash as _;", "imports or renames Hash");
        rejected_with("use std::hash::Hash as Debug;", "imports or renames Hash");
        assert_eq!(problems("use std::hash::Hash as Debug;").len(), 2);
    }

    #[test]
    fn use_items_naming_neither_list_are_accepted() {
        for code in [
            "use std::collections::HashMap;",
            "use std::{collections::{HashMap, BTreeMap}, io::{self, Read}};",
        ] {
            assert!(problems(code).is_empty(), "{code}: {:?}", problems(code));
        }
    }

    #[test]
    fn code_that_does_not_tokenize_is_an_error() {
        assert!(scan("let s = \"open;").is_err());
    }
}
