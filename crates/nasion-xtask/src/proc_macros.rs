//! Locks the set of proc macros in the build graph (D52). A proc macro runs
//! arbitrary code inside rustc: it can read a file that dep-info never lists,
//! or build a `doc(test(...))` attribute from a string the token rules never
//! see. So the packages with a proc-macro target that the workspace can build
//! must be exactly `ALLOWED`, name and version, and no workspace member may
//! have a proc-macro target.
//!
//! The graph is cargo's resolve with every feature and every platform, walked
//! from the workspace members through normal, build and dev dependencies. Dev
//! dependencies count only from a workspace member, because cargo never builds
//! the dev dependencies of a dependency.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// The proc macros the build graph must hold, by name and version (D52). A
/// change needs a new decision that repeats the source read of every crate
/// that runs inside the new version.
const ALLOWED: [(&str, &str); 2] = [("pyo3-macros", "0.30.1"), ("serde_derive", "1.0.229")];

/// Runs cargo metadata with dependencies and returns the number of proc macros
/// in the build graph and one line per D52 problem.
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
    evaluate_json(&output.stdout).map_err(|e| format!("{shown}: {e}\n{}", stderr.trim_end()))
}

fn evaluate_json(stdout: &[u8]) -> Result<(usize, Vec<String>), String> {
    let metadata: Value =
        serde_json::from_slice(stdout).map_err(|e| format!("the output is not JSON: {e}"))?;
    evaluate(&metadata)
}

/// Each package id's dependencies, with whether the edge is dev only.
type Edges<'a> = HashMap<&'a str, Vec<(&'a str, bool)>>;

struct Package<'a> {
    name: &'a str,
    version: &'a str,
    proc_macro: bool,
}

/// The D52 comparison on parsed cargo metadata.
fn evaluate(metadata: &Value) -> Result<(usize, Vec<String>), String> {
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
    let mut found: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    for (id, users) in &reachable {
        let package = known(&packages, id)?;
        if !package.proc_macro || members.contains(id) {
            continue;
        }
        let names = users
            .iter()
            .map(|user| known(&packages, user).map(|p| p.name))
            .collect::<Result<BTreeSet<_>, _>>()?;
        found
            .entry((package.name, package.version))
            .or_default()
            .extend(names);
    }
    for ((name, version), users) in &found {
        let users = users.iter().copied().collect::<Vec<_>>().join(", ");
        match ALLOWED.iter().find(|(allowed, _)| allowed == name) {
            None => problems.push(format!(
                "proc macro {name} {version}, used by {users}, is not on the D52 allowlist (D52)."
            )),
            Some((_, allowed)) if allowed != version => problems.push(format!(
                "proc macro {name} {version}, used by {users}, is on the D52 allowlist only as {name} {allowed} (D52)."
            )),
            Some(_) => {}
        }
    }
    for (name, version) in ALLOWED {
        if !found.contains_key(&(name, version)) {
            problems.push(format!(
                "proc macro {name} {version} is on the D52 allowlist but not in the build graph (D52)."
            ));
        }
    }
    Ok((found.len(), problems))
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
        packages.insert(
            text(package, "id")?,
            Package {
                name: text(package, "name")?,
                version: text(package, "version")?,
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
            for kind in kinds {
                let kind = kind.get("kind").ok_or("a dep_kinds entry has no kind")?;
                dev_only &= kind.as_str() == Some("dev");
            }
            deps.push((text(dep, "pkg")?, dev_only));
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
        for (dep, dev_only) in edges.get(id).into_iter().flatten() {
            if *dev_only && !members.contains(id) {
                continue;
            }
            let users = seen.entry(dep).or_default();
            let first = users.is_empty() && !members.contains(dep);
            users.insert(id);
            if first {
                queue.push(dep);
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{evaluate, evaluate_json};

    const CORE: &str = "path+file:///ws/crates/core#0.1.0";
    const TOOL: &str = "path+file:///ws/crates/tool#0.1.0";
    const PYO3: &str = "registry#pyo3@0.30.1";
    const PYO3_MACROS: &str = "registry#pyo3-macros@0.30.1";
    const SERDE: &str = "registry#serde_core@1.0.229";
    const SERDE_DERIVE: &str = "registry#serde_derive@1.0.229";

    fn id_parts(id: &str) -> (&str, &str) {
        let tail = id.rsplit('/').next().unwrap().rsplit('#').next().unwrap();
        tail.split_once('@')
            .or_else(|| tail.split_once('#'))
            .unwrap_or((tail, "0.1.0"))
    }

    fn package(id: &str, proc_macro: bool) -> Value {
        let (name, version) = id_parts(id);
        let name = if id.starts_with("path+") {
            id.split('/')
                .next_back()
                .unwrap()
                .split('#')
                .next()
                .unwrap()
        } else {
            name
        };
        let kind = if proc_macro { "proc-macro" } else { "lib" };
        json!({"id": id, "name": name, "version": version, "targets": [{"kind": [kind]}]})
    }

    fn edge(pkg: &str, kinds: &[Option<&str>]) -> Value {
        let kinds: Vec<Value> = kinds
            .iter()
            .map(|kind| json!({"kind": kind, "target": null}))
            .collect();
        json!({"pkg": pkg, "dep_kinds": kinds})
    }

    /// Metadata with the given packages (id, proc macro), members and edges
    /// (from, to, kinds).
    fn metadata(
        packages: &[(&str, bool)],
        members: &[&str],
        edges: &[(&str, &str, &[Option<&str>])],
    ) -> Value {
        let nodes: Vec<Value> = packages
            .iter()
            .map(|(id, _)| {
                let deps: Vec<Value> = edges
                    .iter()
                    .filter(|(from, _, _)| from == id)
                    .map(|(_, to, kinds)| edge(to, kinds))
                    .collect();
                json!({"id": id, "deps": deps})
            })
            .collect();
        json!({
            "packages": packages.iter().map(|(id, proc_macro)| package(id, *proc_macro)).collect::<Vec<_>>(),
            "workspace_members": members,
            "resolve": {"nodes": nodes},
        })
    }

    const NORMAL: &[Option<&str>] = &[None];
    const BUILD: &[Option<&str>] = &[Some("build")];
    const DEV: &[Option<&str>] = &[Some("dev")];

    fn base_packages() -> Vec<(&'static str, bool)> {
        vec![
            (CORE, false),
            (TOOL, false),
            (PYO3, false),
            (PYO3_MACROS, true),
            (SERDE, false),
            (SERDE_DERIVE, true),
        ]
    }

    fn base_edges() -> Vec<(&'static str, &'static str, &'static [Option<&'static str>])> {
        vec![
            (CORE, PYO3, NORMAL),
            (PYO3, PYO3_MACROS, NORMAL),
            (TOOL, SERDE, NORMAL),
            (SERDE, SERDE_DERIVE, NORMAL),
        ]
    }

    fn run(
        packages: &[(&str, bool)],
        edges: &[(&str, &str, &[Option<&str>])],
    ) -> (usize, Vec<String>) {
        evaluate(&metadata(packages, &[CORE, TOOL], edges)).unwrap()
    }

    #[test]
    fn the_exact_allowlist_is_accepted() {
        assert_eq!(run(&base_packages(), &base_edges()), (2, vec![]));
    }

    #[test]
    fn a_proc_macro_off_the_list_is_rejected_with_its_users() {
        let extra = "registry#thiserror-impl@2.0.0";
        let mut packages = base_packages();
        packages.push((extra, true));
        let mut edges = base_edges();
        edges.push((CORE, extra, NORMAL));
        edges.push((PYO3, extra, NORMAL));
        assert_eq!(
            run(&packages, &edges),
            (
                3,
                vec!["proc macro thiserror-impl 2.0.0, used by core, pyo3, is not on the D52 allowlist (D52).".to_owned()]
            )
        );
    }

    #[test]
    fn a_listed_proc_macro_missing_from_the_graph_is_rejected() {
        let mut edges = base_edges();
        edges.retain(|(_, to, _)| *to != SERDE_DERIVE);
        assert_eq!(
            run(&base_packages(), &edges),
            (
                1,
                vec!["proc macro serde_derive 1.0.229 is on the D52 allowlist but not in the build graph (D52).".to_owned()]
            )
        );
    }

    #[test]
    fn a_version_mismatch_of_each_listed_name_is_rejected() {
        for (old, new, user, name, version, listed) in [
            (
                PYO3_MACROS,
                "registry#pyo3-macros@0.30.2",
                "pyo3",
                "pyo3-macros",
                "0.30.2",
                "0.30.1",
            ),
            (
                SERDE_DERIVE,
                "registry#serde_derive@1.0.230",
                "serde_core",
                "serde_derive",
                "1.0.230",
                "1.0.229",
            ),
        ] {
            let packages: Vec<(&str, bool)> = base_packages()
                .into_iter()
                .map(|(id, proc_macro)| (if id == old { new } else { id }, proc_macro))
                .collect();
            let edges: Vec<(&str, &str, &[Option<&str>])> = base_edges()
                .into_iter()
                .map(|(from, to, kinds)| (from, if to == old { new } else { to }, kinds))
                .collect();
            assert_eq!(
                run(&packages, &edges),
                (
                    2,
                    vec![
                        format!(
                            "proc macro {name} {version}, used by {user}, is on the D52 allowlist only as {name} {listed} (D52)."
                        ),
                        format!(
                            "proc macro {name} {listed} is on the D52 allowlist but not in the build graph (D52)."
                        ),
                    ]
                )
            );
        }
    }

    #[test]
    fn a_workspace_member_with_a_proc_macro_target_is_rejected_once() {
        let member = "path+file:///ws/crates/derive#0.1.0";
        let mut packages = base_packages();
        packages.push((member, true));
        let mut edges = base_edges();
        edges.push((CORE, member, NORMAL));
        let found = evaluate(&metadata(&packages, &[CORE, TOOL, member], &edges)).unwrap();
        assert_eq!(
            found,
            (
                2,
                vec!["workspace member derive 0.1.0 has a proc-macro target, which no workspace member may have (D52).".to_owned()]
            )
        );
    }

    #[test]
    fn build_dependencies_and_member_dev_dependencies_count() {
        for (from, kinds) in [
            (CORE, BUILD),
            (CORE, DEV),
            (PYO3, BUILD),
            (PYO3, &[Some("dev"), None][..]),
            (PYO3, &[][..]),
        ] {
            let extra = "registry#extra@1.0.0";
            let mut packages = base_packages();
            packages.push((extra, true));
            let mut edges = base_edges();
            edges.push((from, extra, kinds));
            let (count, problems) = run(&packages, &edges);
            assert_eq!(count, 3, "{from} {kinds:?}");
            assert_eq!(problems.len(), 1, "{from} {kinds:?}: {problems:?}");
            assert!(
                problems[0].starts_with("proc macro extra 1.0.0, used by "),
                "{problems:?}"
            );
        }
    }

    #[test]
    fn unreachable_proc_macros_and_dev_dependencies_of_dependencies_do_not_count() {
        let extra = "registry#extra@1.0.0";
        let lone = "registry#lone@1.0.0";
        let mut packages = base_packages();
        packages.push((extra, true));
        packages.push((lone, true));
        let mut edges = base_edges();
        edges.push((PYO3, extra, DEV));
        assert_eq!(run(&packages, &edges), (2, vec![]));
    }

    #[test]
    fn malformed_metadata_is_an_error() {
        assert!(
            evaluate_json(b"not json")
                .unwrap_err()
                .starts_with("the output is not JSON")
        );
        let good = metadata(&base_packages(), &[CORE, TOOL], &base_edges());
        assert_eq!(evaluate_json(good.to_string().as_bytes()).unwrap().0, 2);
        type Change = Box<dyn Fn(&mut Value)>;
        let cases: Vec<(&str, Change)> = vec![
            (
                "no packages field",
                Box::new(|m| {
                    m.as_object_mut().unwrap().remove("packages");
                }),
            ),
            (
                "packages is not a list",
                Box::new(|m| m["packages"] = json!({})),
            ),
            (
                "no workspace_members field",
                Box::new(|m| {
                    m.as_object_mut().unwrap().remove("workspace_members");
                }),
            ),
            (
                "workspace_members is not a list",
                Box::new(|m| m["workspace_members"] = json!("x")),
            ),
            (
                "member id is not a string",
                Box::new(|m| m["workspace_members"][0] = json!(1)),
            ),
            ("no resolve field", Box::new(|m| m["resolve"] = Value::Null)),
            (
                "no nodes field",
                Box::new(|m| {
                    m["resolve"].as_object_mut().unwrap().remove("nodes");
                }),
            ),
            (
                "resolve.nodes is not a list",
                Box::new(|m| m["resolve"]["nodes"] = json!(1)),
            ),
            (
                "no id field",
                Box::new(|m| {
                    m["resolve"]["nodes"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("id");
                }),
            ),
            (
                "no deps field",
                Box::new(|m| {
                    m["resolve"]["nodes"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("deps");
                }),
            ),
            (
                "deps is not a list",
                Box::new(|m| m["resolve"]["nodes"][0]["deps"] = json!(1)),
            ),
            (
                "no pkg field",
                Box::new(|m| {
                    m["resolve"]["nodes"][0]["deps"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("pkg");
                }),
            ),
            (
                "pkg field is not a string",
                Box::new(|m| m["resolve"]["nodes"][0]["deps"][0]["pkg"] = json!(1)),
            ),
            (
                "no dep_kinds field",
                Box::new(|m| {
                    m["resolve"]["nodes"][0]["deps"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("dep_kinds");
                }),
            ),
            (
                "dep_kinds is not a list",
                Box::new(|m| m["resolve"]["nodes"][0]["deps"][0]["dep_kinds"] = json!(1)),
            ),
            (
                "entry has no kind",
                Box::new(|m| m["resolve"]["nodes"][0]["deps"][0]["dep_kinds"][0] = json!({})),
            ),
            (
                "package no id",
                Box::new(|m| {
                    m["packages"][0].as_object_mut().unwrap().remove("id");
                }),
            ),
            (
                "package no name",
                Box::new(|m| {
                    m["packages"][0].as_object_mut().unwrap().remove("name");
                }),
            ),
            (
                "package no version",
                Box::new(|m| {
                    m["packages"][0].as_object_mut().unwrap().remove("version");
                }),
            ),
            (
                "package no targets",
                Box::new(|m| {
                    m["packages"][0].as_object_mut().unwrap().remove("targets");
                }),
            ),
            (
                "targets is not a list",
                Box::new(|m| m["packages"][0]["targets"] = json!(1)),
            ),
            (
                "target no kind",
                Box::new(|m| m["packages"][0]["targets"][0] = json!({})),
            ),
            (
                "kind is not a list",
                Box::new(|m| m["packages"][0]["targets"][0]["kind"] = json!("lib")),
            ),
            (
                "kind entry is not a string",
                Box::new(|m| m["packages"][0]["targets"][0]["kind"] = json!([1])),
            ),
            (
                "member not in packages",
                Box::new(|m| m["workspace_members"][0] = json!("unknown#1")),
            ),
            (
                "graph id not in packages",
                Box::new(|m| m["resolve"]["nodes"][0]["deps"][0]["pkg"] = json!("unknown#1")),
            ),
        ];
        for (name, change) in cases {
            let mut broken = good.clone();
            change(&mut broken);
            assert!(evaluate(&broken).is_err(), "{name}");
        }
    }
}
