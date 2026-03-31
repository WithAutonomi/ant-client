---
name: update-ant-node-ref
description: Update the ant-node dependency version in ant-core/Cargo.toml, regenerate lockfile, verify the build, and commit the change. Use when asked to update ant-node or /update-ant-node-ref.
allowed-tools: Bash, Read, Edit, Glob, Grep, AskUserQuestion
---

# Update ant-node Reference

This skill updates the `ant-node` dependency version in `ant-core/Cargo.toml`, regenerates the
lockfile, verifies the build, and commits the change.

## Procedure

### Step 1: Prompt for target version

Ask the user: **What version of `ant-node` should we update to?** (e.g., `0.9.0`)

Do not proceed without a version string.

### Step 2: Update the dependency

Edit `ant-core/Cargo.toml` and change the `ant-node` version to the one the user provided. The line
to change looks like:

```toml
ant-node = "X.Y.Z"
```

### Step 3: Update the lockfile

Run:

```bash
cargo update
```

This regenerates `Cargo.lock` to reflect the new version.

### Step 4: Verify the build

Run:

```bash
cargo build
```

Do **not** use `--release` — it is too slow for a validation build.

If the build **succeeds**, proceed to Step 5.

If the build **fails**, stop and report the error to the user. Explain what breaking changes were
detected and what code modifications would be needed to get the build to compile. Do not attempt to
fix the code automatically — let the user decide how to proceed.

### Step 5: Commit the change

Stage `ant-core/Cargo.toml` and `Cargo.lock`, then create a commit:

```
chore: update ant-node to <version>
```

### Step 6: Ask about PR

Ask the user: **Would you like to create a pull request for this change?**

If yes, create a PR with:
- **Title:** `chore: update ant-node to <version>`
- **Body:** A brief summary noting the version bump and that the build was verified.
