# Releasing

How a release goes out. The steps that publish something, or change who can see the repository, are run by hand.

## Before

1. Everything for the release is merged into `main`, and `main`'s CI is green.
2. The version in the workspace `Cargo.toml` (both crates share it) and the top entry of [CHANGELOG](../CHANGELOG.md) say the same thing.
3. From a clean checkout of `main`, these pass:

```bash
cargo test --workspace
```

```bash
cargo test --workspace -- --ignored
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo deny check
```

```bash
cargo package --workspace
```

`cargo package --workspace` packages and builds both crates together, which is the only way to check `nupdf-cli` before `nupdf` is on crates.io.

## The first release only

1. Make the repository public: GitHub, the repository's Settings, General, Danger Zone, Change visibility.
2. Turn on GitHub Pages: Settings, Pages, Source: GitHub Actions. The next push to `main` that touches the docs publishes the site at https://raspao-studio.github.io/nupdf/ (or run the Docs workflow by hand).
3. Turn on private vulnerability reporting: Settings, Security, Private vulnerability reporting. [SECURITY](../SECURITY.md) sends reports there.
4. Apply to OSS-Fuzz with the files in `oss-fuzz/` (its README says how).

## Publishing

Log in to crates.io once with a token from https://crates.io/settings/tokens:

```bash
cargo login
```

Publish the library first, then the CLI, which depends on it:

```bash
cargo publish -p nupdf
```

```bash
cargo publish -p nupdf-cli
```

Tag the release and push the tag:

```bash
git tag -a v0.1.0 -m "nupdf 0.1.0"
```

```bash
git push origin v0.1.0
```

Then make a GitHub release from the tag, with the CHANGELOG entry as its notes.

## After

- `cargo install nupdf-cli` on a clean machine installs a `nupdf` that runs `nupdf guide`.
- docs.rs builds `nupdf` (https://docs.rs/nupdf).
- The site is up, and its links resolve.
