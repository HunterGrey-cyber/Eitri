// Fixture for packaging/test_check_abi_floor.py -- stands in for a real `-sys` crate's `src/lib.rs`
// (gtk4-sys, gdk4-sys, gsk4-sys, webkit6-sys, javascriptcore6-sys all use this exact shape: a
// `#[cfg(feature = "vX_Y")]` immediately before the gated item, sometimes followed by a
// `#[cfg_attr(docsrs, doc(cfg(...)))]` doc-only twin). Not a real crate; never built.

extern "C" {
    // Unguarded: this crate's baseline, always available. Never above any floor this script would
    // plausibly be asked to check, so it must be absent from the parsed gate map.
    pub fn gtk_fake_baseline_fn();

    #[cfg(feature = "v4_14")]
    #[cfg_attr(docsrs, doc(cfg(feature = "v4_14")))]
    pub fn gtk_fake_v4_14_fn();

    // The "fails at the 4.14 floor" case.
    #[cfg(feature = "v4_16")]
    #[cfg_attr(docsrs, doc(cfg(feature = "v4_16")))]
    pub fn gtk_fake_v4_16_fn();

    // Exercises the `cfg(any(feature = "...", docsrs))` shape the parser must also handle (not
    // seen in the real crates as of 2026-09-27, but the brief calls it out defensively).
    #[cfg(any(feature = "v4_18", docsrs))]
    pub fn gtk_fake_v4_18_fn();
}
