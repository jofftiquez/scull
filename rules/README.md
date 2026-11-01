# Detector rules

Writing your own rule, testing it with `scull rules test`, and proposing it: see
[docs/RULE_AUTHORING.md](../docs/RULE_AUTHORING.md).

Declarative, **data-only** rules for simple detection cases (PRD §8.1, CLS-01…05). Complex cases
(Git worktrees, the pnpm store, shared `CARGO_TARGET_DIR`, package caches) are Rust detectors
in `crates/scull-detectors/`, not rules (CLS-02).

- Loader: `scull_detectors::rules` (`parse_rules`, `RuleSet`, `Rule::check_markers`).
- Detection: `scull_detectors::Detectors::bundled()` turns every enabled rule into a
  `RuleDetector`, which lists the directory and the ancestor levels the rule needs through
  `scull-platform` (no-follow), runs `check_markers` and returns a `DetectedArtifact`. Shared
  Cargo target directories: `scull_detectors::cargo::SharedTargetDirs` (`rust.shared-target`).
  The `node.*` rules are run by `scull_detectors::node` detectors, which add workspace lockfile
  resolution, link recording in `node_modules`, and a package.json dependency alternative to the
  config-file markers of framework outputs (see the header of `ecosystems/node.toml`).
  The `python.venv` and `python.pycache` rules are run by `scull_detectors::python` detectors,
  which accept a fully pinned `requirements*.txt` as a virtual environment's lockfile and
  require the source of every compiled module beside a `__pycache__` (see the header of
  `ecosystems/python.toml`).
- ML caches (`ecosystems/ml.toml`: Hugging Face hub repositories and processed datasets, Ollama
  models, PyTorch Hub) are run by `scull_detectors::ml::MlDetector`. It records no project,
  requires the usual parent folder for generic names (`datasets`, `hub`), and turns the installed
  Ollama models into the `ollama rm` command, chosen in Rust (CLS-04). Roots moved by environment
  variables (`HF_HOME`, `HF_HUB_CACHE`, `HF_DATASETS_CACHE`, `OLLAMA_MODELS`, `TORCH_HOME`,
  `XDG_CACHE_HOME`) are found per OS from an injected `CacheEnv` by
  `scull_detectors::ml::locate_ml_caches`.
- Java/Kotlin (`ecosystems/jvm.toml`: Gradle `build/` and `.gradle/`, Maven `target/`) and .NET
  (`ecosystems/dotnet.toml`: `bin/`, `obj/`) build folders need the build or project file beside
  them **and** entries only the tool writes inside them; `build/` next to another build system's
  file is left alone (FX-CLS-10), and so are `bin/`/`obj/` in Unity and Unreal projects
  (FX-CLS-14). `scull_detectors::dotnet::DotnetBinDetector` raises a `bin/` holding anything but
  configuration folders to S2. Go has no project build folder (its outputs go to the build cache).
- Xcode (`ecosystems/xcode.toml`, macOS only: per-project DerivedData, device support files,
  archives, the simulator device set) and Android (`ecosystems/android.toml`: Android Gradle
  Plugin `build/`, emulator `.avd` folders) are run by `scull_detectors::xcode::XcodeDetector` and
  `scull_detectors::android::AndroidDetector`. They require the usual parent folder
  (`DerivedData`, `<OS> DeviceSupport`, `CoreSimulator`, `avd`), read a DerivedData folder's
  project from its `info.plist` as data, raise Android release outputs (S1) and mapping files
  (S2), and turn the simulator device set into `xcrun simctl delete unavailable`, chosen in Rust
  (CLS-04).
- Game engines (`ecosystems/unity.toml`: `Library/` S1, `Temp/` S0, `obj/` S0;
  `ecosystems/unreal.toml`: `Intermediate/`, `Binaries/`, `DerivedDataCache/` S1, `Saved/` S2)
  are run by `scull_detectors::game_engines::GameEngineDetector`. Every folder needs the
  engine's project markers beside it (Unity: `ProjectSettings/` holding `ProjectVersion.txt`
  and `Assets/`; Unreal: a `.uproject` file), FX-CLS-14. Scene backups in Unity's `Temp/` and
  Unreal `Binaries/` without a `Source/` folder raise the tier to S2, and the editor's
  `Temp/UnityLockfile` is checked as an activity lock. Folders holding the user's work
  (`Assets/`, `Content/`, `Source/`, `Config/`, ...), plugin and engine folders, and the shared
  DerivedDataCache outside projects are never candidates.
- Long-tail ecosystems (P5-DET-05). Project folders need the build file beside them **and**
  what the tool writes inside them; only the usual folder names are considered:
  - C/C++/embedded (`ecosystems/native.toml`): CMake build folders (`CMakeCache.txt` and
    `CMakeFiles/` inside, FX-CLS-10; `scull_detectors::long_tail` reads the source folder from
    `CMAKE_HOME_DIRECTORY` and never takes an in-source build), Meson build folders, PlatformIO
    `.pio/`. S1: they hold the configure options, and C/C++ rebuilds are long.
  - SwiftPM `.build/` and CocoaPods `Pods/` (`ecosystems/apple.toml`, S1, lockfile-aware).
  - JVM extras (`ecosystems/jvm.toml`): sbt `target/` and `project/target/` (S0, like Maven),
    `.bloop/` and `.metals/` (S1).
  - Haskell and Elixir (`ecosystems/functional.toml`): `.stack-work/`, `dist-newstyle/` (S1),
    Mix `_build/` (S0) and `deps/` (S1; S2 without `mix.lock`).
  - Ruby `vendor/bundle` (`ecosystems/ruby.toml`, S1, must sit in `vendor/`), Zig `.zig-cache/`
    and `zig-out/` (`ecosystems/zig.toml`, S0).
  - Infrastructure (`ecosystems/infra.toml`): Terraform `.terraform/` (S1; S2 when it records a
    backend or a selected workspace), Bazel output bases (inspect-only, workspace read from
    `DO_NOT_BUILD_HERE`; `bazel clean --expunge` is the user's), Podman storage and the minikube
    cache (inspect-only). kind's images live in Docker.
  - IDE caches (`ecosystems/ide.toml`, run by `scull_detectors::ide`): JetBrains caches and
    indexes in the OS cache folder only (S1; never settings), editor workspace storage only when
    its workspace no longer exists (S2), and extension versions the editor itself listed in
    `.obsolete` that nothing refers to (S1).
- Package caches (Quick Clean): `scull_detectors::caches::locate_caches` finds the npm, pnpm,
  Bun, Yarn (classic and Berry), pip, uv, conda, Go (build and module), Gradle, Maven and NuGet
  caches per OS from an injected `CacheEnv` (home, environment, OS layout), validates their
  locations (QC-05, T-23) and picks each tool's native command by rule ID (`npm.cache`,
  `pnpm.store`, `bun.cache`, `yarn.classic-cache`, `yarn.berry-cache`, `pip.cache`, `uv.cache`,
  `conda.pkgs`, `go.build-cache`, `go.modcache`, `nuget.packages`). Gradle (`gradle.caches`) and
  Maven (`maven.repository`) have no cleanup command: located and measured, inspect-only.
  The long-tail tool caches are located the same way (`scull_detectors::tool_caches` lists the
  locations): ccache (`ccache --clear`), Conan (`conan cache clean`), Arduino (`arduino-cli
  cache clean`), CocoaPods (`pod cache clean --all`), Deno (`deno clean`; S2 when it holds
  `location_data/`), Playwright (`playwright uninstall --all`), Cypress (`cypress cache prune`),
  and Puppeteer (`puppeteer browsers clear`); vcpkg, PlatformIO, ESP-IDF, SwiftPM, Coursier,
  Zig, Electron, Homebrew and the Nix store are located and measured but inspect-only (no
  command that cleans exactly that folder: `brew cleanup`, for one, also uninstalls old formula
  versions).
- Bundled rules live in `rules/ecosystems/*.toml` and are **compiled into the binary**
  (`BUNDLED_RULE_FILES`, `include_str!`). scull never downloads rules (CLS-05). A test fails if a
  `.toml` file under `rules/` is not listed in `BUNDLED_RULE_FILES`.

## File format (schema version 1)

A rule file is TOML with a top-level `schema_version` and one or more `[[rule]]` tables.
**Unknown fields are rejected everywhere**, and one bad rule rejects the whole file.

```toml
schema_version = 1

[[rule]]
id = "rust.target"                 # namespaced, unique: <ecosystem>.<name>, a-z 0-9 - _
version = 1                        # >= 1; bump when matching or tiering changes
artifact = "Cargo build output"    # human-readable artifact type
ecosystem = "rust"                 # lowercase a-z 0-9 - _
category = "project_artifact"      # scull_core::Category (snake_case); not "scull_data"
platforms = ["macos", "linux", "windows"]
base_tier = "S0"                   # S0..S3; tier when every check passes (PRD §8.3)
strategy = "filesystem"            # filesystem | tool_native | inspect_only
evidence = ["marker", "lockfile", "known_artifact"]

[rule.match]
names = ["target"]                 # candidate directory name patterns (required, >= 1)

[rule.markers]                     # REQUIRED: at least one marker (CLS-03)
all = ["../Cargo.toml"]            # every one must be present
any = []                           # if listed (>= 2 entries), at least one must be present

[rule.lockfiles]                   # optional; files to reproduce from
any = ["../Cargo.lock"]

[rule.exclude]                     # optional
markers = ["../pom.xml"]           # present => rule does not apply
within = []                        # an ancestor dir with this name => rule does not apply

[rule.activity]                    # optional; names only, nothing is ever spawned
processes = ["cargo", "rust-analyzer"]
lock_files = ["debug/.cargo-lock"]

[rule.regeneration]                # optional; shown in the preview, never executed
instructions = "Run `cargo build` in the project to rebuild."
network = false                    # optional hint
long_build = false                 # optional hint
quick = false                      # optional hint
```

### Fields

| Field | Required | Meaning |
|---|---|---|
| `schema_version` | yes | Must equal the loader's `SCHEMA_VERSION` (currently `1`). Older/newer values are rejected. |
| `id` | yes | Stable namespaced ID, recorded on candidates as `scull_core::RuleRef { id, version }`. Duplicate IDs (within a file or across loaded files) are rejected; a user rule can't shadow a bundled one. |
| `version` | yes | Integer >= 1. |
| `artifact`, `ecosystem` | yes | Display/grouping text. |
| `category` | yes | `project_artifact`, `package_cache`, `worktree`, `ml_cache`, `container`, `large_file`, `other`. |
| `platforms` | yes | Non-empty, no duplicates: `macos`, `linux`, `windows`. |
| `base_tier` | yes | The tier when all checks pass. The classifier only ever raises it (Git-tracked, protected, active, recent, no lockfile, …). |
| `strategy` | yes | `filesystem`: safe executor (delete for S0, quarantine for S1/S2, D-3). `tool_native`: the owning tool's own cleanup; the command is chosen by Rust code keyed by rule ID, never by the rule. `inspect_only`: shown, never cleaned. |
| `evidence` | yes | Must include `marker`. Includes `lockfile` if and only if `lockfiles.any` is set. `known_artifact` is optional. |
| `match.names` | yes | Candidate directory name patterns. |
| `markers.all` / `markers.any` | at least one marker | See *Relative entries*. |
| `lockfiles.any` | no | If set and none is present, the candidate is reported with `lockfile_missing` and must be at least S2 (PRD §8.3, FX-CLS-05). |
| `exclude.markers` | no | Relative entries; matched **case-insensitively** (more exclusions = protective). |
| `exclude.within` | no | Name patterns matched case-insensitively against every ancestor directory name up to the scan root. |
| `activity.processes` | no | Process names (letters, digits, `-_.`) whose handles/cwd inside the candidate mean "in use". The universal 24 h activity window always applies and can't be disabled by a rule. |
| `activity.lock_files` | no | Literal paths **inside** the candidate (`/`-separated, no `..`, no wildcards, max 4 segments). |
| `regeneration.*` | no | Display text and hints. The hints set the rubric's regeneration cost: `long_build = true` → costly (S1); `quick = true` with `long_build = false` → quick (S0-eligible) even if `network = true` (e.g. a lockfile-pinned reinstall); `network = true` → costly; `network = false` and `long_build = false` → local (S0-eligible); anything unstated → unknown, treated as costly (D-5). The rule's `base_tier` is always a floor. |

### Name patterns

Each pattern matches **one** path component:

- `*` any run of characters, `?` one character;
- `{a,b,c}` one of the literal alternatives (no nesting, no wildcards inside, max 32 expansions);
- rejected: empty, `.`/`..`, `/`, `\`, `**`, `[`/`]` (reserved), control characters, > 255 bytes.

`match.names` and markers/lockfiles match case-sensitively (stricter); exclusions
case-insensitively.

### Relative entries (markers, lockfiles, exclusions)

A relative entry is zero or more `..` segments followed by one name pattern, relative to the
**candidate directory**:

| Entry | Where it is looked for |
|---|---|
| `CMakeCache.txt` | directly inside the candidate |
| `../Cargo.toml` | in the candidate's parent (a sibling of the candidate) |
| `../../pnpm-workspace.yaml` | in the grandparent |

At most 4 `..` segments. Absolute paths, `~`, `\`, `:` (drive letters, streams), empty segments
and anything but `..` before the name are rejected. "Candidate is child X of a directory
containing marker Y" is written `names = ["X"]`, `markers.all = ["../Y"]`.

## Safety rules enforced by the loader

1. **A directory name is never enough (CLS-03).** A rule with no marker is rejected
   (`NameOnly`). So is a marker the candidate could satisfy itself, such as `../target` for a
   rule matching `target` (compared case-insensitively; if both the name and a sibling marker use
   wildcards the loader can't prove they differ and rejects the rule), and a bare-wildcard marker
   such as `../*`. At match time the candidate itself never counts as a sibling marker.
2. **Rules never execute anything (CLS-04, FX-CFG-06).** Before schema checks the loader scans
   every key in the file and rejects any that looks like a command: keys containing `command`,
   `cmd`, `exec`, `shell` or `spawn`, and keys with a token `script(s)`, `run`, `args`, `argv`,
   `program`, `hook(s)`, `invoke`, `binary`, `sh`, `bash`, `pwsh`, `powershell` (split on `_`,
   `-`, `.` and camelCase). `regeneration.instructions` is display text only.
3. **User and community rules are untrusted (CLS-04, FX-CFG-05, P6-COM-01).** A non-bundled
   rule that assigns `S0` or uses `tool_native` is rejected. So is one whose `match.names` can
   match a folder holding people's work, settings or secrets (`PROTECTED_FOLDER_NAMES`: `src`,
   `lib`, `data`, `Documents`, `Library`, `.ssh`, ...; `user_work_folder`), one with a name
   pattern of fewer than two literal characters (`wildcard_name`), and one without a marker
   beside or above the candidate (`no_project_marker`): community rules only ever apply inside
   a recognizable project. Non-bundled rules load **disabled** and only run after the user
   enables them in config (`RuleSet::enable`); a rule file can't enable itself. Community S0
   rules backed by passing fixture tests are a v1.x feature.
4. **Bundled only, no network (CLS-05).**

## Marker evaluation (no I/O)

`Rule::check_markers(&Surroundings)` decides from entry names alone whether a candidate meets a
rule's requirements. The scanner lists the levels the rule needs (`Rule::required_levels()`, and
ancestor names if `Rule::needs_ancestor_names()`). A level that wasn't listed is unknown, not
empty, and gives `MarkerOutcome::Incomplete`. Outcomes: `NameMismatch`, `Incomplete`,
`Excluded`, `MissingMarker`, or `Matched(MarkerMatch)` with the markers and lockfile found;
`MarkerMatch::evidence()` turns a match into `scull_core::Evidence`.
