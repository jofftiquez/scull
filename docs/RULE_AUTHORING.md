# Writing detector rules

A **detector rule** teaches scull to recognize one kind of regenerable folder: a build output,
a dependency folder, a tool's cache. A rule is a small TOML file. It holds data only, never
code or commands. This guide covers writing one, testing it, using it on your own machine,
and proposing it for the rules bundled with scull.

The full file format is in [`rules/README.md`](../rules/README.md). A complete example with
its tests is in [`docs/examples/rules/`](examples/rules/).

## The safety model in one paragraph

A rule only *suggests* that a folder is a candidate. scull's classifier still checks every
candidate (Git tracking, recent activity, protected paths, lockfiles, ...) and can only make
it **more** protected, never less. Rules you write yourself (*user* or *community* rules) are
untrusted:

* they load **disabled**, and run only after you list their IDs in your config;
* they are capped at **S1**, so the most they can do is move a folder to the restorable
  quarantine, never delete it outright;
* they can't name a cleanup command;
* the guardrails below keep them away from folders that hold your work.

Bundled rules are reviewed in this repository and ship with fixture tests.

## Guardrails the loader enforces

`scull rules validate` (and scull itself, when it loads your rules) rejects a community rule
file, with a reason code, when any of these holds:

| Code | Rule |
|---|---|
| `name_only` | No marker. A folder name alone is never enough (CLS-03). |
| `marker_matches_candidate`, `wildcard_marker` | A marker the candidate could satisfy itself (`../target` for a rule matching `target`), or a bare wildcard (`../*`). |
| `no_project_marker` | No marker **beside or above** the candidate (`../build.gradle`, `../../pnpm-workspace.yaml`). Community rules only apply inside a recognizable project. A marker inside the candidate is fine on top of one outside. |
| `user_work_folder` | A `match.names` pattern that matches a folder holding people's work, settings or secrets (`src`, `lib`, `data`, `docs`, `Documents`, `Desktop`, `Library`, `.git`, `.ssh`, ...). The full list is `PROTECTED_FOLDER_NAMES` in `crates/scull-detectors/src/rules/loader.rs`. The comparison ignores case, and a pattern that merely *can* match one (`S{rc,ource}`, `Doc*`) counts. |
| `wildcard_name` | A name pattern with fewer than two literal characters (`*`, `?*`, `x*`). |
| `s0_not_allowed` | `base_tier = "S0"`. Community rules are capped at S1. |
| `tool_native_not_allowed` | `strategy = "tool_native"`. |
| `command_field` | Any key that looks like a command, script or hook, anywhere in the file. |
| `forbidden_category` | `category = "scull_data"`. |
| `duplicate_id` | An ID that is already loaded (a community rule can't shadow a bundled one). |

Unknown fields, unsupported `schema_version`s, and malformed patterns or paths are rejected
too. One bad rule rejects the whole file.

## Write a rule

1. **Pick the folder and what proves it.** Find what the tool always leaves beside the folder
   (the project file: `project.godot`, `build.gradle`, `Cargo.toml`) and, if you can, what it
   writes inside (`CACHEDIR.TAG`, `.lock`).
2. **Choose a namespaced ID**: `community.<tool>-<folder>`, lowercase.
3. **Write the file.** Start from `docs/examples/rules/godot.toml`:

   ```toml
   schema_version = 1

   [[rule]]
   id = "community.godot-import"
   version = 1
   artifact = "Godot import cache"
   ecosystem = "godot"
   category = "project_artifact"
   platforms = ["macos", "linux", "windows"]
   base_tier = "S1"
   strategy = "filesystem"
   evidence = ["marker", "known_artifact"]

   [rule.match]
   names = [".godot"]

   [rule.markers]
   all = ["../project.godot"]

   [rule.exclude]
   within = ["addons"]
   ```

4. **Validate it**: `scull rules validate my-rules.toml`. It prints `ok` or the rejection with
   its code. `--json` gives the full report (`scull/rules-validate@1`).

## Test it

Every rule ships with test cases (CLS-04). Put them in a **cases file** beside the rule file:
`my-rules.toml` → `my-rules.cases.toml`. Each case is a small tree of names, the folder the
rule is asked about, and what you expect:

```toml
schema_version = 1

[[case]]
name = "import cache next to project.godot"
rule = "community.godot-import"
tree = ["game/project.godot", "game/.godot/imported/icon.svg-1234.ctex"]
candidate = "game/.godot"
expect = "match"

[[case]]
name = "a .godot folder without a project file is left alone"
rule = "community.godot-import"
tree = ["downloads/.godot/imported/x.ctex"]
candidate = "downloads/.godot"
expect = "missing_marker"
```

* `tree`: paths relative to an imaginary scan root, `/`-separated. A trailing `/` marks an
  empty folder, and every parent of a listed path is a folder.
* `candidate`: the folder the rule is asked about.
* `expect`: one of
  * `match`: the rule claims it;
  * `lockfile_missing`: it claims it, but the declared lockfile is absent, so the folder is
    reported at least S2;
  * `excluded`: an exclusion applies;
  * `missing_marker`: a required marker is absent;
  * `no_match`: any of the non-claiming outcomes, including a name that doesn't match.

Run the cases with `scull rules test my-rules.toml`:

```console
$ scull rules test docs/examples/rules/godot.toml
pass docs/examples/rules/godot.toml (docs/examples/rules/godot.cases.toml)
  ok   import cache next to project.godot: expected match, got match (../project.godot)
  ok   a .godot folder without a project file is left alone: expected missing_marker, got missing_marker (no `../project.godot`)
  ok   a plugin's own copy under addons/ is excluded: expected excluded, got excluded (excluded by `within addons`)
  ok   the project folder itself is never the candidate: expected no_match, got name_mismatch
4 of 4 case(s) passed
```

The harness:

* loads the rule file exactly as scull does, with the same loader and the same guardrails, so
  a rejected file never reaches its cases;
* evaluates each case with the rule's own marker logic on the tree **described in the file**,
  so nothing on disk is read, created or deleted;
* requires at least one case expecting a match and one expecting no match for every rule.

It exits 0 when everything passes and 2 otherwise. `--json` gives `scull/rules-test@1`.

`scull rules test --init my-rules.toml` writes a starter cases file, with one positive and one
negative case per rule built from its markers. It never overwrites an existing file.

Good cases cover the folder with its project files (`match`), without them
(`missing_marker`), with every exclusion you declared (`excluded`), and look-alike folders
elsewhere (`no_match`).

## Use it on your machine

1. Copy the rule file to the `rules/` folder in scull's config directory (`scull doctor`
   prints where that is).
2. Enable it in scull's config:

   ```toml
   [rules]
   enabled = ["community.godot-import"]
   ```

   or with `SCULL_RULES=community.godot-import`.
3. `scull rules list` shows it as `user`, enabled. `scull scan` now reports matching folders,
   never above S1, and every other check still applies.

## Propose it for the bundled set

Bundled rules can do more than community rules: S0 where regeneration is safe, a tool's own
cleanup command chosen in Rust, and self-identifying folders. They also carry more
responsibility. Open a **New detector rule** issue first (the template asks for the evidence
below), then a pull request that adds:

* the rule to `rules/ecosystems/<ecosystem>.toml` and that file to `BUNDLED_RULE_FILES` (a
  test fails if a rule file is not listed);
* fixture tests on real temp trees for what the classifier decides: detection, the tier, a
  negative case, and every exclusion (`docs/FIXTURES.md`; tree builders and invariant checks
  in `scull-fixtures`). `scull rules test` runs community rules only (it loads the file with
  the community guardrails), so draft and test the rule as a community rule first, then raise
  what only a bundled rule may do;
* the review checklist in the pull request template, filled in.

See [CONTRIBUTING.md](../CONTRIBUTING.md#contributing-a-detector-rule).
