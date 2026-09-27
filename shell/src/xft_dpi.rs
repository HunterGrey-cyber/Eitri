//! Supplies a fallback when `gtk-xft-dpi` is unset before a `WebView` is created.
//!
//! WebKitGTK 2.52.6's `fontDPI()` uses `gtk-xft-dpi` when set. Otherwise it derives DPI from
//! the primary screen's diagonal pixels divided by diagonal millimetres, without guarding
//! against zero millimetres. If the monitor reports 0x0mm and neither a portal nor
//! `gtk-4.0/settings.ini` supplies a DPI, WebKit divides by zero. `devicePixelRatio` becomes
//! `Infinity`, and the panel stays blank with `innerWidth` and `innerHeight` both zero.
//! Resizing does not recover it; the process must be relaunched.
//!
//! Before any `WebView` is constructed, unset values (`<= 0`) are replaced with GTK's own
//! default of 96dpi, stored as `96 * 1024` in the setting's 1/1024dpi units. WebKit then reads
//! a finite value without deriving it from the monitor. Native GTK text already uses 96dpi
//! when this setting is unset. Existing positive values from the user or portal are preserved.
//!
//! The fallback is pinned for the process. GTK 4.22.5 marks a value set through
//! `gtk_settings_set_property` as `GTK_SETTINGS_SOURCE_APPLICATION`, and
//! `settings_update_xsetting` ignores later portal or xsettings updates for that property.
//! A session that starts with the setting unset therefore does not follow later DPI changes.

/// GTK's own font default when the setting is unset (`gtk-xft-dpi` is in 1/1024 dpi).
pub(crate) const GTK_DEFAULT_XFT_DPI: i32 = 96 * 1024;

/// `None` when `current` is already a real value (do not overwrite a user's or a portal's
/// setting); `Some(GTK_DEFAULT_XFT_DPI)` when `current` is GTK's own "unset" sentinel (`<= 0`).
pub(crate) fn xft_dpi_fallback(current: i32) -> Option<i32> {
    if current <= 0 {
        Some(GTK_DEFAULT_XFT_DPI)
    } else {
        None
    }
}

/// Called once, as the first statement of `build_ui`, before `ThemeCss::install` and before any
/// `WebView` (the agent panel's or a Lua panel's) is constructed. Also re-applies the fallback if
/// `gtk-xft-dpi` is ever notified back to `<= 0`. Once the fallback has been applied, no external
/// source can notify the property again (see the module doc), so that hook can only fire on an
/// in-process write -- a cheap backstop, since WebKit reads the setting live.
pub(crate) fn ensure_xft_dpi() {
    let Some(settings) = gtk4::Settings::default() else {
        // No display/settings object in this process at all (e.g. a non-GUI test binary). Nothing
        // to guard.
        return;
    };
    apply_fallback(&settings);
    settings.connect_gtk_xft_dpi_notify(apply_fallback);
}

fn apply_fallback(settings: &gtk4::Settings) {
    let current = settings.gtk_xft_dpi();
    if let Some(fallback) = xft_dpi_fallback(current) {
        println!(
            "[hidpi] gtk-xft-dpi unset ({current}); set to {fallback} (96 dpi) so WebKitGTK does not derive its \
             zoom from the monitor's millimetres"
        );
        settings.set_gtk_xft_dpi(fallback);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unset_dpi_is_replaced() {
        assert_eq!(xft_dpi_fallback(-1), Some(GTK_DEFAULT_XFT_DPI));
        assert_eq!(xft_dpi_fallback(0), Some(GTK_DEFAULT_XFT_DPI));
        assert_eq!(xft_dpi_fallback(98304), None);
        assert_eq!(xft_dpi_fallback(147456), None);
    }
}
