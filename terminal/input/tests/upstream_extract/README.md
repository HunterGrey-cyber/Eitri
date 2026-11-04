# Upstream extracts

Byte-exact copies of the two line ranges of

    alacritty/src/input/keyboard.rs

at upstream commit `94e7c8874e526b1e67b349d9ba30ddf81669119e` (tags `v0.17.0` and
`alacritty_terminal_v0.26.0`) that `src/vendored/build_sequence.rs` vendors.

They are checked in so that `tests/vendored_is_verbatim.rs` can prove the vendored
regions are unmodified on a machine that has no alacritty checkout. Regenerate with:

    sed -n '133,172p' <alacritty>/alacritty/src/input/keyboard.rs > keyboard_133_172.rs.txt
    sed -n '291,718p' <alacritty>/alacritty/src/input/keyboard.rs > keyboard_291_718.rs.txt

sha256, verified 2026-09-12 against the live checkout:

    9fbb843111798ab496fdbe2fc89f691ee1aa2ff8f1789126cf9a587b900a1211  keyboard_133_172.rs.txt
    88c30e1e8190a9c1fb8199c7d5c54134379b11330d4f82ef8bd7f8610927a0ba  keyboard_291_718.rs.txt
    99e4c978ef2ddc0986c241de5f7183b68c0c3065e69c95b1200f7583a69628c7  (whole upstream keyboard.rs)

Licence: Apache-2.0, unmodified upstream source. See `../../LICENSE-APACHE`.
