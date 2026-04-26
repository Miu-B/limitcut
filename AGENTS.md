# AGENTS

## Build and test

```bash
cargo build --release
cargo test
```

The project compiles as a single binary. Tests must pass before any commit.

## Release workflow

### 1. Feature work

- Branch from `master` (or commit directly for small changes)
- Write tests that exercise the new behaviour
- `cargo test` must pass with all 128+ tests

### 2. Commit message convention

Short imperative sentence, present tense:

```
Allow --preview-blur with just the pre-video
Validate input modes before loading config
Add fade-in/fade-out, black-hold, and title overlay support
Bump version to 0.4.0
```

### 3. Versioning (semver)

- **Patch** (`x.y.Z`): bug fixes only — `0.3.0 → 0.3.1`
- **Minor** (`x.Y.z`): new backward-compatible features — `0.3.1 → 0.4.0`
- **Major** (`X.y.z`): breaking changes — not applicable yet (pre-1.0)

### 4. Bump the version

```bash
# 1. Edit Cargo.toml  version = "0.X.Y"
# 2. Sync the lockfile
cargo update

# 3. Commit
git add Cargo.toml Cargo.lock
git commit -m "Bump version to 0.X.Y"

# 4. Tag and push atomically
git tag v0.X.Y
git push --atomic origin master v0.X.Y
```

### 5. Create the GitHub Release

Use `gh release create`. Write the notes in markdown — `## What's new` for features, `## Fixes` for patches. **Do not** include a `**Full Changelog**` link manually; GitHub appends it automatically.

```bash
gh release create v0.X.Y --title "v0.X.Y" --notes "$(cat <<'EOF'
## What's new

- **Feature name** — short description of what it does
- **Another feature** — another short description
EOF
)"
```

Example for a patch release:

```bash
gh release create v0.X.Y --title "v0.X.Y" --notes "$(cat <<'EOF'
## Fixes

- **Area** — short description of what was fixed
EOF
)"
```
