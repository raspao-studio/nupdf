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
CARGO_TARGET_DIR=target/pkgcheck cargo package -p nupdf -p nupdf-cli
```

Packaging the two crates together is the only way to check `nupdf-cli` before `nupdf` is on crates.io. Cargo takes a registry crate's version to mean its contents never change, so a second check of the same version reuses the first one's build; a target directory of its own each time (delete `target/pkgcheck` first) keeps the check honest.

## The first release only

1. Make the repository public: GitHub, the repository's Settings, General, Danger Zone, Change visibility.
2. Turn on GitHub Pages: Settings, Pages, Source: GitHub Actions. Set its custom domain to nupdf.dev. The next push to `main` that touches the site or the docs publishes the landing page at https://nupdf.dev/ and the docs at https://nupdf.dev/docs/ (or run the Docs workflow by hand).
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

Pushing the tag starts the `release` workflow, which builds the `nupdf` binary for Linux (x86-64 and ARM, each for glibc 2.35 or newer and as a static musl binary), macOS (Apple silicon and Intel) and Windows (x86-64), runs each on a file where the runner can, and attaches the archives and their SHA-256 checksums to a draft GitHub release for the tag. It fails if the tag is not the workspace's version.

Then open that draft, replace its notes with the CHANGELOG entry, check the seven archives are there, and publish it. If a release for the tag already exists, the workflow adds the archives to it instead.

## After

- `cargo install nupdf-cli` on a clean machine installs a `nupdf` that runs `nupdf guide`.
- docs.rs builds `nupdf` (https://docs.rs/nupdf).
- The site is up, and its links resolve.
