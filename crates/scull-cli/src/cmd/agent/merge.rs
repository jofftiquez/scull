//! Adding and removing scull's entry in client config files without disturbing anything else.
//!
//! * **JSON** (`mcpServers` / `servers`): the top-level object and the servers object are read
//!   as ordered lists of `(key, raw JSON text)`, so every other entry is written back
//!   byte-for-byte, in its original order, with the file's indent and line endings. Only the
//!   `scull` entry is rendered by scull. Comments and trailing commas (JSONC) are not supported:
//!   such a file is refused and the user is shown the snippet to add by hand.
//! * **TOML** (Codex): `toml_edit` keeps comments, order and formatting; only
//!   `[mcp_servers.scull]` is touched.
//! * **Markdown guidance**: a block between `<!-- scull:begin -->` and `<!-- scull:end -->`,
//!   appended or replaced in place; uninstall removes exactly that block.
//!
//! Re-installing over an existing `scull` entry keeps the user's extra keys (e.g. `env`) but
//! always resets `command` and `args` to the read-only `mcp serve`, so a hand-added
//! `--allow-destructive` is never carried forward.

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use std::fmt;

/// The server name scull registers under.
pub const SERVER_NAME: &str = "scull";
/// The arguments every client runs: the read-only server. Never `--allow-destructive`.
pub const SERVE_ARGS: [&str; 2] = ["mcp", "serve"];

/// First line of scull's guidance block.
pub const BEGIN: &str = "<!-- scull:begin (managed by `scull agent install`) -->";
/// Prefix used to find the block (the rest of the line may change between versions).
const BEGIN_PREFIX: &str = "<!-- scull:begin";
/// Last line of scull's guidance block.
pub const END: &str = "<!-- scull:end -->";

/// Why a file could not be merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeError(pub String);

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A JSON object read as ordered `(key, raw text)` pairs.
struct Ordered(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut v = Vec::new();
                while let Some(e) = map.next_entry::<String, Box<RawValue>>()? {
                    v.push(e);
                }
                Ok(Ordered(v))
            }
        }
        d.deserialize_map(V)
    }
}

/// Indent unit and line ending of an existing JSON document (defaults: two spaces, `\n`).
struct Style {
    indent: String,
    nl: &'static str,
}

impl Style {
    fn of(text: &str) -> Self {
        let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let indent = text
            .lines()
            .skip_while(|l| !l.trim_start().starts_with('{'))
            .skip(1)
            .find(|l| l.trim_start().starts_with('"'))
            .map(|l| l[..l.len() - l.trim_start().len()].to_owned())
            .filter(|i| !i.is_empty())
            .unwrap_or_else(|| "  ".to_owned());
        Style { indent, nl }
    }

    fn object(&self, entries: &[(String, String)], level: usize) -> String {
        if entries.is_empty() {
            return "{}".to_owned();
        }
        let inner = self.indent.repeat(level + 1);
        let body: Vec<String> = entries
            .iter()
            .map(|(k, v)| format!("{inner}{}: {v}", json_string(k)))
            .collect();
        format!(
            "{{{nl}{}{nl}{}}}",
            body.join(&format!(",{}", self.nl)),
            self.indent.repeat(level),
            nl = self.nl
        )
    }

    /// `v` pretty-printed with this indent, positioned at nesting `level`. Arrays of scalars
    /// stay on one line (`["mcp", "serve"]`).
    fn value(&self, v: &Value, level: usize) -> String {
        if let Value::Array(items) = v
            && items.iter().all(|i| !i.is_array() && !i.is_object())
        {
            let parts: Vec<String> = items.iter().map(Value::to_string).collect();
            return format!("[{}]", parts.join(", "));
        }
        let mut buf = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(self.indent.as_bytes());
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
        if v.serialize(&mut ser).is_err() {
            return "null".to_owned();
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        text.replace('\n', &format!("{}{}", self.nl, self.indent.repeat(level)))
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

fn parse_object(text: &str, what: &str) -> Result<Ordered, MergeError> {
    serde_json::from_str::<Ordered>(text).map_err(|e| {
        MergeError(format!(
            "{what} is not a plain JSON object ({e}); comments and trailing commas are not \
             supported, so add the entry by hand"
        ))
    })
}

/// scull's JSON entry, in the order it is written.
pub type Entry = Vec<(&'static str, Value)>;

/// The JSON entry for scull: (for `typed` clients) `"type": "stdio"`, `command`, `args`.
pub fn json_entry(command: &str, typed: bool) -> Entry {
    let mut e: Entry = Vec::new();
    if typed {
        e.push(("type", "stdio".into()));
    }
    e.push(("command", command.into()));
    e.push((
        "args",
        Value::Array(SERVE_ARGS.iter().map(|a| Value::from(*a)).collect()),
    ));
    e
}

/// `old` (`None`: no file) with scull's entry added under `key`, or `old` itself when the entry
/// is already exactly right.
pub fn json_install(old: Option<&str>, key: &str, entry: &Entry) -> Result<String, MergeError> {
    let text = old.unwrap_or("");
    let style = Style::of(text);
    let mut top = if text.trim().is_empty() {
        Vec::new()
    } else {
        parse_object(text, "the file")?.0
    };
    let pos = top.iter().position(|(k, _)| k == key);
    let mut servers = match pos {
        Some(i) => parse_object(top[i].1.get(), &format!("`{key}`"))?.0,
        None => Vec::new(),
    };
    let existing = servers.iter().position(|(k, _)| k == SERVER_NAME);
    // The existing entry's keys keep their order and text; scull's keys are set or appended.
    let mut fields: Vec<(String, String)> = match existing {
        Some(i) => serde_json::from_str::<Ordered>(servers[i].1.get())
            .map(|o| {
                o.0.into_iter()
                    .map(|(k, v)| (k, v.get().to_owned()))
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let mut changed = existing.is_none();
    for (k, want) in entry {
        match fields.iter_mut().find(|(fk, _)| fk == k) {
            Some((_, raw)) => {
                if serde_json::from_str::<Value>(raw).ok().as_ref() != Some(want) {
                    *raw = style.value(want, 3);
                    changed = true;
                }
            }
            None => {
                fields.push(((*k).to_owned(), style.value(want, 3)));
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(text.to_owned());
    }
    let rendered = style.object(&fields, 2);
    let mut server_entries: Vec<(String, String)> = servers
        .drain(..)
        .map(|(k, v)| (k, v.get().to_owned()))
        .collect();
    match existing {
        Some(i) => server_entries[i].1 = rendered,
        None => server_entries.push((SERVER_NAME.to_owned(), rendered)),
    }
    let servers_text = style.object(&server_entries, 1);
    let mut top_entries: Vec<(String, String)> = top
        .drain(..)
        .map(|(k, v)| (k, v.get().to_owned()))
        .collect();
    match pos {
        Some(i) => top_entries[i].1 = servers_text,
        None => top_entries.push((key.to_owned(), servers_text)),
    }
    Ok(format!("{}{}", style.object(&top_entries, 0), style.nl))
}

/// `old` without scull's entry under `key`, or `None` when there is no such entry.
pub fn json_uninstall(old: &str, key: &str) -> Result<Option<String>, MergeError> {
    if old.trim().is_empty() {
        return Ok(None);
    }
    let style = Style::of(old);
    let top = parse_object(old, "the file")?.0;
    let Some(pos) = top.iter().position(|(k, _)| k == key) else {
        return Ok(None);
    };
    let servers = parse_object(top[pos].1.get(), &format!("`{key}`"))?.0;
    if !servers.iter().any(|(k, _)| k == SERVER_NAME) {
        return Ok(None);
    }
    let kept: Vec<(String, String)> = servers
        .iter()
        .filter(|(k, _)| k != SERVER_NAME)
        .map(|(k, v)| (k.clone(), v.get().to_owned()))
        .collect();
    let mut top_entries: Vec<(String, String)> = top
        .iter()
        .map(|(k, v)| (k.clone(), v.get().to_owned()))
        .collect();
    top_entries[pos].1 = style.object(&kept, 1);
    Ok(Some(format!(
        "{}{}",
        style.object(&top_entries, 0),
        style.nl
    )))
}

fn toml_doc(old: &str) -> Result<toml_edit::DocumentMut, MergeError> {
    old.parse::<toml_edit::DocumentMut>().map_err(|e| {
        MergeError(format!(
            "the file is not valid TOML ({}); fix it or add the entry by hand",
            super::super::one_line(&e.to_string())
        ))
    })
}

/// Codex's `config.toml` with `[mcp_servers.scull]` added or corrected.
pub fn toml_install(old: Option<&str>, command: &str) -> Result<String, MergeError> {
    use toml_edit::{Array, Item, Table, value};
    let text = old.unwrap_or("");
    let mut doc = toml_doc(text)?;
    let servers = doc.as_table_mut().entry("mcp_servers").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    let servers = servers
        .as_table_like_mut()
        .ok_or_else(|| MergeError("`mcp_servers` is not a table".into()))?;
    let args: Array = SERVE_ARGS.iter().copied().collect();
    match servers
        .get_mut(SERVER_NAME)
        .and_then(Item::as_table_like_mut)
    {
        Some(entry) => {
            let same_cmd = entry.get("command").and_then(Item::as_str) == Some(command);
            let same_args = entry
                .get("args")
                .and_then(Item::as_array)
                .is_some_and(|a| a.iter().map(|v| v.as_str()).eq(SERVE_ARGS.map(Some)));
            if same_cmd && same_args {
                return Ok(text.to_owned());
            }
            if !same_cmd {
                entry.insert("command", value(command));
            }
            if !same_args {
                entry.insert("args", value(args));
            }
        }
        None => {
            let mut t = Table::new();
            t.insert("command", value(command));
            t.insert("args", value(args));
            servers.insert(SERVER_NAME, Item::Table(t));
        }
    }
    let mut out = doc.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Codex's `config.toml` without `[mcp_servers.scull]`, or `None` when it has none.
pub fn toml_uninstall(old: &str) -> Result<Option<String>, MergeError> {
    let mut doc = toml_doc(old)?;
    let Some(servers) = doc
        .as_table_mut()
        .get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_table_like_mut)
    else {
        return Ok(None);
    };
    if servers.remove(SERVER_NAME).is_none() {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// scull's own config with `[mcp] enabled = true`, or `old` when it is already set.
pub fn scull_enable_mcp(old: Option<&str>) -> Result<String, MergeError> {
    use toml_edit::{Item, Table, value};
    let text = old.unwrap_or("");
    let mut doc = toml_doc(text)?;
    let mcp = doc
        .as_table_mut()
        .entry("mcp")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| MergeError("`mcp` in scull's config is not a table".into()))?;
    if mcp.get("enabled").and_then(Item::as_bool) == Some(true) {
        return Ok(text.to_owned());
    }
    mcp.insert("enabled", value(true));
    let mut out = doc.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Byte range of scull's block (from the start of the begin line to after the end line).
fn block_range(text: &str) -> Option<(usize, usize)> {
    let start = text
        .match_indices(BEGIN_PREFIX)
        .map(|(i, _)| i)
        .find(|&i| i == 0 || text[..i].ends_with('\n'))?;
    let end_rel = text[start..].find(END)?;
    let mut end = start + end_rel + END.len();
    if text[end..].starts_with("\r\n") {
        end += 2;
    } else if text[end..].starts_with('\n') {
        end += 1;
    }
    Some((start, end))
}

/// The guidance block for `body`.
pub fn guidance_block(body: &str) -> String {
    format!("{BEGIN}\n{}\n{END}\n", body.trim_end())
}

/// `old` with scull's guidance block added, or replaced if one is there.
pub fn md_install(old: Option<&str>, body: &str) -> String {
    let block = guidance_block(body);
    let text = old.unwrap_or("");
    if let Some((s, e)) = block_range(text) {
        return format!("{}{block}{}", &text[..s], &text[e..]);
    }
    if text.is_empty() {
        return block;
    }
    let sep = if text.ends_with("\n\n") {
        ""
    } else if text.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{text}{sep}{block}")
}

/// `old` without scull's guidance block, or `None` when it has none.
pub fn md_uninstall(old: &str) -> Option<String> {
    let (s, e) = block_range(old)?;
    let mut head = &old[..s];
    // Drop the blank line install added before the block.
    if e == old.len() && head.ends_with("\n\n") {
        head = &head[..head.len() - 1];
    }
    Some(format!("{head}{}", &old[e..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        json_entry("scull", false)
    }

    #[test]
    fn json_new_file() {
        let out = json_install(None, "mcpServers", &entry()).unwrap();
        assert_eq!(
            out,
            "{\n  \"mcpServers\": {\n    \"scull\": {\n      \"command\": \"scull\",\n      \"args\": [\"mcp\", \"serve\"]\n    }\n  }\n}\n"
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["scull"]["command"], "scull");
    }

    #[test]
    fn json_keeps_other_entries_verbatim_and_in_order() {
        let old = "{\n  \"zeta\": 1,\n  \"mcpServers\": {\n    \"other\": {\"command\": \"x\",   \"args\": []}\n  },\n  \"alpha\": [1, 2.50, 1e3]\n}\n";
        let out = json_install(Some(old), "mcpServers", &entry()).unwrap();
        assert!(out.starts_with("{\n  \"zeta\": 1,\n  \"mcpServers\": {\n    \"other\": {\"command\": \"x\",   \"args\": []},\n    \"scull\": {"), "{out}");
        assert!(out.ends_with("  \"alpha\": [1, 2.50, 1e3]\n}\n"), "{out}");
        // Idempotent.
        assert_eq!(
            json_install(Some(&out), "mcpServers", &entry()).unwrap(),
            out
        );
        // Uninstall restores the other server exactly.
        let back = json_uninstall(&out, "mcpServers").unwrap().unwrap();
        assert_eq!(back, old);
        assert_eq!(json_uninstall(&back, "mcpServers").unwrap(), None);
    }

    #[test]
    fn json_reinstall_resets_args_and_keeps_extra_keys() {
        let old = r#"{"mcpServers": {"scull": {"command": "scull", "args": ["mcp", "serve", "--allow-destructive"], "env": {"A": "1"}}}}"#;
        let out = json_install(Some(old), "mcpServers", &entry()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["mcpServers"]["scull"]["args"],
            serde_json::json!(["mcp", "serve"])
        );
        assert_eq!(v["mcpServers"]["scull"]["env"]["A"], "1");
        assert!(!out.contains("allow-destructive"));
    }

    #[test]
    fn json_indent_and_crlf_are_kept() {
        let old = "{\r\n    \"a\": true\r\n}\r\n";
        let out = json_install(Some(old), "servers", &json_entry("scull", true)).unwrap();
        assert!(out.starts_with("{\r\n    \"a\": true,\r\n    \"servers\": {\r\n        \"scull\": {\r\n            \"type\""), "{out:?}");
        assert!(!out.replace("\r\n", "").contains('\n'), "{out:?}");
        assert!(out.contains("\"type\": \"stdio\""));
    }

    #[test]
    fn jsonc_and_non_objects_are_refused() {
        for bad in [
            "{ // comment\n \"a\": 1 }",
            "{\"a\": 1,}",
            "[1]",
            "{\"mcpServers\": []}",
        ] {
            assert!(
                json_install(Some(bad), "mcpServers", &entry()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn toml_keeps_comments_and_other_servers() {
        let old =
            "# my codex config\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\" # keep\n";
        let out = toml_install(Some(old), "scull").unwrap();
        assert!(out.starts_with(old), "{out}");
        assert!(
            out.contains("[mcp_servers.scull]\ncommand = \"scull\"\nargs = [\"mcp\", \"serve\"]\n"),
            "{out}"
        );
        assert_eq!(toml_install(Some(&out), "scull").unwrap(), out);
        let back = toml_uninstall(&out).unwrap().unwrap();
        assert_eq!(back, old);
        assert_eq!(toml_uninstall(&back).unwrap(), None);
        let fresh = toml_install(None, "/usr/bin/scull").unwrap();
        assert_eq!(
            fresh,
            "[mcp_servers.scull]\ncommand = \"/usr/bin/scull\"\nargs = [\"mcp\", \"serve\"]\n"
        );
    }

    #[test]
    fn toml_reinstall_resets_args() {
        let old = "[mcp_servers.scull]\ncommand = \"scull\"\nargs = [\"mcp\", \"serve\", \"--allow-destructive\"]\nenv = { A = \"1\" }\n";
        let out = toml_install(Some(old), "scull").unwrap();
        assert!(!out.contains("allow-destructive"), "{out}");
        assert!(out.contains("env = { A = \"1\" }"), "{out}");
        assert!(toml_install(Some("not = [toml"), "scull").is_err());
    }

    #[test]
    fn scull_config_enable() {
        let old = "# mine\n[scan]\nroots = [\"/p\"]\n";
        let out = scull_enable_mcp(Some(old)).unwrap();
        assert!(out.starts_with(old));
        assert!(out.contains("[mcp]\nenabled = true\n"), "{out}");
        assert_eq!(scull_enable_mcp(Some(&out)).unwrap(), out);
        let off = "[mcp]\nenabled = false # was off\n";
        assert!(
            scull_enable_mcp(Some(off))
                .unwrap()
                .contains("enabled = true")
        );
    }

    #[test]
    fn markdown_block_round_trip() {
        let body = "## scull\nUse it.";
        assert_eq!(md_install(None, body), guidance_block(body));
        let old = "# My rules\n\nBe nice.\n";
        let out = md_install(Some(old), body);
        assert_eq!(out, format!("{old}\n{}", guidance_block(body)));
        // Replaced in place, not duplicated.
        let again = md_install(Some(&out), "## scull\nUse it v2.");
        assert_eq!(again.matches(BEGIN_PREFIX).count(), 1);
        assert!(again.contains("v2"));
        assert_eq!(md_uninstall(&again).unwrap(), old);
        assert_eq!(md_uninstall(old), None);
        // A block in the middle keeps what follows.
        let mid = format!("a\n{}b\n", guidance_block(body));
        assert_eq!(md_uninstall(&mid).unwrap(), "a\nb\n");
    }
}
