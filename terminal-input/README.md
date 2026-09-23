> **Moved into neovibe on 2026-09-23** from Verdandi `crates/terminal-input` @ `e5f9cc3` (neovibe spec
> `docs/superpowers/specs/2026-09-23-bottom-terminal-design.md`, decision 4.1(a)). Paths, revisions and pins
> below are Verdandi's as of that commit; here the crate is the workspace member `terminal-input/`.

# terminal-input

A `TermMode`-aware keyboard and paste encoder: `NormalizedInput` + the
authoritative terminal's current `TermMode` → the bytes to write to the PTY.

```
NormalizedInput ─┬─ Key { event, mods } ─┐
                 └─ Paste { text, .. } ──┤
                                         ├─→ encode(input, mode) → Vec<u8> → PTY
    alacritty_terminal::Term::mode() ────┘
```

## Layout

| path | what it is |
|---|---|
| `src/keys.rs` | winit-shaped input types, written here because `winit::event::KeyEvent` cannot be constructed outside winit (private `platform_specific` field) and winit with `default-features = false` does not build on Linux. |
| `src/vendored/build_sequence.rs` | Alacritty's encoder core, **byte-identical** to upstream lines 133-172 and 291-718. Apache-2.0. Do not edit; `tests/vendored_is_verbatim.rs` enforces it. |
| `src/regime.rs` | everything upstream does *around* `build_sequence`: the declarative binding table, `APP_CURSOR`, `APP_KEYPAD`, the Alt-ESC prefix, plain text, key **release**, bracketed paste. This is where the behaviour lives. |

`build_sequence` is not the encoder. It returns an **empty** vector for every
plain printable character and knows nothing about `APP_CURSOR`, `APP_KEYPAD`,
the legacy `Backspace`/`Tab`/`F1..F4` escapes, or paste. Wiring a terminal to it
alone yields "typing does nothing".

## Licensing

The vendored encoder is Apache-2.0 **only** — upstream's `alacritty/Cargo.toml`
declares `license = "Apache-2.0"`, and the `LICENSE-MIT` at the upstream repo
root does not cover that crate. `LICENSE-APACHE` satisfies section 4(a); `NOTICE`
and the per-file provenance header satisfy section 4(b), naming the upstream
commit `94e7c8874e526b1e67b349d9ba30ddf81669119e` and the exact line ranges.

## Tests

| file | what it proves |
|---|---|
| `tests/differential.rs` | **1,634,304-case sweep** (256 mode combos × 16 modifier sets × 133 key shapes × 3 states) against an independently written transliteration of upstream in `tests/support/reference.rs`. Upstream fidelity. |
| `tests/known_vectors.rs` | hand-written expected bytes, derived by reading upstream, not generated from this crate. Correctness. |
| `tests/golden.rs` | one FNV-1a digest per `TermMode` combination. Makes every behavioural change visible — including changes inside the shared vendored code, which the differential cannot see. |
| `tests/vendored_is_verbatim.rs` | the Apache-2.0 §4(b) claim: zero vendored lines changed. |
| `tests/real_app.rs` | real `nvim`, `bash` and `fzf` on a real PTY. `#[ignore]`d. |

```sh
cargo test --release                                          # fast gate
cargo test --release --test real_app -- --ignored --test-threads=1
cargo mutants --profile release -j 6 --timeout 120            # exhaustive mutation
```

## Things that look like bugs and are not

* **The six `APP_CURSOR` rows carry no kitty gate.** Unlike `F1..F4`, `Tab` and
  `Backspace` (bindings.rs:451-459), rows 444-449 have no
  `~REPORT_ALL_KEYS_AS_ESC` / `~DISAMBIGUATE_ESC_CODES`. Upstream really does
  emit `\x1bOA` for an unmodified `ArrowUp` with kitty negotiated. Adding a
  pre-check there would be a divergence, not a fix.
* **Plain `Backspace` still returns `\x7f` under `DISAMBIGUATE_ESC_CODES`.**
  Row 457 is gated on `~REPORT_ALL_KEYS_AS_ESC` alone. Rows 458/459 (Alt, Shift)
  additionally require `~DISAMBIGUATE_ESC_CODES`, so *those* become
  `\x1b[127;3u` / `\x1b[127;2u` under kitty level 1.
* **`Ctrl+Alt+Backspace` is `\x1b\x08`, not `\x1b\x7f`.** Binding modifiers match
  with `==`, not `contains`.
* **A key press and its release can encode asymmetrically.** `key_release` calls
  `build_sequence` directly and never consults `should_build_sequence` or the
  binding table. So with `REPORT_EVENT_TYPES` alone, `a` press → `a` but `a`
  release → `\x1b[97;1:3u`; and with `APP_CURSOR`, `ArrowUp` press → `\x1bOA` but
  its release → `\x1b[1;1:3A`.
* **`Enter`, `Tab` and `Backspace` releases emit nothing** unless
  `REPORT_ALL_KEYS_AS_ESC`. Removing that arm makes one keystroke act twice.

## Not implemented, by decision

`modifyOtherKeys` (XTMODKEYS). Upstream Alacritty does not implement it either.
