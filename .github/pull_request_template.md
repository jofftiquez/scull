Closes #

## What

## Acceptance criteria
- [ ]

## Fixtures added / updated
-

## Checklist
- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` pass locally
- [ ] No OS-specific code outside `scull-platform`; no deletion outside `scull-cleaner`
- [ ] Evidence comment posted on the issue and `evidence-posted` label added

## Detector rules (only if this PR adds or changes a rule)
- [ ] A marker beside or above the folder proves the project; the name alone never does
- [ ] The folder never holds people's own work, settings or secrets, on any OS
- [ ] The tier is the least that's true (S0 only for quick, local, credential-free regeneration)
- [ ] Lockfile-aware where the tool has one; no command, script or hook in the rule
- [ ] Look-alike folders excluded; positive, negative and exclusion cases tested
- [ ] `scull rules validate` and `scull rules test` (community rules) or fixture tests (bundled) pass
