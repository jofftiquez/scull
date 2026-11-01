//! Parsing npm-style `.cmd` shims (P2-QC-02, SAFE-12): which Node script a shim runs, so
//! Quick Clean can start `node <script> <args>` itself, never `cmd.exe`.
//!
//! On Windows, npm, pnpm and Yarn are usually installed as `.cmd` shims. Running one needs
//! `cmd.exe`, which Quick Clean never starts (no shell, SAFE-12). Every common shim only starts
//! Node on a JavaScript entry point next to it, so the shim is **read, never run**: this module
//! extracts the program (Node) and the script, and [`super::resolve`] verifies both paths with
//! the trusted-locations rules before anything starts.
//!
//! # Accepted formats
//!
//! The shim's final command must be exactly `<node> "<script>" %*`, where:
//!
//! * `<node>` is Node: `"%_prog%"` / `"%NODE_EXE%"` whose every assignment is the `node.exe`
//!   next to the shim or a bare `node` (cmd-shim, Node's bundled `npm.cmd`), `"%~dp0\node.exe"`
//!   / `"%dp0%\node.exe"` (old cmd-shim, guarded by `IF EXIST`), or a bare `node` (Yarn's
//!   installer: `PATH` only);
//! * `<script>` is `"%~dp0\<rel>"` / `"%dp0%\<rel>"`, directly or through one variable
//!   assigned that way (Node's bundled `npm.cmd`: `"%NPM_CLI_JS%"`), with `<rel>` a plain
//!   relative path strictly below the shim's folder (no `..`, no variables, no drive);
//! * every final command of the shim (old cmd-shim has two, one per branch) names the same
//!   script.
//!
//! Anything else (a shebang program other than Node, extra arguments, unknown variables,
//! `.bat`/`.ps1`) is not a shim Quick Clean can run, and the tool stays skipped. The shim is
//! never executed, so lines that only affect how `cmd.exe` would have found Node (`PATHEXT`,
//! `NODE_PATH`, the bundled `npm.cmd`'s prefix lookup) are ignored: scull picks Node itself.
//! In particular the bundled `npm.cmd` would prefer a newer npm installed under the global
//! prefix; scull runs the npm the shim ships with (any npm understands the cache commands).

/// Largest shim file read, bytes (real shims are well under 2 KiB).
pub const MAX_SHIM_BYTES: u64 = 16 * 1024;

/// Where the shim finds Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRef {
    /// `node.exe` next to the shim if it exists, otherwise `node` from the search path
    /// (cmd-shim and the bundled `npm.cmd`).
    AdjacentThenPath,
    /// `node` from the search path only (Yarn's installer shim).
    PathOnly,
}

/// What a shim runs: Node on a script below the shim's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimTarget {
    /// Where Node comes from.
    pub node: NodeRef,
    /// The script's path relative to the shim's folder, one entry per component.
    pub script: Vec<String>,
}

/// Why a shim is not one Quick Clean can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn no(why: impl Into<String>) -> Unsupported {
    Unsupported(why.into())
}

/// `%~dp0` / `%dp0%` (the shim's folder, with a trailing backslash), case-insensitively.
fn strip_dp0(s: &str) -> Option<&str> {
    let lower = s.to_ascii_lowercase();
    for p in ["%~dp0", "%dp0%"] {
        if lower.starts_with(p) {
            return Some(&s[p.len()..]);
        }
    }
    None
}

/// One `SET` assignment: `SET "NAME=VALUE"` or `SET NAME=VALUE` (name upper-cased).
fn assignment(line: &str) -> Option<(String, String)> {
    let rest = line
        .get(..4)
        .filter(|p| p.eq_ignore_ascii_case("set "))
        .map(|_| line[4..].trim())?;
    let rest = match rest.strip_prefix('"') {
        Some(r) => r.strip_suffix('"')?,
        None => rest,
    };
    let (name, value) = rest.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((name.to_ascii_uppercase(), value.to_owned()))
}

/// Splits a command into tokens: `"quoted"` (quotes removed) or bare words.
fn tokens(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&c) = chars.peek() else { break };
        let mut tok = String::new();
        if c == '"' {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some(c) => tok.push(c),
                    None => return None,
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                if c == '"' {
                    return None;
                }
                tok.push(c);
                chars.next();
            }
        }
        out.push(tok);
    }
    Some(out)
}

/// A variable reference `%NAME%` (name upper-cased).
fn variable(tok: &str) -> Option<String> {
    let name = tok.strip_prefix('%')?.strip_suffix('%')?;
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| name.to_ascii_uppercase())
}

/// Whether `value` names the `node.exe` next to the shim.
fn is_adjacent_node(value: &str) -> bool {
    strip_dp0(value).is_some_and(|rest| {
        rest.trim_start_matches('\\')
            .eq_ignore_ascii_case("node.exe")
    })
}

fn is_bare_node(value: &str) -> bool {
    value.eq_ignore_ascii_case("node") || value.eq_ignore_ascii_case("node.exe")
}

/// The program token of a final command.
fn program(tok: &str, sets: &[(String, String)]) -> Result<NodeRef, Unsupported> {
    if let Some(name) = variable(tok) {
        let values: Vec<&str> = sets
            .iter()
            .filter(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        if values.is_empty() {
            return Err(no(format!("the program variable %{name}% is never set")));
        }
        if values
            .iter()
            .all(|v| is_adjacent_node(v) || is_bare_node(v))
        {
            return Ok(NodeRef::AdjacentThenPath);
        }
        return Err(no(format!("%{name}% is not Node")));
    }
    if is_adjacent_node(tok) {
        return Ok(NodeRef::AdjacentThenPath);
    }
    if is_bare_node(tok) {
        return Ok(NodeRef::PathOnly);
    }
    Err(no("the shim does not start Node"))
}

/// The script token of a final command, as path components below the shim's folder.
fn script(tok: &str, sets: &[(String, String)]) -> Result<Vec<String>, Unsupported> {
    let value = match variable(tok) {
        Some(name) => sets
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
            .ok_or_else(|| no(format!("the script variable %{name}% is never set")))?,
        None => tok,
    };
    let rest = strip_dp0(value).ok_or_else(|| no("the script is not next to the shim"))?;
    let rest = rest.trim_start_matches('\\');
    let parts: Vec<String> = rest.split(['\\', '/']).map(str::to_owned).collect();
    let bad = |p: &str| {
        p.is_empty()
            || p == "."
            || p == ".."
            || p.chars()
                .any(|c| c.is_control() || "%!\"*?<>|:".contains(c))
    };
    if parts.iter().any(|p| bad(p)) {
        return Err(no(format!("unsupported script path `{rest}`")));
    }
    let name = parts
        .last()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if ![".js", ".cjs", ".mjs"].iter().any(|e| name.ends_with(e)) {
        return Err(no(format!("`{rest}` is not a JavaScript file")));
    }
    Ok(parts)
}

/// Parses the text of a `.cmd` shim (see the module docs for what is accepted).
pub fn parse(text: &str) -> Result<ShimTarget, Unsupported> {
    let mut sets = Vec::new();
    let mut commands = Vec::new();
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('@').trim();
        let lower = line.to_ascii_lowercase();
        if line.is_empty() || line.starts_with("::") || lower == "rem" || lower.starts_with("rem ")
        {
            continue;
        }
        if let Some(a) = assignment(line) {
            sets.push(a);
            continue;
        }
        if let Some(cmd) = line.strip_suffix("%*") {
            // cmd-shim's last line: `endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% &
            // "%_prog%"  "%dp0%\x.js" %*`: the command is what follows the last `&`.
            let cmd = cmd.rsplit_once('&').map_or(cmd, |(_, c)| c);
            commands.push(cmd.trim().to_owned());
        }
    }
    if commands.is_empty() {
        return Err(no("no command passing the arguments on (`%*`)"));
    }
    let mut found: Option<ShimTarget> = None;
    for cmd in &commands {
        let toks = tokens(cmd).ok_or_else(|| no("unbalanced quotes"))?;
        let [prog, scr] = toks.as_slice() else {
            return Err(no(format!(
                "expected `<node> \"<script>\" %*`, found `{cmd} %*`"
            )));
        };
        let target = ShimTarget {
            node: program(prog, &sets)?,
            script: script(scr, &sets)?,
        };
        match &mut found {
            None => found = Some(target),
            Some(f) if f.script != target.script => {
                return Err(no("the shim's commands run different scripts"));
            }
            // Old cmd-shim: `"%~dp0\node.exe" ...` if it exists, else `node ...`.
            Some(f) => {
                if f.node != target.node {
                    f.node = NodeRef::AdjacentThenPath;
                }
            }
        }
    }
    found.ok_or_else(|| no("no command"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(s: &str) -> Vec<String> {
        s.split('/').map(str::to_owned).collect()
    }

    /// cmd-shim 4.x+ (npm 7+, `npm i -g pnpm`, corepack), CRLF line endings.
    const CMD_SHIM_NEW: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\pnpm\\bin\\pnpm.cjs\" %*\r\n";

    /// cmd-shim 2.x/3.x (npm 6, older global installs).
    const CMD_SHIM_OLD: &str = "@IF EXIST \"%~dp0\\node.exe\" (\n  \"%~dp0\\node.exe\"  \"%~dp0\\node_modules\\yarn\\bin\\yarn.js\" %*\n) ELSE (\n  @SETLOCAL\n  @SET PATHEXT=%PATHEXT:;.JS;=;%\n  node  \"%~dp0\\node_modules\\yarn\\bin\\yarn.js\" %*\n)\n";

    /// Node's bundled `npm.cmd` (Node 18-22, npm 9-10).
    const NODE_NPM_CMD: &str = ":: Created by npm, please don't edit manually.\r\n@ECHO OFF\r\n\r\nSETLOCAL\r\n\r\nSET \"NODE_EXE=%~dp0\\node.exe\"\r\nIF NOT EXIST \"%NODE_EXE%\" (\r\n  SET \"NODE_EXE=node\"\r\n)\r\n\r\nSET \"NPM_PREFIX_JS=%~dp0\\node_modules\\npm\\bin\\npm-prefix.js\"\r\nSET \"NPM_CLI_JS=%~dp0\\node_modules\\npm\\bin\\npm-cli.js\"\r\nSET \"NPM_PREFIX_NPM_CLI_JS=\"\r\nFOR /F \"delims=\" %%F IN ('CALL \"%NODE_EXE%\" \"%NPM_PREFIX_JS%\"') DO (\r\n  SET \"NPM_PREFIX_NPM_CLI_JS=%%F\\node_modules\\npm\\bin\\npm-cli.js\"\r\n)\r\nIF EXIST \"%NPM_PREFIX_NPM_CLI_JS%\" (\r\n  SET \"NPM_CLI_JS=%NPM_PREFIX_NPM_CLI_JS%\"\r\n)\r\n\r\n\"%NODE_EXE%\" \"%NPM_CLI_JS%\" %*\r\n";

    /// Yarn's MSI installer (`Yarn\bin\yarn.cmd`).
    const YARN_MSI: &str = "@echo off\r\nnode \"%~dp0\\yarn.js\" %*\r\n";

    /// pnpm's own `@zkochan/cmd-shim` (global bins), with `NODE_PATH`.
    const PNPM_ZKOCHAN: &str = "@SETLOCAL\r\n@SET \"NODE_PATH=%~dp0\\global\\5\\node_modules\"\r\n@IF EXIST \"%~dp0\\node.exe\" (\r\n  \"%~dp0\\node.exe\"  \"%~dp0\\global\\5\\node_modules\\pnpm\\bin\\pnpm.cjs\" %*\r\n) ELSE (\r\n  @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n  node  \"%~dp0\\global\\5\\node_modules\\pnpm\\bin\\pnpm.cjs\" %*\r\n)\r\n";

    #[test]
    fn real_world_shims_resolve_to_node_and_their_script() {
        for (name, text, node, script) in [
            (
                "cmd-shim new",
                CMD_SHIM_NEW,
                NodeRef::AdjacentThenPath,
                "node_modules/pnpm/bin/pnpm.cjs",
            ),
            (
                "cmd-shim old",
                CMD_SHIM_OLD,
                NodeRef::AdjacentThenPath,
                "node_modules/yarn/bin/yarn.js",
            ),
            (
                "node npm.cmd",
                NODE_NPM_CMD,
                NodeRef::AdjacentThenPath,
                "node_modules/npm/bin/npm-cli.js",
            ),
            ("yarn msi", YARN_MSI, NodeRef::PathOnly, "yarn.js"),
            (
                "pnpm zkochan",
                PNPM_ZKOCHAN,
                NodeRef::AdjacentThenPath,
                "global/5/node_modules/pnpm/bin/pnpm.cjs",
            ),
        ] {
            assert_eq!(
                parse(text),
                Ok(ShimTarget {
                    node,
                    script: parts(script)
                }),
                "{name}"
            );
        }
    }

    #[test]
    fn anything_but_node_on_a_script_below_the_shim_is_refused() {
        let cases = [
            // A shebang program other than Node (cmd-shim on a `#!/bin/sh` script).
            CMD_SHIM_NEW.replace("SET \"_prog=node\"", "SET \"_prog=sh\""),
            // Extra arguments before the script.
            CMD_SHIM_NEW.replace("\"%_prog%\"  ", "\"%_prog%\" --inspect "),
            // A script outside the shim's folder.
            YARN_MSI.replace("%~dp0\\yarn.js", "%~dp0\\..\\evil\\yarn.js"),
            YARN_MSI.replace("%~dp0\\yarn.js", "C:\\evil\\yarn.js"),
            YARN_MSI.replace("%~dp0\\yarn.js", "%TEMP%\\yarn.js"),
            YARN_MSI.replace("%~dp0\\yarn.js", "%~dp0\\%X%\\yarn.js"),
            // Not JavaScript.
            YARN_MSI.replace("yarn.js", "yarn.exe"),
            // Another program.
            YARN_MSI.replace("node ", "powershell "),
            "@echo off\r\ncmd /c \"%~dp0\\x.js\" %*\r\n".to_owned(),
            // Two branches, two scripts.
            CMD_SHIM_OLD.replacen("yarn.js", "other.js", 1),
            // An unset variable.
            "\"%NODE_EXE%\" \"%~dp0\\a.js\" %*\r\n".to_owned(),
            NODE_NPM_CMD.replace(
                "SET \"NPM_CLI_JS=%~dp0\\node_modules\\npm\\bin\\npm-cli.js\"\r\n",
                "",
            ),
            // No `%*`, unbalanced quotes, empty.
            "node \"%~dp0\\a.js\"\r\n".to_owned(),
            "node \"%~dp0\\a.js %*\r\n".to_owned(),
            String::new(),
        ];
        for text in &cases {
            assert!(
                parse(text).is_err(),
                "accepted: {text:?} -> {:?}",
                parse(text)
            );
        }
    }

    #[test]
    fn assignments_and_tokens() {
        assert_eq!(
            assignment(r#"SET "_prog=%dp0%\node.exe""#),
            Some(("_PROG".into(), r"%dp0%\node.exe".into()))
        );
        assert_eq!(
            assignment("set PATHEXT=%PATHEXT:;.JS;=;%").map(|a| a.0),
            Some("PATHEXT".into())
        );
        assert_eq!(assignment("SETLOCAL"), None);
        assert_eq!(
            tokens(r#""%_prog%"  "%dp0%\a b.js""#),
            Some(vec!["%_prog%".to_owned(), r"%dp0%\a b.js".to_owned()])
        );
        assert_eq!(tokens(r#"a"b"#), None);
    }
}
