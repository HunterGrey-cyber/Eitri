# poc/

The Cargo workspace root for these crates is the repo root's `Cargo.toml`, not this directory —
it moved there when `neovide-editor/` was added as a sibling of `poc/` (Cargo requires all
workspace members to live within the workspace root's own directory tree). Build/run any crate
here from the repo root, e.g. `cargo build --manifest-path Cargo.toml -p gl_skia_test`, or `cd`
into the repo root first — `cargo`'s own upward directory-tree discovery finds the root manifest
either way as long as there's no stray `Cargo.toml` in `poc/` itself to shadow it.
