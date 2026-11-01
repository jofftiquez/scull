//! SAFE-10 as the PRD states it: deletion is only possible through the core `Executor` API, and
//! the desktop has no direct filesystem-deletion code path. Since P3-EXEC-01 the desktop runs
//! cleanups: only the bridge depends on `scull-cleaner`, only `src/cleanup.rs` uses it, and
//! that module reaches deletion only through the Executor's API (an allowlist of calls); no
//! desktop source calls a filesystem-mutation API itself. The MCP server never reaches the
//! cleaner.
//!
//! Runs in the main workspace on every OS without building Tauri: it reads manifests and
//! sources only (no network, no build). `apps/desktop/src-tauri/tests/safe10.rs` adds the
//! `cargo tree` check on the resolved graph of the app crate.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn desktop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// `[workspace.dependencies]` of the root manifest: key -> (package name, path if any). The
/// package is the key unless `package = "..."` renames it.
fn workspace_paths() -> BTreeMap<String, (String, Option<PathBuf>)> {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let mut out = BTreeMap::new();
    let mut in_ws = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_ws = line == "[workspace.dependencies]";
            continue;
        }
        if in_ws && let Some((key, rest)) = line.split_once('=') {
            let key = key.trim().trim_matches('"').to_owned();
            let name = quoted_after(rest, "package").unwrap_or_else(|| key.clone());
            let path = quoted_after(rest, "path").map(|p| root.join(p));
            out.insert(key, (name, path));
        }
    }
    out
}

fn quoted_after(s: &str, key: &str) -> Option<String> {
    let i = s.find(key)?;
    let rest = &s[i + key.len()..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(rest[start..end].to_owned())
}

/// A non-dev dependency section header: `[dependencies]`, `[build-dependencies]`,
/// `[target.'cfg(..)'.dependencies]`, or a `[dependencies.<name>]` table.
fn dep_section(header: &str) -> Option<Option<String>> {
    let h = header.trim_start_matches('[').trim_end_matches(']');
    if h.contains("dev-dependencies") {
        return None;
    }
    let tail = h.rsplit("dependencies").next().unwrap_or("");
    if !h.contains("dependencies") {
        return None;
    }
    if let Some(name) = tail.strip_prefix('.') {
        return Some(Some(name.trim_matches('"').to_owned()));
    }
    Some(None)
}

#[derive(Debug, Default)]
struct Manifest {
    /// `scull-*` packages this manifest depends on (normal and build edges), by package name
    /// (a `package = "..."` rename is resolved), with their directory when it can be resolved.
    deps: Vec<(String, Option<PathBuf>)>,
    /// Whether `scull-platform`'s `delete` feature is enabled anywhere.
    enables_delete: bool,
}

/// One dependency entry while reading: its key, package name, path, and whether its own
/// `features` list names `delete`.
struct Entry {
    key: String,
    name: String,
    path: Option<PathBuf>,
    workspace: bool,
    delete: bool,
}

fn read_manifest(dir: &Path, ws: &BTreeMap<String, (String, Option<PathBuf>)>) -> Manifest {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    let mut entries: Vec<Entry> = Vec::new();
    // Feature lists of the whole manifest, for `<key>/delete` forwarding.
    let mut features = String::new();
    let mut section: Option<Option<String>> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            section = dep_section(line);
            if let Some(Some(key)) = &section {
                // A `[dependencies.<key>]` table; its lines fill the entry in.
                entries.push(Entry {
                    key: key.clone(),
                    name: key.clone(),
                    path: None,
                    workspace: false,
                    delete: false,
                });
            }
            continue;
        }
        features.push_str(line);
        features.push('\n');
        let Some(sec) = &section else { continue };
        let delete = line.contains("\"delete\"") || line.contains("'delete'");
        match sec {
            Some(_) => {
                let e = entries.last_mut().unwrap();
                let (k, v) = line.split_once('=').unwrap_or((line, ""));
                match k.trim() {
                    "package" => e.name = quoted_after(v, "").unwrap_or_default(),
                    "path" => e.path = quoted_after(v, "").map(|p| dir.join(p)),
                    "workspace" => e.workspace = v.trim() == "true",
                    _ => e.delete |= delete,
                }
            }
            None => {
                let Some((k, v)) = line.split_once('=') else {
                    continue;
                };
                // `key = ...`, `key.workspace = true`, `"key" = ...`
                let key = k.trim().split('.').next().unwrap_or("").trim_matches('"');
                if key.is_empty() {
                    continue;
                }
                entries.push(Entry {
                    key: key.to_owned(),
                    name: quoted_after(v, "package").unwrap_or_else(|| key.to_owned()),
                    path: quoted_after(v, "path").map(|p| dir.join(p)),
                    workspace: k.contains(".workspace") || v.contains("workspace = true"),
                    delete,
                });
            }
        }
    }
    let mut m = Manifest::default();
    for e in entries {
        let (name, path) = match (e.workspace, ws.get(&e.key)) {
            (true, Some((name, path))) => (name.clone(), path.clone()),
            _ => (
                e.name.clone(),
                e.path.or_else(|| ws.get(&e.name).and_then(|w| w.1.clone())),
            ),
        };
        if name == "scull-platform"
            && (e.delete
                || features.contains(&format!("{}/delete", e.key))
                || features.contains(&format!("{}?/delete", e.key)))
        {
            m.enables_delete = true;
        }
        if name.starts_with("scull-") {
            m.deps.push((name, path));
        }
    }
    if features.contains("scull-platform/delete") || features.contains("scull-platform?/delete") {
        m.enables_delete = true;
    }
    m
}

/// Every `scull-*` crate reachable from `start` through normal and build dependencies.
fn reachable(start: &Path) -> (Vec<String>, Vec<String>) {
    let ws = workspace_paths();
    let mut seen: Vec<String> = Vec::new();
    let mut delete: Vec<String> = Vec::new();
    let mut todo = vec![(String::from("<start>"), start.to_path_buf())];
    while let Some((name, dir)) = todo.pop() {
        if seen.contains(&name) {
            continue;
        }
        let m = read_manifest(&dir, &ws);
        if m.enables_delete {
            delete.push(name.clone());
        }
        for (dep, path) in m.deps {
            if dep == "scull-cleaner" {
                seen.push(dep);
                continue;
            }
            let path = path.unwrap_or_else(|| panic!("{name}: cannot resolve {dep}"));
            todo.push((dep, path));
        }
        seen.push(name);
    }
    (seen, delete)
}

/// Direct `scull-*` dependencies of the manifest in `dir`.
fn direct_deps(dir: &Path) -> Vec<String> {
    read_manifest(dir, &workspace_paths())
        .deps
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

/// PRD SAFE-10: deletion only through the core Executor. The bridge reaches `scull-cleaner`
/// (P3-EXEC-01) and never enables `scull-platform`'s `delete` feature itself; the Tauri app
/// crate reaches the cleaner only through the bridge.
#[test]
fn only_the_bridge_reaches_the_cleaner() {
    let bridge = direct_deps(&desktop().join("bridge"));
    assert!(bridge.iter().any(|d| d == "scull-cleaner"), "{bridge:?}");
    let ws = workspace_paths();
    assert!(
        !read_manifest(&desktop().join("bridge"), &ws).enables_delete,
        "the bridge enables scull-platform/delete itself"
    );
    let app = direct_deps(&desktop().join("src-tauri"));
    assert!(app.iter().any(|d| d == "scull-desktop-bridge"), "{app:?}");
    assert!(
        !app.iter().any(|d| d == "scull-cleaner"),
        "the app crate depends on scull-cleaner directly: {app:?}"
    );
    assert!(
        !read_manifest(&desktop().join("src-tauri"), &ws).enables_delete,
        "the app crate enables scull-platform/delete"
    );
    // The MCP server never reaches the cleaner (nor the desktop bridge).
    let (seen, delete) = reachable(&repo_root().join("crates/scull-mcp"));
    assert!(
        !seen
            .iter()
            .any(|c| c == "scull-cleaner" || c == "scull-desktop-bridge"),
        "{seen:?}"
    );
    assert!(delete.is_empty(), "{delete:?}");
}

/// The one desktop module allowed to use `scull_cleaner` (P3-EXEC-01).
const CLEANUP_MODULE: &str = "bridge/src/cleanup.rs";

/// Filesystem-mutation APIs that must not appear in desktop sources outside the Executor
/// module. The history database is written by scull-storage, never by desktop code.
const MUTATION_APIS: &[&str] = &[
    "scull_cleaner",
    "remove_file",
    "remove_dir",
    "fs::rename",
    "fs::write",
    "fs::copy",
    "create_dir",
    "set_permissions",
    "File::create",
    "OpenOptions",
    "hard_link",
    "symlink",
    "trash::",
];

/// What even the Executor module may not do itself: every deletion or move goes through the
/// Executor's operations (`execute_plan`, `restore`, `QuickClean::run`).
const DIRECT_DELETION: &[&str] = &[
    "remove_file",
    "remove_dir",
    "fs::rename",
    "fs::write",
    "fs::copy",
    "set_permissions",
    "File::create",
    "OpenOptions",
    "hard_link",
    "symlink",
    "scull_platform::remove",
    "scull_platform::trash",
    "scull_platform::quarantine",
    ".delete(",
    "delete_observed",
    ".trash(",
    "trash_checked",
    "delete_orphaned_cache",
    "remove_orphaned",
    "purge",
    "purge_own_files",
];

/// The `scull-cleaner` API names the module may call (any receiver): the operations, their
/// builders and the read-only accessors of their reports. Every other public method name of
/// `scull-cleaner` is refused, whatever the binding is called.
const ALLOWED_CLEANER_CALLS: &[&str] = &[
    "acquire",
    "recover",
    "begin_operation",
    "execute_plan",
    "restore",
    "plan",
    "run",
    "with_quarantine",
    "with_trash",
    "with_elevation",
    "finish",
    "finish_measured",
    "audit_plan",
    "succeeded",
    "reason_code",
    "cache_bytes_freed",
    "total_reclaimable",
    // Generic or read-only names that `scull-cleaner` also uses (Vec, Mutex, constructors,
    // `Action::restorability`).
    "restorability",
    "new",
    "is_empty",
    "lock",
    "id",
];

/// The `scull_cleaner` items the module may import.
const ALLOWED_CLEANER_IMPORTS: &[&str] = &[
    "Executor",
    "ItemOutcome",
    "ItemReport",
    "LockOptions",
    "PlanContext",
    "QuarantineSettings",
    "RestoreOutcome",
    "Revalidation",
    "Ended",
    "QuickClean",
    "QuickCleanOptions",
    "QuickCleanRequest",
    "ToolResult",
    "TrashLocations",
];

/// The method names of the types that act on the disk (`Executor`, `Operation`, `QuickClean`):
/// every `fn` in their `impl` blocks in `scull-cleaner`'s sources, public or behind `bare_api!`.
fn cleaner_api_names() -> std::collections::BTreeSet<String> {
    let mut files = Vec::new();
    rust_sources(&repo_root().join("crates/scull-cleaner/src"), &mut files);
    let mut names = std::collections::BTreeSet::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for head in [
            "impl Executor {",
            "impl Operation<'_> {",
            "impl QuickClean {",
        ] {
            for (start, _) in text.match_indices(head) {
                // The block, by brace matching.
                let mut depth = 0i32;
                let mut end = text.len();
                for (k, c) in text[start..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = start + k;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let block = &text[start..end];
                for (i, _) in block.match_indices("fn ") {
                    let name: String = block[i + 3..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        names.insert(name);
                    }
                }
            }
        }
    }
    names
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The code of `f` without its test-only items. The file is parsed (`syn`) and exactly the
/// source text of each item whose `#[cfg(..)]` is `test` or `all(.., test, ..)` is cut, at any
/// depth (modules, impl and trait items). Everything else is checked, comments included: no
/// brace, indentation or formatting trick can hide production code, and `cfg(not(test))` code
/// is production code. A file that doesn't parse fails the check.
fn code_of(f: &Path) -> String {
    strip_test_items(&std::fs::read_to_string(f).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", f.display()))
}

/// Whether `attrs` make the item exist only in test builds.
fn test_only(attrs: &[syn::Attribute]) -> bool {
    use syn::punctuated::Punctuated;
    attrs.iter().any(|a| {
        if !a.path().is_ident("cfg") {
            return false;
        }
        match a.parse_args::<syn::Meta>() {
            Ok(syn::Meta::Path(p)) => p.is_ident("test"),
            Ok(syn::Meta::List(l)) if l.path.is_ident("all") => l
                .parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
                .is_ok_and(|m| {
                    m.iter()
                        .any(|m| matches!(m, syn::Meta::Path(p) if p.is_ident("test")))
                }),
            _ => false,
        }
    })
}

fn item_attrs(i: &syn::Item) -> &[syn::Attribute] {
    use syn::Item;
    match i {
        Item::Const(x) => &x.attrs,
        Item::Enum(x) => &x.attrs,
        Item::ExternCrate(x) => &x.attrs,
        Item::Fn(x) => &x.attrs,
        Item::ForeignMod(x) => &x.attrs,
        Item::Impl(x) => &x.attrs,
        Item::Macro(x) => &x.attrs,
        Item::Mod(x) => &x.attrs,
        Item::Static(x) => &x.attrs,
        Item::Struct(x) => &x.attrs,
        Item::Trait(x) => &x.attrs,
        Item::TraitAlias(x) => &x.attrs,
        Item::Type(x) => &x.attrs,
        Item::Union(x) => &x.attrs,
        Item::Use(x) => &x.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(i: &syn::ImplItem) -> &[syn::Attribute] {
    match i {
        syn::ImplItem::Const(x) => &x.attrs,
        syn::ImplItem::Fn(x) => &x.attrs,
        syn::ImplItem::Type(x) => &x.attrs,
        syn::ImplItem::Macro(x) => &x.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(i: &syn::TraitItem) -> &[syn::Attribute] {
    match i {
        syn::TraitItem::Const(x) => &x.attrs,
        syn::TraitItem::Fn(x) => &x.attrs,
        syn::TraitItem::Type(x) => &x.attrs,
        syn::TraitItem::Macro(x) => &x.attrs,
        _ => &[],
    }
}

type Extent = (proc_macro2::LineColumn, proc_macro2::LineColumn);

#[derive(Default)]
struct TestItems(Vec<Extent>);

impl TestItems {
    fn push(&mut self, tokens: proc_macro2::TokenStream) {
        let tokens: Vec<proc_macro2::TokenTree> = tokens.into_iter().collect();
        if let (Some(first), Some(last)) = (tokens.first(), tokens.last()) {
            self.0.push((first.span().start(), last.span().end()));
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for TestItems {
    fn visit_item(&mut self, i: &'ast syn::Item) {
        if test_only(item_attrs(i)) {
            self.push(quote::ToTokens::to_token_stream(i));
        } else {
            syn::visit::visit_item(self, i);
        }
    }
    fn visit_impl_item(&mut self, i: &'ast syn::ImplItem) {
        if test_only(impl_item_attrs(i)) {
            self.push(quote::ToTokens::to_token_stream(i));
        } else {
            syn::visit::visit_impl_item(self, i);
        }
    }
    fn visit_trait_item(&mut self, i: &'ast syn::TraitItem) {
        if test_only(trait_item_attrs(i)) {
            self.push(quote::ToTokens::to_token_stream(i));
        } else {
            syn::visit::visit_trait_item(self, i);
        }
    }
}

fn strip_test_items(text: &str) -> Result<String, String> {
    let file = syn::parse_file(text).map_err(|e| format!("does not parse: {e}"))?;
    let mut v = TestItems::default();
    syn::visit::Visit::visit_file(&mut v, &file);
    // Line/column (1-based line, column in chars) to a byte offset.
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let byte = |lc: proc_macro2::LineColumn| -> usize {
        let line_start = starts[lc.line - 1];
        text[line_start..]
            .char_indices()
            .nth(lc.column)
            .map_or(text.len(), |(i, _)| line_start + i)
    };
    let mut ranges: Vec<(usize, usize)> =
        v.0.into_iter().map(|(s, e)| (byte(s), byte(e))).collect();
    ranges.sort_unstable();
    let mut out = String::new();
    let mut at = 0;
    for (s, e) in ranges {
        if s >= at {
            out.push_str(&text[at..s]);
            at = e;
        }
    }
    out.push_str(&text[at..]);
    Ok(out)
}

#[test]
fn only_test_items_are_cut() {
    let cut = |t: &str| strip_test_items(t).unwrap();
    // A test module goes; code after it stays, whatever braces its strings hold.
    let c = cut(
        "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { remove_file(\"{\") }\n}\nfn b() { remove_tree(y) }\n",
    );
    assert!(
        c.contains("fn a()") && c.contains("remove_tree(y)") && !c.contains("remove_file"),
        "{c}"
    );
    // Unformatted, one line: only the empty test module goes.
    let c = cut("#[cfg(test)] mod tests {} pub fn evil() { remove_tree(r) } mod t2 {}\n");
    assert!(c.contains("remove_tree(r)"), "{c}");
    // A misleadingly indented closing brace: the sibling module is production code.
    let c = cut(
        "#[cfg(test)]\nmod tests {\n    fn t() {}\n    }\n    pub mod hidden {\n    pub fn evil() { remove_tree(r) }\n}\n",
    );
    assert!(c.contains("remove_tree(r)"), "{c}");
    // A file module and cfg(not(test)) code are production code.
    let c = cut(
        "#[cfg(test)]\nmod tests_x;\nfn evil() { remove_tree(r) }\n#[cfg(not(test))]\nfn p() { move_to_trash(x) }\n",
    );
    assert!(
        c.contains("remove_tree(r)") && c.contains("move_to_trash(x)"),
        "{c}"
    );
    // all(test, ..) items and test-only impl items go; the rest of the impl stays.
    let c = cut(
        "#[cfg(all(test, unix))]\nmod t { fn x() { remove_file(a) } }\nimpl S {\n    #[cfg(test)]\n    fn t() { remove_dir(b) }\n    fn p() { remove_tree(c) }\n}\n",
    );
    assert!(
        !c.contains("remove_file") && !c.contains("remove_dir") && c.contains("remove_tree(c)"),
        "{c}"
    );
    // Unicode before an item doesn't shift the cut.
    let c = cut(
        "const E: &str = \"é—é\";\n#[cfg(test)]\nmod tests { fn t() { remove_file(a) } }\nfn z() { remove_tree(d) }\n",
    );
    assert!(
        c.contains("const E") && !c.contains("remove_file") && c.contains("remove_tree(d)"),
        "{c}"
    );
    assert!(strip_test_items("fn broken( {").is_err());
}

#[test]
fn desktop_sources_have_no_filesystem_mutation() {
    let mut files = Vec::new();
    for d in [
        "bridge/src",
        "bridge/build.rs",
        "src-tauri/src",
        "src-tauri/build.rs",
    ] {
        let p = desktop().join(d);
        if p.is_file() {
            files.push(p);
        } else {
            rust_sources(&p, &mut files);
        }
    }
    assert!(files.len() >= 3, "found {files:?}");
    let cleanup = desktop().join(CLEANUP_MODULE);
    for f in files.iter().filter(|f| **f != cleanup) {
        let code = code_of(f);
        for api in MUTATION_APIS {
            assert!(!code.contains(api), "{} uses {api}", f.display());
        }
    }
}

/// Every desktop Rust source (bridge, Tauri app crate and its build script), cleanup.rs included.
fn desktop_rust_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for d in [
        "bridge/src",
        "bridge/build.rs",
        "src-tauri/src",
        "src-tauri/build.rs",
    ] {
        let p = desktop().join(d);
        if p.is_file() {
            files.push(p);
        } else {
            rust_sources(&p, &mut files);
        }
    }
    assert!(files.len() >= 3, "found {files:?}");
    files
}

/// The `scull-platform` modules behind its `delete` feature (`exec_lock`, `quarantine`,
/// `remove`, `trash`), read from its lib.rs.
fn platform_delete_modules() -> std::collections::BTreeSet<String> {
    let lib =
        std::fs::read_to_string(repo_root().join("crates/scull-platform/src/lib.rs")).unwrap();
    let lines: Vec<&str> = lib.lines().map(str::trim).collect();
    let mut mods = std::collections::BTreeSet::new();
    for w in lines.windows(2) {
        if w[0] == "#[cfg(feature = \"delete\")]"
            && let Some(m) = w[1].strip_prefix("pub mod ")
        {
            mods.insert(m.trim_end_matches(';').trim().to_string());
        }
    }
    assert!(
        mods.contains("remove") && mods.contains("trash") && mods.contains("quarantine"),
        "{mods:?}"
    );
    mods
}

/// The entry points of those modules that move or delete files or take the deletion lock: every
/// top-level `pub fn` in their sources, plus their request and lock types.
fn platform_delete_items(
    mods: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    let src = repo_root().join("crates/scull-platform/src");
    let mut files = Vec::new();
    for m in mods {
        let f = src.join(format!("{m}.rs"));
        if f.is_file() {
            files.push(f);
        }
        rust_sources(&src.join(m), &mut files);
    }
    let mut items: std::collections::BTreeSet<String> = [
        "RemoveRequest",
        "MoveRequest",
        "TrashRequest",
        "ExclusiveLock",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for f in files {
        for line in std::fs::read_to_string(&f).unwrap().lines() {
            if let Some(rest) = line.strip_prefix("pub fn ") {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                items.insert(name);
            }
        }
    }
    for must in [
        "remove_tree",
        "remove_tree_observed",
        "move_no_replace",
        "move_to_trash",
    ] {
        assert!(items.contains(must), "{must} not found in {items:?}");
    }
    items
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

/// Problems with how `code` names scull-platform's delete-gated modules (empty when none).
/// The cleaner turns on `scull-platform/delete`, and Cargo unifies features, so these modules
/// compile in every desktop crate: only this source guard keeps them out.
fn platform_delete_uses(
    code: &str,
    mods: &std::collections::BTreeSet<String>,
    items: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let mut bad = Vec::new();
    for w in words(code) {
        if items.contains(w) {
            bad.push(format!("names scull-platform's {w}"));
        }
    }
    for (i, _) in code.match_indices("scull_platform") {
        let rest = code[i + "scull_platform".len()..].trim_start();
        if rest.starts_with("as ") || rest.starts_with("as\n") || rest.starts_with("as\t") {
            bad.push("aliases scull_platform".into());
            continue;
        }
        let Some(path) = rest.strip_prefix("::") else {
            continue;
        };
        let path = path.trim_start();
        if path.starts_with('*') {
            bad.push("glob-imports scull_platform".into());
        } else if path.starts_with('{') {
            // A use group: everything up to its matching brace.
            let mut depth = 0i32;
            let mut end = path.len();
            for (k, c) in path.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = k;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let group = &path[..end];
            if group.contains('*') {
                bad.push("glob-imports from scull_platform".into());
            }
            for w in words(group) {
                if mods.contains(w) || w == "self" {
                    bad.push(format!("imports scull_platform::{w}"));
                }
            }
        } else {
            let seg: String = path
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if mods.contains(&seg) {
                bad.push(format!("uses scull_platform::{seg}"));
            }
        }
    }
    bad
}

#[test]
fn no_desktop_source_reaches_platform_deletion() {
    let mods = platform_delete_modules();
    let items = platform_delete_items(&mods);
    for f in desktop_rust_files() {
        let code = code_of(&f);
        let bad = platform_delete_uses(&code, &mods, &items);
        assert!(bad.is_empty(), "{}: {bad:?}", f.display());
        // Every desktop source is one of the files read here: no module or code pulled in
        // from elsewhere.
        for pulled in ["#[path", "include!"] {
            assert!(!code.contains(pulled), "{} uses {pulled}", f.display());
        }
    }
}

#[test]
fn platform_deletion_guard_sees_every_spelling() {
    let mods = platform_delete_modules();
    let items = platform_delete_items(&mods);
    for text in [
        "scull_platform::remove::remove_tree(r)",
        "use scull_platform::remove;",
        "use scull_platform::{remove::RemoveRequest};",
        "use scull_platform::{trash as t}; t::run(x)",
        "use scull_platform::{\n    volumes,\n    quarantine,\n};",
        "use scull_platform as p;",
        "extern crate scull_platform as p;",
        "use scull_platform::*;",
        "use scull_platform::{self as p};",
        "use scull_platform::{volumes, *};",
        "::scull_platform::exec_lock::lock_exclusive(d)",
        "let r = x::move_to_trash(p);",
        "use something::remove_tree_observed;",
        "fn f(_: MoveRequest) {}",
    ] {
        assert!(
            !platform_delete_uses(text, &mods, &items).is_empty(),
            "not caught: {text}"
        );
    }
    for text in [
        "use scull_platform::{Elevation, EntryKind, volumes};",
        "scull_platform::startup_env::remove_vars_before_threads()",
        "scull_platform::create_dir_owner_only(&dir)",
        "let trash = settings.trash_dir();",
    ] {
        assert_eq!(
            platform_delete_uses(text, &mods, &items),
            Vec::<String>::new(),
            "false positive: {text}"
        );
    }
}

#[test]
fn the_cleanup_module_deletes_only_through_the_executor() {
    let code = code_of(&desktop().join(CLEANUP_MODULE));
    for api in DIRECT_DELETION {
        assert!(!code.contains(api), "{CLEANUP_MODULE} uses {api}");
    }
    // Nothing of scull-cleaner is re-exported, and it is only named in `use` lines.
    for line in code.lines() {
        let t = line.trim_start();
        assert!(
            !(t.starts_with("pub use ")
                || t.starts_with("pub(crate) use ")
                || t.starts_with("pub(super) use ")),
            "{CLEANUP_MODULE} re-exports something: {line}"
        );
    }
    let mut imports = String::new();
    let mut in_use = false;
    for line in code.lines() {
        let t = line.trim_start();
        if t.starts_with("use scull_cleaner") {
            in_use = true;
        }
        if in_use {
            imports.push_str(t);
            imports.push(' ');
            in_use = !t.ends_with(';');
        } else {
            assert!(
                !line.contains("scull_cleaner::"),
                "{CLEANUP_MODULE} names scull_cleaner outside a use line: {line}"
            );
        }
    }
    assert!(
        !imports.contains('*'),
        "{CLEANUP_MODULE} glob-imports scull_cleaner: {imports}"
    );
    for word in imports
        .replace("use scull_cleaner", " ")
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| w.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
    {
        assert!(
            ALLOWED_CLEANER_IMPORTS.contains(&word),
            "{CLEANUP_MODULE} imports scull_cleaner::{word}"
        );
    }
    // Every call of a scull-cleaner method name, whatever the receiver, is an allowed one.
    let api = cleaner_api_names();
    assert!(
        api.contains("execute_plan") && api.contains("trash"),
        "{api:?}"
    );
    for (i, _) in code.match_indices('.') {
        let name: String = code[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let called = code[i + 1 + name.len()..].starts_with('(');
        if called && api.contains(&name) {
            assert!(
                ALLOWED_CLEANER_CALLS.contains(&name.as_str()),
                "{CLEANUP_MODULE} calls scull-cleaner's {name}"
            );
        }
    }
    // The only directory it creates is Quick Clean's private sandbox, owner-only, in scull's
    // own data directory.
    assert_eq!(
        code.matches("create_dir").count(),
        1,
        "one create_dir_owner_only"
    );
    assert!(code.contains("scull_platform::create_dir_owner_only(&dir)"));
    // Every operation it starts is recorded as the human's desktop confirmation.
    assert!(code.contains("method: ApprovalMethod::Desktop"));
    assert!(!code.contains("ApprovalMethod::Cli"));
}

#[test]
fn detector_sees_every_spelling() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    let ws = workspace_paths();
    for (text, cleaner, del) in [
        (
            "[dependencies]\nscull-cleaner.workspace = true\n",
            true,
            false,
        ),
        (
            "[target.'cfg(unix)'.dependencies]\nscull-cleaner = { path = \"x\" }\n",
            true,
            false,
        ),
        ("[dependencies.scull-cleaner]\npath = \"x\"\n", true, false),
        ("[build-dependencies]\nscull-cleaner = \"0\"\n", true, false),
        (
            "[dependencies]\nscull-platform = { workspace = true, features = [\"delete\"] }\n",
            false,
            true,
        ),
        (
            "[dependencies.scull-platform]\nworkspace = true\nfeatures = ['delete']\n",
            false,
            true,
        ),
        ("[features]\nx = [\"scull-platform/delete\"]\n", false, true),
        (
            "[dev-dependencies]\nscull-cleaner.workspace = true\n",
            false,
            false,
        ),
        (
            "[dependencies]\nscull-platform.workspace = true # features = [\"delete\"]\n",
            false,
            false,
        ),
        // Renamed: the package is what counts, not the key.
        (
            "[dependencies]\ndeleter = { package = \"scull-cleaner\", path = \"x\" }\n",
            true,
            false,
        ),
        (
            "[dependencies.deleter]\npath = \"x\"\npackage = \"scull-cleaner\"\n",
            true,
            false,
        ),
        (
            "[dependencies]\nplat = { package = \"scull-platform\", path = \"x\", features = [\"delete\"] }\n",
            false,
            true,
        ),
        (
            "[dependencies.plat]\nfeatures = [\"delete\"]\npackage = \"scull-platform\"\npath = \"x\"\n",
            false,
            true,
        ),
        (
            "[dependencies]\nplat = { package = \"scull-platform\", path = \"x\" }\n[features]\nx = [\"plat/delete\"]\n",
            false,
            true,
        ),
        // Only a different package under the cleaner's name is not the cleaner.
        (
            "[dependencies]\nscull-cleaner = { package = \"other\", path = \"x\" }\n",
            false,
            false,
        ),
    ] {
        std::fs::write(tmp.join("Cargo.toml"), text).unwrap();
        let m = read_manifest(tmp, &ws);
        assert_eq!(
            m.deps.iter().any(|(n, _)| n == "scull-cleaner"),
            cleaner,
            "{text}"
        );
        assert_eq!(m.enables_delete, del, "{text}");
    }
}
