//! Locks the proc macros in the build graph and the code that runs inside
//! them (D52, D53). A proc macro runs arbitrary code inside rustc: it can read
//! a file that dep-info never lists, or build a `doc(test(...))` attribute from
//! a string the token rules never see. Its own dependencies run inside it too,
//! so patching syn changes what serde_derive emits.
//!
//! - No workspace member may have a proc-macro target (D52).
//! - Every proc macro the workspace can build, and everything it depends on
//!   through normal edges, must be on `ALLOWED` by name, version and checksum,
//!   come from crates.io by its package id, and have a Cargo.lock entry with
//!   exactly that source and checksum (D53). An edge that cargo builds only for
//!   some platforms still counts, so the closure is never smaller than what the
//!   host builds.
//!
//! The graph is cargo's resolve with every feature and every platform, walked
//! from the workspace members through normal, build and dev dependencies. Dev
//! dependencies count only from a workspace member, because cargo never builds
//! the dev dependencies of a dependency.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// The only source a pinned crate may come from.
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
/// Every crate that may run inside a proc macro, with its version and the
/// checksum Cargo.lock must hold for it (D53). A change needs a new decision
/// that repeats the source read of every crate in the changed closure.
const ALLOWED: [(&str, &str, &str); 8] = [
    (
        "pyo3-macros",
        "0.30.1",
        "c46d2e5862c7bb8207e3b639f713dda3c499933444784154c11d013c5a691b60",
    ),
    (
        "pyo3-macros-backend",
        "0.30.1",
        "38b89f066f2ae72a2aef52d1537ca2fcd104440ebfadee06e97aa90738a25b9b",
    ),
    (
        "serde_derive",
        "1.0.229",
        "e7a5d71263a5a7d47b41f6b3f06ba276f10cc18b0931f1799f710578e2309348",
    ),
    (
        "syn",
        "3.0.10",
        "4ae9ccbea3a7b0b09443036490c8f34a8b1b760fd345e5c0dae0aac718faea79",
    ),
    (
        "quote",
        "1.0.47",
        "1fbf4db142a473a8d80c26bbf18454ed458bf8d26c8219c331daecfdbd079001",
    ),
    (
        "proc-macro2",
        "1.0.107",
        "985e7ec9bb745e6ce6535b544d84d6cd6f7ad8bd711c398938ae983b91a766d9",
    ),
    (
        "heck",
        "0.5.0",
        "2304e00983f87ffb38b55b444b5e3b60a884b5d30c0fca7d82fe33449bbe55ea",
    ),
    (
        "unicode-ident",
        "1.0.27",
        "a2c754d6c33795a1c324727428e5a7dedb5b06195f9890bdbcba760d3e246563",
    ),
];

/// Runs cargo metadata with dependencies, reads Cargo.lock, and returns the
/// number of proc macros in the build graph and one line per problem.
pub fn check(cargo: &OsString, root: &Path) -> Result<(usize, Vec<String>), String> {
    let args = [
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--all-features",
    ];
    let shown = format!("cargo {}", args.join(" "));
    let output = Command::new(cargo)
        .current_dir(root)
        .env("CARGO_TERM_COLOR", "never")
        .args(args)
        .output()
        .map_err(|e| format!("starting {shown} failed: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!(
            "{shown} failed ({}).\n{}",
            output.status,
            stderr.trim_end()
        ));
    }
    let lock_path = root.join("Cargo.lock");
    let lock = fs::read_to_string(&lock_path)
        .map_err(|e| format!("{} cannot be read: {e}", lock_path.display()))?;
    let lock = parse_lock(&lock).map_err(|e| format!("{}: {e}", lock_path.display()))?;
    evaluate_json(&output.stdout, &lock).map_err(|e| format!("{shown}: {e}\n{}", stderr.trim_end()))
}

fn evaluate_json(stdout: &[u8], lock: &[LockEntry]) -> Result<(usize, Vec<String>), String> {
    let metadata: Value =
        serde_json::from_slice(stdout).map_err(|e| format!("the output is not JSON: {e}"))?;
    evaluate(&metadata, lock)
}

/// One `[[package]]` block of Cargo.lock.
#[derive(Debug, Default, PartialEq)]
struct LockEntry {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}

/// Reads Cargo.lock as cargo writes it: comment lines and a `version = N` line
/// at the top, then `[[package]]` blocks of `name`, `version`, `source`,
/// `checksum` and a `dependencies` list, one item per line. Anything else,
/// such as a `[[patch.unused]]` table, is an error, so nothing in the file
/// goes unread.
fn parse_lock(text: &str) -> Result<Vec<LockEntry>, String> {
    let mut entries: Vec<LockEntry> = Vec::new();
    let mut in_dependencies = false;
    let mut seen_version = false;
    for (number, line) in text.lines().enumerate() {
        let error = |what: &str| format!("line {} {what}: {line:?}", number + 1);
        if in_dependencies {
            if line == "]" {
                in_dependencies = false;
            } else if !(line.starts_with(" \"") && line.ends_with("\",")) {
                return Err(error("is not a dependency list item"));
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if line == "[[package]]" {
            entries.push(LockEntry::default());
            continue;
        }
        let Some(entry) = entries.last_mut() else {
            if line.starts_with('#') {
                continue;
            }
            if !seen_version
                && let Some(number) = line.strip_prefix("version = ")
                && number.parse::<u32>().is_ok()
            {
                seen_version = true;
                continue;
            }
            return Err(error("is not a comment or the lock file version"));
        };
        if line == "dependencies = [" {
            in_dependencies = true;
            continue;
        }
        let Some((key, value)) = line.split_once(" = ") else {
            return Err(error("is not a key and a value"));
        };
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .filter(|value| !value.contains(['"', '\\']))
            .ok_or_else(|| error("does not hold a plain quoted string"))?
            .to_owned();
        let slot = match key {
            "name" => (entry.name.is_empty()).then_some(&mut entry.name),
            "version" => (entry.version.is_empty()).then_some(&mut entry.version),
            "source" if entry.source.is_none() => {
                entry.source = Some(value);
                continue;
            }
            "checksum" if entry.checksum.is_none() => {
                entry.checksum = Some(value);
                continue;
            }
            _ => None,
        };
        let Some(slot) = slot else {
            return Err(error("is an unknown or repeated key"));
        };
        *slot = value;
    }
    if in_dependencies {
        return Err("the file ends inside a dependencies list".into());
    }
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.name.is_empty() || entry.version.is_empty())
    {
        return Err(format!(
            "a [[package]] block has no name or version: {entry:?}"
        ));
    }
    Ok(entries)
}

/// One dependency edge of a resolve node.
struct Edge<'a> {
    pkg: &'a str,
    dev_only: bool,
    normal: bool,
}

/// Each package id's dependency edges.
type Edges<'a> = HashMap<&'a str, Vec<Edge<'a>>>;

struct Package<'a> {
    name: &'a str,
    version: &'a str,
    source: Option<&'a str>,
    proc_macro: bool,
}

/// The D52 and D53 checks on parsed cargo metadata and Cargo.lock.
fn evaluate(metadata: &Value, lock: &[LockEntry]) -> Result<(usize, Vec<String>), String> {
    let packages = packages(metadata)?;
    let members: BTreeSet<&str> = field(metadata, "workspace_members")?
        .as_array()
        .ok_or("workspace_members is not a list")?
        .iter()
        .map(|id| id.as_str().ok_or("a workspace member id is not a string"))
        .collect::<Result<_, _>>()?;
    let edges = edges(metadata)?;
    let reachable = reachable(&members, &edges);
    let mut problems = Vec::new();
    for id in &members {
        let package = known(&packages, id)?;
        if package.proc_macro {
            problems.push(format!(
                "workspace member {} {} has a proc-macro target, which no workspace member may have (D52).",
                package.name, package.version
            ));
        }
    }
    let mut roots = Vec::new();
    for id in reachable.keys() {
        if known(&packages, id)?.proc_macro && !members.contains(id) {
            roots.push(*id);
        }
    }
    let closure = closure(&roots, &edges);
    let mut pinned = BTreeSet::new();
    for (id, macros) in &closure {
        let package = known(&packages, id)?;
        let described = if package.proc_macro {
            let users = reachable
                .get(id)
                .into_iter()
                .flatten()
                .map(|user| known(&packages, user).map(|p| p.name))
                .collect::<Result<BTreeSet<_>, _>>()?;
            format!(
                "proc macro {} {}, used by {},",
                package.name,
                package.version,
                users.into_iter().collect::<Vec<_>>().join(", ")
            )
        } else {
            let macros = macros
                .iter()
                .map(|root| known(&packages, root).map(|p| p.name))
                .collect::<Result<BTreeSet<_>, _>>()?;
            format!(
                "{} {}, which runs inside proc macro {},",
                package.name,
                package.version,
                macros.into_iter().collect::<Vec<_>>().join(", ")
            )
        };
        let Some((_, allowed, checksum)) =
            ALLOWED.iter().find(|(name, _, _)| *name == package.name)
        else {
            problems.push(format!("{described} is not on the D53 allowlist (D53)."));
            continue;
        };
        if *allowed != package.version {
            problems.push(format!(
                "{described} is on the D53 allowlist only as {} {allowed} (D53).",
                package.name
            ));
            continue;
        }
        pinned.insert(package.name);
        if package.source != Some(CRATES_IO) {
            problems.push(format!(
                "{described} comes from {}, not {CRATES_IO} (D53).",
                package.source.unwrap_or("a local path")
            ));
        }
        problems.extend(lock_problems(lock, package, checksum));
    }
    for (name, version, _) in ALLOWED {
        if !pinned.contains(name) {
            problems.push(format!(
                "{name} {version} is on the D53 allowlist but not in the proc macro closure (D53)."
            ));
        }
    }
    Ok((roots.len(), problems))
}

/// What Cargo.lock says about a pinned package that is wrong, if anything.
fn lock_problems(lock: &[LockEntry], package: &Package<'_>, checksum: &str) -> Vec<String> {
    let (name, version) = (package.name, package.version);
    let Some(entry) = lock.iter().find(|entry| {
        entry.name == name && entry.version == version && entry.source.as_deref() == package.source
    }) else {
        return vec![format!(
            "Cargo.lock has no entry for {name} {version} from {} (D53).",
            package.source.unwrap_or("a local path")
        )];
    };
    let mut problems = Vec::new();
    if entry.source.as_deref() != Some(CRATES_IO) {
        problems.push(format!(
            "Cargo.lock lists {name} {version} with source {}, not {CRATES_IO} (D53).",
            entry.source.as_deref().unwrap_or("none")
        ));
    }
    match entry.checksum.as_deref() {
        None => problems.push(format!(
            "Cargo.lock lists {name} {version} with no checksum, not {checksum} (D53)."
        )),
        Some(found) if found != checksum => problems.push(format!(
            "Cargo.lock lists {name} {version} with checksum {found}, not {checksum} (D53)."
        )),
        Some(_) => {}
    }
    problems
}

/// Every package the roots reach through normal edges, with the roots that
/// reach it. A root is in its own closure.
fn closure<'a>(roots: &[&'a str], edges: &Edges<'a>) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
    let mut found: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for root in roots {
        let mut queue = vec![*root];
        while let Some(id) = queue.pop() {
            if !found.entry(id).or_default().insert(root) {
                continue;
            }
            for edge in edges.get(id).into_iter().flatten() {
                if edge.normal {
                    queue.push(edge.pkg);
                }
            }
        }
    }
    found
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, String> {
    value
        .get(name)
        .filter(|found| !found.is_null())
        .ok_or_else(|| format!("the metadata has no {name} field"))
}

fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    field(value, name)?
        .as_str()
        .ok_or_else(|| format!("the {name} field is not a string"))
}

fn known<'a, 'b>(
    packages: &'b HashMap<&'a str, Package<'a>>,
    id: &str,
) -> Result<&'b Package<'a>, String> {
    packages
        .get(id)
        .ok_or_else(|| format!("package id {id} is in the resolve graph but not in packages"))
}

fn packages(metadata: &Value) -> Result<HashMap<&str, Package<'_>>, String> {
    let mut packages = HashMap::new();
    for package in field(metadata, "packages")?
        .as_array()
        .ok_or("packages is not a list")?
    {
        let mut proc_macro = false;
        for target in field(package, "targets")?
            .as_array()
            .ok_or("a package's targets is not a list")?
        {
            for kind in field(target, "kind")?
                .as_array()
                .ok_or("a target's kind is not a list")?
            {
                proc_macro |= kind.as_str().ok_or("a target kind is not a string")? == "proc-macro";
            }
        }
        let source = match package.get("source") {
            None => return Err("a package has no source field".into()),
            Some(Value::Null) => None,
            Some(source) => Some(
                source
                    .as_str()
                    .ok_or("a package's source is not a string")?,
            ),
        };
        packages.insert(
            text(package, "id")?,
            Package {
                name: text(package, "name")?,
                version: text(package, "version")?,
                source,
                proc_macro,
            },
        );
    }
    Ok(packages)
}

fn edges(metadata: &Value) -> Result<Edges<'_>, String> {
    let mut edges = HashMap::new();
    for node in field(field(metadata, "resolve")?, "nodes")?
        .as_array()
        .ok_or("resolve.nodes is not a list")?
    {
        let mut deps = Vec::new();
        for dep in field(node, "deps")?
            .as_array()
            .ok_or("a node's deps is not a list")?
        {
            let kinds = field(dep, "dep_kinds")?
                .as_array()
                .ok_or("a dependency's dep_kinds is not a list")?;
            let mut dev_only = !kinds.is_empty();
            let mut normal = kinds.is_empty();
            for kind in kinds {
                let kind = kind.get("kind").ok_or("a dep_kinds entry has no kind")?;
                dev_only &= kind.as_str() == Some("dev");
                normal |= kind.is_null();
            }
            deps.push(Edge {
                pkg: text(dep, "pkg")?,
                dev_only,
                normal,
            });
        }
        edges.insert(text(node, "id")?, deps);
    }
    Ok(edges)
}

/// Every package cargo can build from the members, with the packages that
/// depend on it.
fn reachable<'a>(
    members: &BTreeSet<&'a str>,
    edges: &Edges<'a>,
) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
    let mut seen: BTreeMap<&str, BTreeSet<&str>> =
        members.iter().map(|id| (*id, BTreeSet::new())).collect();
    let mut queue: Vec<&str> = members.iter().copied().collect();
    while let Some(id) = queue.pop() {
        for edge in edges.get(id).into_iter().flatten() {
            if edge.dev_only && !members.contains(id) {
                continue;
            }
            let users = seen.entry(edge.pkg).or_default();
            let first = users.is_empty() && !members.contains(edge.pkg);
            users.insert(id);
            if first {
                queue.push(edge.pkg);
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{ALLOWED, CRATES_IO, LockEntry, evaluate, evaluate_json, parse_lock};

    struct Pkg {
        id: String,
        name: String,
        version: String,
        source: Option<String>,
        proc_macro: bool,
    }

    struct Graph {
        packages: Vec<Pkg>,
        members: Vec<String>,
        edges: Vec<(String, String, Vec<Kind>)>,
        lock: Vec<LockEntry>,
    }

    /// A dep_kinds entry: kind and platform target.
    type Kind = (Option<&'static str>, Option<&'static str>);

    const NORMAL: Kind = (None, None);

    fn registry(name: &str, version: &str, proc_macro: bool) -> Pkg {
        Pkg {
            id: format!("{CRATES_IO}#{name}@{version}"),
            name: name.into(),
            version: version.into(),
            source: Some(CRATES_IO.into()),
            proc_macro,
        }
    }

    fn member(name: &str, proc_macro: bool) -> Pkg {
        Pkg {
            id: format!("path+file:///ws/crates/{name}#0.1.0"),
            name: name.into(),
            version: "0.1.0".into(),
            source: None,
            proc_macro,
        }
    }

    fn id(name: &str) -> String {
        ALLOWED
            .iter()
            .find(|(allowed, _, _)| *allowed == name)
            .map(|(name, version, _)| format!("{CRATES_IO}#{name}@{version}"))
            .unwrap_or_else(|| match name {
                "core" | "tool" | "derive" => format!("path+file:///ws/crates/{name}#0.1.0"),
                "pyo3" => format!("{CRATES_IO}#pyo3@0.30.1"),
                "serde_core" => format!("{CRATES_IO}#serde_core@1.0.229"),
                other => format!("{CRATES_IO}#{other}@1.0.0"),
            })
    }

    fn checksum(name: &str) -> String {
        ALLOWED
            .iter()
            .find(|(allowed, _, _)| *allowed == name)
            .map_or_else(|| "f".repeat(64), |(_, _, sum)| (*sum).to_owned())
    }

    impl Graph {
        fn base() -> Self {
            let mut packages = vec![
                member("core", false),
                member("tool", false),
                registry("pyo3", "0.30.1", false),
                registry("serde_core", "1.0.229", false),
            ];
            for (name, version, _) in ALLOWED {
                packages.push(registry(
                    name,
                    version,
                    matches!(name, "pyo3-macros" | "serde_derive"),
                ));
            }
            let mut graph = Graph {
                packages,
                members: vec![id("core"), id("tool")],
                edges: Vec::new(),
                lock: Vec::new(),
            };
            for (from, tos) in [
                ("core", &["pyo3"][..]),
                ("pyo3", &["pyo3-macros"]),
                (
                    "pyo3-macros",
                    &["pyo3-macros-backend", "proc-macro2", "quote", "syn"],
                ),
                (
                    "pyo3-macros-backend",
                    &["heck", "proc-macro2", "quote", "syn"],
                ),
                ("syn", &["proc-macro2", "quote", "unicode-ident"]),
                ("quote", &["proc-macro2"]),
                ("proc-macro2", &["unicode-ident"]),
                ("tool", &["serde_core"]),
                ("serde_core", &["serde_derive"]),
                ("serde_derive", &["proc-macro2", "quote", "syn"]),
            ] {
                for to in tos {
                    graph.edge(from, to, vec![NORMAL]);
                }
            }
            graph.relock();
            graph
        }

        fn edge(&mut self, from: &str, to: &str, kinds: Vec<Kind>) {
            self.edges.push((id(from), id(to), kinds));
        }

        fn add(&mut self, package: Pkg) {
            self.packages.push(package);
            self.relock();
        }

        fn relock(&mut self) {
            self.lock = self
                .packages
                .iter()
                .map(|p| LockEntry {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    source: p.source.clone(),
                    checksum: p.source.as_ref().map(|_| checksum(&p.name)),
                })
                .collect();
        }

        fn package(&mut self, name: &str) -> &mut Pkg {
            self.packages.iter_mut().find(|p| p.name == name).unwrap()
        }

        fn lock_entry(&mut self, name: &str) -> &mut LockEntry {
            self.lock.iter_mut().find(|e| e.name == name).unwrap()
        }

        fn metadata(&self) -> Value {
            let nodes: Vec<Value> = self
                .packages
                .iter()
                .map(|p| {
                    let deps: Vec<Value> = self
                        .edges
                        .iter()
                        .filter(|(from, _, _)| *from == p.id)
                        .map(|(_, to, kinds)| {
                            let kinds: Vec<Value> = kinds
                                .iter()
                                .map(|(kind, target)| json!({"kind": kind, "target": target}))
                                .collect();
                            json!({"pkg": to, "dep_kinds": kinds})
                        })
                        .collect();
                    json!({"id": p.id, "deps": deps})
                })
                .collect();
            let packages: Vec<Value> = self
                .packages
                .iter()
                .map(|p| {
                    let kind = if p.proc_macro { "proc-macro" } else { "lib" };
                    json!({"id": p.id, "name": p.name, "version": p.version, "source": p.source, "targets": [{"kind": [kind]}]})
                })
                .collect();
            json!({"packages": packages, "workspace_members": self.members, "resolve": {"nodes": nodes}})
        }

        fn run(&self) -> (usize, Vec<String>) {
            evaluate(&self.metadata(), &self.lock).unwrap()
        }
    }

    fn described(name: &str, version: &str) -> String {
        match name {
            "pyo3-macros" => format!("proc macro {name} {version}, used by pyo3,"),
            "serde_derive" => format!("proc macro {name} {version}, used by serde_core,"),
            "pyo3-macros-backend" | "heck" => {
                format!("{name} {version}, which runs inside proc macro pyo3-macros,")
            }
            _ => {
                format!("{name} {version}, which runs inside proc macro pyo3-macros, serde_derive,")
            }
        }
    }

    #[test]
    fn the_pinned_closure_is_accepted() {
        assert_eq!(Graph::base().run(), (2, vec![]));
    }

    #[test]
    fn each_pinned_crate_at_another_version_is_rejected() {
        for (name, listed, _) in ALLOWED {
            let mut graph = Graph::base();
            graph.package(name).version = "9.9.9".into();
            graph.lock_entry(name).version = "9.9.9".into();
            assert_eq!(
                graph.run().1,
                [
                    format!(
                        "{} is on the D53 allowlist only as {name} {listed} (D53).",
                        described(name, "9.9.9")
                    ),
                    format!(
                        "{name} {listed} is on the D53 allowlist but not in the proc macro closure (D53)."
                    ),
                ],
                "{name}"
            );
        }
    }

    #[test]
    fn each_pinned_crate_from_another_source_is_rejected() {
        let git = "git+https://example.com/fork#0123";
        for (name, version, sum) in ALLOWED {
            let mut graph = Graph::base();
            graph.package(name).source = Some(git.into());
            let entry = graph.lock_entry(name);
            entry.source = Some(git.into());
            entry.checksum = None;
            assert_eq!(
                graph.run().1,
                [
                    format!(
                        "{} comes from {git}, not {CRATES_IO} (D53).",
                        described(name, version)
                    ),
                    format!(
                        "Cargo.lock lists {name} {version} with source {git}, not {CRATES_IO} (D53)."
                    ),
                    format!("Cargo.lock lists {name} {version} with no checksum, not {sum} (D53)."),
                ],
                "{name}"
            );
        }
    }

    #[test]
    fn a_pinned_crate_patched_to_a_path_is_rejected() {
        let mut graph = Graph::base();
        graph.package("serde_derive").source = None;
        let entry = graph.lock_entry("serde_derive");
        entry.source = None;
        entry.checksum = None;
        let sum = ALLOWED[2].2;
        assert_eq!(
            graph.run().1,
            [
                format!(
                    "proc macro serde_derive 1.0.229, used by serde_core, comes from a local path, not {CRATES_IO} (D53)."
                ),
                format!(
                    "Cargo.lock lists serde_derive 1.0.229 with source none, not {CRATES_IO} (D53)."
                ),
                format!("Cargo.lock lists serde_derive 1.0.229 with no checksum, not {sum} (D53)."),
            ]
        );
    }

    #[test]
    fn each_pinned_crate_with_another_checksum_is_rejected() {
        let other = "0".repeat(64);
        for (name, version, sum) in ALLOWED {
            let mut graph = Graph::base();
            graph.lock_entry(name).checksum = Some(other.clone());
            assert_eq!(
                graph.run().1,
                [format!(
                    "Cargo.lock lists {name} {version} with checksum {other}, not {sum} (D53)."
                )],
                "{name}"
            );
        }
        let mut graph = Graph::base();
        graph.lock_entry("quote").checksum = None;
        assert_eq!(
            graph.run().1,
            [format!(
                "Cargo.lock lists quote 1.0.47 with no checksum, not {} (D53).",
                ALLOWED[4].2
            )]
        );
    }

    #[test]
    fn a_pinned_crate_missing_from_the_lock_file_is_rejected() {
        let mut graph = Graph::base();
        graph.lock.retain(|entry| entry.name != "syn");
        assert_eq!(
            graph.run().1,
            [format!(
                "Cargo.lock has no entry for syn 3.0.10 from {CRATES_IO} (D53)."
            )]
        );
    }

    #[test]
    fn an_extra_crate_or_proc_macro_is_rejected() {
        let mut graph = Graph::base();
        graph.add(registry("extra", "1.0.0", false));
        graph.edge("syn", "extra", vec![NORMAL]);
        assert_eq!(
            graph.run(),
            (
                2,
                vec![
                    "extra 1.0.0, which runs inside proc macro pyo3-macros, serde_derive, is not on the D53 allowlist (D53)."
                        .to_owned()
                ]
            )
        );
        let mut graph = Graph::base();
        graph.add(registry("paste", "1.0.0", true));
        graph.edge("core", "paste", vec![NORMAL]);
        graph.edge("pyo3", "paste", vec![NORMAL]);
        assert_eq!(
            graph.run(),
            (
                3,
                vec![
                    "proc macro paste 1.0.0, used by core, pyo3, is not on the D53 allowlist (D53)."
                        .to_owned()
                ]
            )
        );
    }

    #[test]
    fn a_pinned_crate_left_out_of_the_closure_is_rejected() {
        let mut graph = Graph::base();
        graph
            .edges
            .retain(|(from, to, _)| !(from.contains("pyo3-macros-backend") && to.contains("heck")));
        assert_eq!(
            graph.run().1,
            ["heck 0.5.0 is on the D53 allowlist but not in the proc macro closure (D53)."]
        );
    }

    #[test]
    fn the_closure_follows_normal_edges_on_any_platform_only() {
        for (kinds, followed) in [
            (vec![(Some("build"), None)], false),
            (vec![(Some("dev"), None)], false),
            (vec![(None, Some("cfg(windows)"))], true),
            (vec![(Some("build"), None), NORMAL], true),
            (vec![], true),
        ] {
            let mut graph = Graph::base();
            graph.add(registry("extra", "1.0.0", false));
            graph.edge("quote", "extra", kinds.clone());
            let problems = graph.run().1;
            assert_eq!(
                problems.len(),
                usize::from(followed),
                "{kinds:?}: {problems:?}"
            );
        }
    }

    #[test]
    fn a_workspace_member_with_a_proc_macro_target_is_rejected_once() {
        let mut graph = Graph::base();
        graph.add(member("derive", true));
        graph.members.push(id("derive"));
        graph.edge("core", "derive", vec![NORMAL]);
        assert_eq!(
            graph.run(),
            (
                2,
                vec![
                    "workspace member derive 0.1.0 has a proc-macro target, which no workspace member may have (D52)."
                        .to_owned()
                ]
            )
        );
    }

    #[test]
    fn proc_macros_count_through_build_and_member_dev_edges_only() {
        for (from, kinds, counted) in [
            ("core", vec![(Some("build"), None)], true),
            ("core", vec![(Some("dev"), None)], true),
            ("pyo3", vec![(Some("build"), None)], true),
            ("pyo3", vec![(Some("dev"), None)], false),
            ("pyo3", vec![(Some("dev"), None), NORMAL], true),
        ] {
            let mut graph = Graph::base();
            graph.add(registry("paste", "1.0.0", true));
            graph.edge(from, "paste", kinds.clone());
            let (count, problems) = graph.run();
            assert_eq!(
                (count, problems.len()),
                if counted { (3, 1) } else { (2, 0) },
                "{from} {kinds:?}"
            );
        }
        let mut graph = Graph::base();
        graph.add(registry("lone", "1.0.0", true));
        assert_eq!(graph.run(), (2, vec![]));
    }

    #[test]
    fn malformed_metadata_is_an_error() {
        let graph = Graph::base();
        assert!(
            evaluate_json(b"not json", &graph.lock)
                .unwrap_err()
                .starts_with("the output is not JSON")
        );
        let good = graph.metadata();
        assert_eq!(
            evaluate_json(good.to_string().as_bytes(), &graph.lock).unwrap(),
            (2, vec![])
        );
        type Change = Box<dyn Fn(&mut Value)>;
        let remove = |path: &'static [&'static str], key: &'static str| -> Change {
            Box::new(move |m: &mut Value| {
                let mut at = &mut *m;
                for step in path {
                    at = match step.parse::<usize>() {
                        Ok(index) => &mut at[index],
                        Err(_) => &mut at[*step],
                    };
                }
                at.as_object_mut().unwrap().remove(key);
            })
        };
        let set = |path: &'static [&'static str], value: Value| -> Change {
            Box::new(move |m: &mut Value| {
                let mut at = &mut *m;
                for step in path {
                    at = match step.parse::<usize>() {
                        Ok(index) => &mut at[index],
                        Err(_) => &mut at[*step],
                    };
                }
                *at = value.clone();
            })
        };
        let cases: Vec<(&str, Change)> = vec![
            ("no packages", remove(&[], "packages")),
            ("packages not a list", set(&["packages"], json!({}))),
            ("no workspace_members", remove(&[], "workspace_members")),
            (
                "members not a list",
                set(&["workspace_members"], json!("x")),
            ),
            (
                "member not a string",
                set(&["workspace_members", "0"], json!(1)),
            ),
            ("resolve null", set(&["resolve"], Value::Null)),
            ("no nodes", remove(&["resolve"], "nodes")),
            ("nodes not a list", set(&["resolve", "nodes"], json!(1))),
            ("node no id", remove(&["resolve", "nodes", "0"], "id")),
            ("node no deps", remove(&["resolve", "nodes", "0"], "deps")),
            (
                "deps not a list",
                set(&["resolve", "nodes", "0", "deps"], json!(1)),
            ),
            (
                "dep no pkg",
                remove(&["resolve", "nodes", "0", "deps", "0"], "pkg"),
            ),
            (
                "pkg not a string",
                set(&["resolve", "nodes", "0", "deps", "0", "pkg"], json!(1)),
            ),
            (
                "dep no dep_kinds",
                remove(&["resolve", "nodes", "0", "deps", "0"], "dep_kinds"),
            ),
            (
                "dep_kinds not a list",
                set(
                    &["resolve", "nodes", "0", "deps", "0", "dep_kinds"],
                    json!(1),
                ),
            ),
            (
                "kind entry has no kind",
                set(
                    &["resolve", "nodes", "0", "deps", "0", "dep_kinds", "0"],
                    json!({}),
                ),
            ),
            ("package no id", remove(&["packages", "0"], "id")),
            ("package no name", remove(&["packages", "0"], "name")),
            ("package no version", remove(&["packages", "0"], "version")),
            ("package no source", remove(&["packages", "0"], "source")),
            (
                "source not a string",
                set(&["packages", "0", "source"], json!(1)),
            ),
            ("package no targets", remove(&["packages", "0"], "targets")),
            (
                "targets not a list",
                set(&["packages", "0", "targets"], json!(1)),
            ),
            (
                "target no kind",
                set(&["packages", "0", "targets", "0"], json!({})),
            ),
            (
                "kind not a list",
                set(&["packages", "0", "targets", "0", "kind"], json!("lib")),
            ),
            (
                "kind entry not a string",
                set(&["packages", "0", "targets", "0", "kind"], json!([1])),
            ),
            (
                "member not in packages",
                set(&["workspace_members", "0"], json!("unknown#1")),
            ),
            (
                "graph id not in packages",
                set(
                    &["resolve", "nodes", "0", "deps", "0", "pkg"],
                    json!("unknown#1"),
                ),
            ),
        ];
        for (name, change) in cases {
            let mut broken = good.clone();
            change(&mut broken);
            assert!(evaluate(&broken, &graph.lock).is_err(), "{name}");
        }
    }

    const LOCK: &str = "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"core\"\nversion = \"0.1.0\"\ndependencies = [\n \"syn\",\n \"quote 1.0.47 (registry+https://github.com/rust-lang/crates.io-index)\",\n]\n\n[[package]]\nname = \"syn\"\nversion = \"3.0.10\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"abc\"\n";

    #[test]
    fn a_lock_file_as_cargo_writes_it_is_read() {
        assert_eq!(
            parse_lock(LOCK).unwrap(),
            [
                LockEntry {
                    name: "core".into(),
                    version: "0.1.0".into(),
                    source: None,
                    checksum: None,
                },
                LockEntry {
                    name: "syn".into(),
                    version: "3.0.10".into(),
                    source: Some(CRATES_IO.into()),
                    checksum: Some("abc".into()),
                },
            ]
        );
        assert!(parse_lock("").unwrap().is_empty());
        let real =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
                .unwrap();
        assert!(parse_lock(&real).unwrap().len() > 8);
    }

    #[test]
    fn anything_else_in_the_lock_file_is_an_error() {
        for (text, what) in [
            ("garbage\n", "is not a comment or the lock file version"),
            (
                "version = 4\nversion = 4\n",
                "is not a comment or the lock file version",
            ),
            ("version = x\n", "is not a comment or the lock file version"),
            ("[[package]]\n# comment\n", "is not a key and a value"),
            (
                "[[package]]\nname = \"a\"\nname = \"b\"\n",
                "is an unknown or repeated key",
            ),
            (
                "[[package]]\nversion = \"1\"\nversion = \"2\"\n",
                "is an unknown or repeated key",
            ),
            (
                "[[package]]\nsource = \"a\"\nsource = \"b\"\n",
                "is an unknown or repeated key",
            ),
            (
                "[[package]]\nchecksum = \"a\"\nchecksum = \"b\"\n",
                "is an unknown or repeated key",
            ),
            (
                "[[package]]\nflavor = \"x\"\n",
                "is an unknown or repeated key",
            ),
            (
                "[[package]]\nname = x\n",
                "does not hold a plain quoted string",
            ),
            (
                "[[package]]\nname = \"a\\\"b\"\n",
                "does not hold a plain quoted string",
            ),
            (
                "[[package]]\nname = \"a\"b\"\n",
                "does not hold a plain quoted string",
            ),
            (
                "[[package]]\ndependencies = [\n\"a\",\n]\n",
                "is not a dependency list item",
            ),
            (
                "[[package]]\ndependencies = [\n \"a\"\n]\n",
                "is not a dependency list item",
            ),
            (
                "[[patch.unused]]\nname = \"x\"\n",
                "is not a comment or the lock file version",
            ),
            (
                "[[package]]\nname = \"a\"\nversion = \"1\"\n[[patch.unused]]\n",
                "is not a key and a value",
            ),
        ] {
            let error = parse_lock(text).unwrap_err();
            assert!(error.contains(what), "{text:?}: {error}");
        }
        assert_eq!(
            parse_lock("[[package]]\nname = \"a\"\nversion = \"1\"\ndependencies = [\n")
                .unwrap_err(),
            "the file ends inside a dependencies list"
        );
        assert!(
            parse_lock("[[package]]\nname = \"a\"\n")
                .unwrap_err()
                .starts_with("a [[package]] block has no name or version")
        );
        assert!(parse_lock("[[package]]\nversion = \"1\"\n").is_err());
    }
}
