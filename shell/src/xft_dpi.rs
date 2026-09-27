//! Ruling S3 (`docs/superpowers/plans/2026-09-27-v1-scale.md`, D4): guard against `gtk-xft-dpi`
//! being unset when the agent panel's `WebView` is created.
//!
//! WebKitGTK 2.52.6's `fontDPI()` uses `gtk-xft-dpi` when it is set, and otherwise falls back to
//! the primary screen's own DPI, computed by `ScreenManagerGtk.cpp` as diagonal pixels divided by
//! diagonal millimetres, with **no guard for 0mm**
//! (`Source/WebCore/platform/gtk/PlatformScreenGtk.cpp`, tag `webkitgtk-2.52.6`). A monitor that
//! reports 0x0mm (this project's headless-sway sandbox; on real hardware, an EDID that reports 0mm,
//! seen on some projectors/TVs/virtual displays) with no xsettings portal and no
//! `gtk-4.0/settings.ini` leaves `gtk-xft-dpi` at GTK's own "unset" sentinel, so WebKit's DPI
//! divides by zero: `devicePixelRatio` becomes `Infinity` and the panel goes permanently blank
//! (`innerWidth`/`innerHeight` both 0) -- not recoverable by a resize, only by relaunching the
//! whole process.
//!
//! The fix sets `gtk-xft-dpi` to GTK's own default for unset (96dpi, `96 * 1024` in the setting's
//! 1/1024dpi units) before any `WebView` is constructed, so WebKit reads a real number instead of
//! deriving one from the monitor. This changes nothing for native GTK text: 96dpi is what GTK
//! itself already uses when the setting is unset. A real desktop's xsettings/portal value, or a
//! user's own `settings.ini`, is never touched -- only `<= 0` (GTK's own "nothing has set this")
//! is replaced. Once replaced, the value is pinned for the process: GTK 4.22.5 marks a property set
//! through `gtk_settings_set_property` as `GTK_SETTINGS_SOURCE_APPLICATION` (`gtksettings.c`), and
//! `settings_update_xsetting` ignores later portal/xsettings updates for an application-sourced
//! property. So a real DPI that only arrives after startup (a text-scaling change) is not followed
//! in a session that started with the setting unset -- accepted: that session had no DPI at all.

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
        assert_eq!(xft_dpi_fallback(147456), None); // the owner's own settings.ini
    }
}
