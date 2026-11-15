//! Rejects lint-lowering attributes in doctests (D44). A doctest's own
//! `allow`, `expect` or `warn` lifts the `deny(warnings)` that the crate root
//! sets for doctests, and no lint level at the root can stop that.
//!
//! The doctests come from rustdoc's own list, never from walking source files.
//! rustdoc reports each one at the line of the item's first doc fragment plus
//! the offset of the opening fence in the joined doc text, so the code blocks
//! are rebuilt the same way from each item's literal doc attributes.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::ext::IdentExt;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ExprLit, Lit, Meta};

/// Attributes that lower a lint level below what the crate root sets.
const LOWERING: [&str; 3] = ["allow", "expect", "warn"];

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
pub fn check(cargo: &OsStr, root: &Path) -> Result<(usize, Vec<String>), String> {
    let listed = list(cargo, root)?;
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
        match lowering_attributes(&block.code) {
            Ok(found) if found.is_empty() => {}
            Ok(found) => problems.push(format!(
                "{at} sets a lint level with {}. Doctests may not use allow, expect or warn, because they lift deny(warnings) (D44).",
                found.join(", ")
            )),
            Err(e) => problems.push(format!("{at} does not tokenize: {e}")),
        }
    }
    Ok((listed.len(), problems))
}

fn list(cargo: &OsStr, root: &Path) -> Result<Vec<Listed>, String> {
    let output = Command::new(cargo)
        .current_dir(root)
        .args([
            "test",
            "--locked",
            "--workspace",
            "--all-features",
            "--doc",
            "--",
            "--list",
        ])
        .output()
        .map_err(|e| format!("starting {} failed: {e}", cargo.to_string_lossy()))?;
    if !output.status.success() {
        return Err(format!(
            "cargo test --doc -- --list failed ({}). Check that the workspace and its doctests build.\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
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

/// Every attribute in `code` that holds allow, expect or warn followed by a
/// parenthesized list, at any depth, including inside cfg_attr. Code is
/// tokenized, so comments and string literals never count.
fn lowering_attributes(code: &str) -> Result<Vec<String>, proc_macro2::LexError> {
    let stream: TokenStream = code.parse()?;
    let mut found = Vec::new();
    collect_lowering(stream, &mut found);
    Ok(found)
}

fn collect_lowering(stream: TokenStream, found: &mut Vec<String>) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Punct(pound) if pound.as_char() == '#' => {
                let bang = matches!(tokens.get(index + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!');
                let body = tokens.get(index + 1 + usize::from(bang));
                if let Some(TokenTree::Group(group)) = body
                    && group.delimiter() == Delimiter::Bracket
                    && lowers(group.stream())
                {
                    let inner = if bang { "!" } else { "" };
                    found.push(format!("#{inner}[{}]", group.stream()));
                }
            }
            TokenTree::Group(group) => collect_lowering(group.stream(), found),
            _ => {}
        }
    }
}

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
    use super::{fenced_blocks, lowering_attributes, parse_list_line};

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

    #[test]
    fn lowering_attributes_are_found_at_any_depth() {
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
            assert_eq!(lowering_attributes(code).unwrap().len(), 1, "{code}");
        }
    }

    #[test]
    fn the_word_in_comments_strings_and_other_attributes_does_not_count() {
        for code in [
            "// #![allow(unused)]\nlet x = 1;",
            "let s = \"#![allow(unused)]\";",
            "#![deny(warnings)]\n#[cfg(feature = \"allow\")]\nfn f() {}",
            "let allow = 1;\nlet _ = allow;",
        ] {
            assert!(lowering_attributes(code).unwrap().is_empty(), "{code}");
        }
    }

    #[test]
    fn code_that_does_not_tokenize_is_an_error() {
        assert!(lowering_attributes("let s = \"open;").is_err());
    }
}
