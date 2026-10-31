use super::*;
use crate::keymap::{Action, Source as BindSource};
use crate::test_scratch_dir::ScratchDir;

fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
    let pairs = pairs.to_vec();
    move |name| pairs.iter().find(|(n, _)| *n == name).map(|(_, v)| OsString::from(*v))
}

fn root(keys: &[(&str, Action)]) -> Vec<Binding> {
    keys.iter()
        .map(|(key, action)| Binding {
            key: KeySpec::parse(key).unwrap(),
            action: action.clone(),
            repeatable: false,
            source: BindSource::User,
        })
        .collect()
}

fn chords(accels: &[&str]) -> Vec<(Chord, String)> {
    accels
        .iter()
        .map(|a| (checked(Chord::from_accel(a).unwrap()).unwrap(), a.to_string()))
        .collect()
}

#[test]
fn each_variable_and_each_bus_name_is_evidence_and_an_empty_environment_is_none() {
    let none = env_of(&[]);
    assert!(im_evidence(&none, &[]).is_empty());
    for (var, value, im) in [
        ("GTK_IM_MODULE", "fcitx", Im::Fcitx),
        ("GTK_IM_MODULE", "fcitx5", Im::Fcitx),
        ("QT_IM_MODULE", "ibus", Im::Ibus),
        ("XMODIFIERS", "@im=fcitx", Im::Fcitx),
        ("XMODIFIERS", "@im=ibus", Im::Ibus),
    ] {
        assert_eq!(im_evidence(&env_of(&[(var, value)]), &[]), [im], "{var}={value}");
    }
    assert_eq!(im_evidence(&none, &["org.fcitx.Fcitx5"]), [Im::Fcitx]);
    assert_eq!(im_evidence(&none, &["org.freedesktop.IBus"]), [Im::Ibus]);
    assert!(im_evidence(&none, &["org.freedesktop.Notifications"]).is_empty());
    assert!(im_evidence(&env_of(&[("GTK_IM_MODULE", "xim"), ("XMODIFIERS", "")]), &[]).is_empty());
}

#[test]
fn evidence_from_two_places_for_one_input_method_is_one_entry() {
    let env = env_of(&[("GTK_IM_MODULE", "fcitx"), ("XMODIFIERS", "@im=fcitx")]);
    assert_eq!(
        im_evidence(&env, &["org.fcitx.Fcitx5", "org.freedesktop.IBus"]),
        [Im::Fcitx, Im::Ibus]
    );
}

#[test]
fn every_default_row_is_a_key_eitri_can_bind_and_names_its_upstream_file() {
    for (_, key, what, file) in IM_DEFAULTS {
        let spec = KeySpec::parse(key).unwrap_or_else(|e| panic!("{key}: {e}"));
        assert!(spec.ctrl || spec.meta, "{key} would be refused as a root key");
        assert!(!what.is_empty());
        assert!(file.ends_with(".cpp") || file.ends_with(".h"), "{file}");
    }
}

#[test]
fn the_default_trigger_of_a_running_input_method_is_a_conflict_and_only_that_one() {
    let table = root(&[("C-Space", Action::Zoom), ("M-t", Action::Zoom)]);
    let found = conflicts(&table, &[Im::Fcitx], &[]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.human(), "Ctrl+Space");
    assert_eq!(found[0].source, Source::InputMethod(Im::Fcitx));
    assert_eq!(
        log_line(&found[0]),
        "[keys] root key Ctrl+Space (zoom): fcitx5 switches input methods with it by default; if it does here, \
         Eitri never sees it"
    );
    assert!(
        conflicts(&table, &[], &[]).is_empty(),
        "no evidence of an input method, no warning"
    );
    assert!(
        conflicts(&root(&[("M-`", Action::Zoom)]), &[Im::Fcitx], &[]).is_empty(),
        "a key that is no default of this input method is no conflict"
    );
    assert!(
        conflicts(&table, &[Im::Ibus], &[]).is_empty(),
        "IBus's own defaults are not Ctrl or Alt chords: it has no row"
    );
    let clipboard = conflicts(&root(&[("C-;", Action::Zoom)]), &[Im::Fcitx], &[]);
    assert_eq!(clipboard.len(), 1);
    assert!(clipboard[0].what.contains("clipboard"), "{}", clipboard[0].what);
}

#[test]
fn conflicts_are_found_across_the_input_method_gnome_sway_and_hyprland() {
    let table = root(&[
        ("C-Space", Action::Zoom),
        ("M-F4", Action::Zoom),
        ("C-M-t", Action::Zoom),
        ("M-x", Action::Zoom),
        ("M-y", Action::Zoom),
    ]);
    let desktop = vec![
        (Source::Gnome, chords(&["<Alt>F4", "<Primary><Alt>t"])),
        (Source::Sway, chords(&["<Alt>x", "<Control>space"])),
        (Source::Hyprland, chords(&["<Alt>x"])),
    ];
    let found = conflicts(&table, &[Im::Fcitx], &desktop);
    let seen: Vec<(String, Source)> = found.iter().map(|c| (c.key.human(), c.source)).collect();
    assert_eq!(
        seen,
        [
            ("Ctrl+Space".to_string(), Source::InputMethod(Im::Fcitx)),
            ("Ctrl+Space".to_string(), Source::Sway),
            ("Alt+F4".to_string(), Source::Gnome),
            ("Ctrl+Alt+t".to_string(), Source::Gnome),
            ("Alt+x".to_string(), Source::Sway),
            ("Alt+x".to_string(), Source::Hyprland),
        ]
    );
}

#[test]
fn a_shifted_letter_and_a_press_of_it_compare_equal() {
    let table = root(&[("M-Y", Action::Zoom)]);
    let desktop = vec![(Source::Sway, chords(&["<Alt><Shift>y"]))];
    assert_eq!(conflicts(&table, &[], &desktop).len(), 1);
}

#[test]
fn a_sway_session_reads_no_gnome_source() {
    assert_eq!(desktop_sources(Wm::Sway), [Source::Sway]);
    assert_eq!(desktop_sources(Wm::Hyprland), [Source::Hyprland]);
    assert_eq!(desktop_sources(Wm::Gnome), [Source::Gnome]);
    assert!(desktop_sources(Wm::Niri).is_empty());
    assert!(desktop_sources(Wm::None).is_empty());
    for wm in [Wm::Sway, Wm::Hyprland, Wm::Niri, Wm::None] {
        assert!(
            !desktop_sources(wm).contains(&Source::Gnome),
            "{wm:?} must not read GNOME's settings"
        );
    }
}

#[test]
fn the_fingerprint_ignores_order_and_action_and_changes_with_a_new_conflict() {
    let table = root(&[("C-Space", Action::Zoom), ("M-x", Action::Zoom)]);
    let desktop = vec![(Source::Sway, chords(&["<Control>space", "<Alt>x"]))];
    let both = conflicts(&table, &[], &desktop);
    let mut reversed = both.clone();
    reversed.reverse();
    assert_eq!(fingerprint(&both), fingerprint(&reversed));
    let mut other_action = both.clone();
    other_action[0].action = "tab.new".to_string();
    assert_eq!(fingerprint(&both), fingerprint(&other_action));
    assert_ne!(fingerprint(&both), fingerprint(&both[..1]));
    assert_ne!(fingerprint(&both), fingerprint(&[]));
}

#[test]
fn a_set_is_announced_once_and_again_only_when_it_changes() {
    let dir = ScratchDir::new("eitri-root-conflicts", "notice");
    let file = dir.join("root-key-conflicts");
    let table = root(&[("C-Space", Action::Zoom), ("M-x", Action::Zoom)]);
    let one = conflicts(&table[..1], &[Im::Fcitx], &[]);
    let two = conflicts(&table, &[Im::Fcitx], &[(Source::Sway, chords(&["<Alt>x"]))]);

    assert!(needs_notice(&file, &fingerprint(&one)));
    let (toast, err) = announce(Some(&file), &one);
    assert!(toast.is_some() && err.is_none());
    assert!(!needs_notice(&file, &fingerprint(&one)), "remembered");
    assert_eq!(announce(Some(&file), &one).0, None, "the same set is no news");
    assert!(announce(Some(&file), &two).0.is_some(), "a new conflict is");
    assert_eq!(announce(Some(&file), &two).0, None);

    assert_eq!(announce(Some(&file), &[]).0, None, "nothing found, nothing shown");
    assert!(
        announce(Some(&file), &two).0.is_some(),
        "but it is news again after a quiet launch"
    );
}

#[test]
fn nothing_found_and_nothing_recorded_writes_no_file() {
    let dir = ScratchDir::new("eitri-root-conflicts", "quiet");
    let file = dir.join("root-key-conflicts");
    let (toast, err) = announce(Some(&file), &[]);
    assert!(toast.is_none() && err.is_none());
    assert!(!file.exists());
    assert_eq!(announce(None, &[]).0, None);
}

#[test]
fn without_a_state_directory_the_toast_shows_every_launch() {
    let table = root(&[("C-Space", Action::Zoom)]);
    let found = conflicts(&table, &[Im::Fcitx], &[]);
    assert!(announce(None, &found).0.is_some());
}

#[test]
fn the_state_file_is_under_eitris_state_directory() {
    let state = std::ffi::OsStr::new("/state");
    assert_eq!(
        state_file(Some(state), None),
        Some(PathBuf::from("/state/eitri/root-key-conflicts"))
    );
    assert_eq!(state_file(None, None), None);
}

#[test]
fn the_log_line_and_the_toast_hold_no_text_of_a_reply_but_the_chords_own_spelling() {
    let hostile = "<Alt>x";
    let table = root(&[("M-x", Action::Zoom)]);
    // A reply whose second field is hostile text: the field is kept for the caller, never printed.
    let mut found = chords(&[hostile]);
    found[0].1 = "\u{1b}[31mowned\u{1b}[0m rm -rf".to_string();
    let cs = conflicts(&table, &[], &[(Source::Sway, found)]);
    assert_eq!(cs.len(), 1);
    let line = log_line(&cs[0]);
    let toast = toast_text(&cs);
    for text in [&line, &toast] {
        assert!(
            !text.contains('\u{1b}') && !text.contains("owned") && !text.contains("rm -rf"),
            "{text:?}"
        );
        assert!(text.contains("Alt+x") && text.contains("sway"), "{text:?}");
    }
    assert_eq!(line, "[keys] root key Alt+x (zoom): sway binds it; Eitri never sees it");
}

#[test]
fn the_toast_names_four_and_counts_the_rest() {
    let keys = ["M-a", "M-b", "M-c", "M-d", "M-e", "M-f"];
    let table = root(&keys.map(|k| (k, Action::Zoom)));
    let desktop = vec![(
        Source::Hyprland,
        chords(&["<Alt>a", "<Alt>b", "<Alt>c", "<Alt>d", "<Alt>e", "<Alt>f"]),
    )];
    let cs = conflicts(&table, &[], &desktop);
    assert_eq!(cs.len(), 6);
    let toast = toast_text(&cs);
    assert!(toast.contains("and 2 more"), "{toast}");
    assert!(
        toast.contains("Alt+a (Hyprland)") && !toast.contains("Alt+f"),
        "{toast}"
    );
}

#[test]
fn a_compositors_key_is_read_in_the_keymaps_spelling() {
    let human = |ctrl, alt, shift, key| chord_of_parts(ctrl, alt, shift, key).map(|c| c.human());
    assert_eq!(human(true, false, false, "space").as_deref(), Some("Ctrl+space"));
    assert_eq!(human(false, true, false, "Return").as_deref(), Some("Alt+Return"));
    assert_eq!(human(false, true, false, "RETURN").as_deref(), Some("Alt+Return"));
    assert_eq!(human(true, false, false, "f5").as_deref(), Some("Ctrl+F5"));
    assert_eq!(human(false, true, false, "T").as_deref(), Some("Alt+t"));
    assert_eq!(human(false, true, false, "comma").as_deref(), Some("Alt+,"));
    assert_eq!(human(false, true, false, "Prior").as_deref(), Some("Alt+Page_Up"));
    for bad in ["", "<Alt>", "a b", "t\u{1b}", "mouse:272", "é", &"k".repeat(40)] {
        assert_eq!(human(true, false, false, bad), None, "{bad:?}");
    }
}

#[test]
fn a_shifted_symbol_meets_the_bind_that_names_it_with_shift_held() {
    use super::sway_chords;
    let sway = |bind: &str| sway_chords(&serde_json::json!({ "config": bind }).to_string()).unwrap();
    let table = root(&[("M-!", Action::Zoom)]);
    let found = conflicts(&table, &[], &[(Source::Sway, sway("bindsym Mod1+Shift+exclam exec a"))]);
    assert_eq!(found.len(), 1, "{found:?}");
    let plain = conflicts(&table, &[], &[(Source::Sway, sway("bindsym Mod1+exclam exec a"))]);
    assert_eq!(plain.len(), 1, "{plain:?}");
    let table = root(&[("M--", Action::Zoom)]);
    assert!(conflicts(&table, &[], &[(Source::Sway, sway("bindsym Mod1+Shift+minus exec a"))]).is_empty());
}

#[test]
fn a_root_space_key_meets_a_compositors_space() {
    let table = root(&[("C-Space", Action::Zoom)]);
    for source in [Source::Sway, Source::Hyprland, Source::Gnome] {
        let chord = chord_of_parts(true, false, false, "space").unwrap();
        let desktop = vec![(source, vec![(chord, String::new())])];
        assert_eq!(conflicts(&table, &[], &desktop).len(), 1, "{source:?}");
    }
}

#[test]
fn a_punctuation_key_named_in_full_meets_the_character_a_root_key_uses() {
    use super::{gnome_chords, sway_chords};
    let sway = |bind: &str| sway_chords(&serde_json::json!({ "config": bind }).to_string()).unwrap();
    let gnome = |accel: &str| gnome_chords(&[("k".to_string(), accel.to_string())]);
    for (key, sway_bind, gnome_accel) in [
        ("M-`", "bindsym Mod1+grave exec a", "<Alt>grave"),
        ("M-[", "bindsym Mod1+bracketleft exec a", "<Alt>bracketleft"),
        ("M-]", "bindsym Mod1+bracketright exec a", "<Alt>bracketright"),
        ("M-'", "bindsym Mod1+apostrophe exec a", "<Alt>apostrophe"),
        ("M-,", "bindsym Mod1+comma exec a", "<Alt>comma"),
        ("M-\\", "bindsym Mod1+backslash exec a", "<Alt>backslash"),
        ("M-`", "bindsym Mod1+Grave exec a", "<Alt>GRAVE"),
    ] {
        let table = root(&[(key, Action::Zoom)]);
        for (source, chords) in [(Source::Sway, sway(sway_bind)), (Source::Gnome, gnome(gnome_accel))] {
            assert_eq!(conflicts(&table, &[], &[(source, chords)]).len(), 1, "{key} {source:?}");
        }
    }
    let table = root(&[("M-`", Action::Zoom)]);
    assert!(conflicts(&table, &[], &[(Source::Sway, sway("bindsym Mod1+bracketleft exec a"))]).is_empty());
}
