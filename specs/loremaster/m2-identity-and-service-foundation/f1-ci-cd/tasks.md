# M2 F1. CI/CD: Tasks

Scope: faster CI on all 3 runners, no timing checks in CI. No Rust code changes except removing wall-clock limits from tests. Requiring CI on `main` is done: `main` requires `check (macos-15)`, `check (ubuntu-latest)`, `check (ubuntu-24.04-arm)`, so job and matrix names stay as they are.

Decision: latency checks leave CI and get no nightly run. `dev bench` and the release-only `search_over_50k_memories_is_fast` test stay in the code for local runs. `dev bench` on a developer laptop remains the NFR1 and NFR3 pass or fail.

All CI checks below run on a PR on all 3 runners. A second run means `gh run rerun` on the same PR, because a PR run can only restore caches saved by its own ref or `main`.

## 1. CI runs no latency checks [serial]
- [x] 1.1 `.github/workflows/ci.yml`: delete the `search speed` and `bench` steps. CI run lists no release build, no `dev bench`; the 3 required checks are green.
- [x] 1.2 Docs: in `specs/loremaster/m1-local-client-core/design.md`, replace the sentence on `dev bench` running in CI with "It does not run in CI, because shared runners are too noisy for wall-clock limits."; in `specs/loremaster/milestones.md`, rewrite the M2 F1 entry to "cache the Cargo build and the embedding model in CI, require CI on `main` (done), and remove the latency checks from CI". `grep -n "nightly" specs/loremaster/milestones.md` finds nothing.
- [x] 1.3 `crates/loremaster/tests/daemon.rs`, `crates/loremaster/tests/hook.rs`: remove the 5 ms and 150 ms limits, keep the behavior assertions. The 30 s `wait_for` timeout stays, because it stops a hung test and limits no latency. No test that CI runs fails on a latency limit.

## 2. A repeat CI run reuses the Cargo build [serial, needs 1]
- [x] 2.1 `.github/workflows/ci.yml`: add `Swatinem/rust-cache@v2` directly after `Install toolchain`, because it reads the active `rustc`. Second run logs a cache hit and the `clippy` step does not recompile dependencies.
- [x] 2.2 Confirm on the second run: `test` links and is green on all 3 runners with no `ort-sys` build or download, with nothing extra cached for ONNX Runtime. The cached `ort-sys` library appears to bundle it (seen on macOS arm64, so the Linux runners must show it too).

## 3. A repeat CI run reuses the embedding model [serial, needs 2]
- [x] 3.1 `.github/workflows/ci.yml`: add `actions/cache@v4` directly after rust-cache, with `path: target/tmp/shared-models` (`shared_models()` in `crates/loremaster/tests/common/mod.rs`) and `key: embedding-model-${{ hashFiles('crates/loremaster/src/embed.rs') }}` (`embed.rs` pins the model URL and SHA-256s). Keep this order: restore puts the model into the restored `target/`, and post steps run in reverse, so the model is saved before rust-cache cleans `target/`. Second run logs `Cache hit` for this key.
- [x] 3.2 Confirm on the second run: `pinned_model_has_the_expected_files_and_sizes` passes on all 3 runners from the restored files, and a test made to fail still turns its job red. Revert the failing test.
- [x] 3.3 `specs/loremaster/milestones.md`: set M2 F1 `Status: done` after the 3 required checks are green on the PR.

## Dependencies
- All three parents edit `.github/workflows/ci.yml`, so they run in order: 1, 2, 3.
- 2.1 blocks 2.2 and 3.1, because of step order.
