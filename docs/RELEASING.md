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
git tag -a v0.1.1 -m "nupdf 0.1.1"
```

```bash
git push origin v0.1.1
```

Pushing the tag starts the `release` workflow, which builds the `nupdf` binary for Linux (x86-64 and ARM, each for glibc 2.35 or newer and as a static musl binary), macOS (Apple silicon and Intel) and Windows (x86-64), runs each on a file where the runner can, and attaches the archives and their SHA-256 checksums to a draft GitHub release for the tag. It builds the same seven binaries as Python wheels (`crates/nupdf-cli/pyproject.toml`), installs each where the runner can, and attaches those too. It fails if the tag is not the workspace's version.

Then open that draft, replace its notes with the CHANGELOG entry, check the seven archives, seven wheels and eight npm packages (`.tgz`) are there, and publish it. Publishing it starts the `publish` workflow, which sends the wheels to PyPI as `nupdf-cli` (the name `nupdf` there belongs to another project), so that `pip install nupdf-cli` and `uvx nupdf-cli` give the `nupdf` command. PyPI accepts them without a token because the project there names this repository, the `publish.yml` workflow and the `pypi` environment as a trusted publisher; that is set up once, at https://pypi.org/manage/account/publishing/. A version on PyPI cannot be replaced, only yanked.

The same workflow sends the npm packages: `nupdf`, a small launcher, and one `@nupdf/...` package per platform carrying the binary, of which npm installs the one that matches the machine (`npm/`; `node --test npm/test.mjs` checks them against a built binary). So `npx nupdf` and `npm install -g nupdf` give the command. npm trusts the workflow per package, and only once the package exists, so the first release is published by hand: download the eight `.tgz` files from the release, `npm login`, and `npm publish FILE --access public` for each, `nupdf-VERSION.tgz` last. Then, in each package's settings on npmjs.com, add a trusted publisher: GitHub Actions, `raspao-studio/nupdf`, workflow `publish.yml`, environment `npm`. A version on npm cannot be replaced either.

If a release for the tag already exists, the release workflow adds its files to it instead of making a draft.

## After

- `cargo install nupdf-cli` on a clean machine installs a `nupdf` that runs `nupdf guide`.
- docs.rs builds `nupdf` (https://docs.rs/nupdf).
- The site is up, and its links resolve.
