//! Opt-in GTK accelerator checks on an isolated Xvfb: run with `-- --ignored`.

#[path = "support/own_x_server.rs"]
mod own_x_server;

#[allow(dead_code, unused_imports)]
#[path = "../src/gtk_host/keys.rs"]
mod keys;

fn command_accels_parse_in_gtk_as_they_did_before() {
    for original in [
        "<Primary>h",
        "<Control>H",
        "<Control><Alt>r",
        "<primary>t",
        "<control><alt>r",
        "<Super>x",
        "<super>x",
    ] {
        let output = keys::registered_accel(original);
        let parsed = gtk4::accelerator_parse(&output).expect("converted command parses");
        assert_eq!(Some(parsed), gtk4::accelerator_parse(original), "{original}");
    }
    let old = [
        "F11",
        "<Control>equal",
        "<Control>KP_Add",
        "<Control>minus",
        "<Control>KP_Subtract",
        "<Control>0",
        "<Control>KP_0",
        "<Control>KP_Insert",
    ];
    let chords: Vec<_> = eitri_core::keymap::root::ROOT_ACCELS
        .iter()
        .flat_map(|r| r.chords)
        .collect();
    assert_eq!(chords.len(), old.len());
    for (root, original) in chords.into_iter().zip(old) {
        let output = keys::gtk_accel(&root.chord());
        let parsed = gtk4::accelerator_parse(&output).expect("converted root chord parses");
        assert_eq!(Some(parsed), gtk4::accelerator_parse(original), "{original}");
    }
}

fn main() {
    if !std::env::args().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("command_accels: ignored (requires its own Xvfb); run with `-- --ignored`");
        return;
    }
    let _server = own_x_server::init_gtk("command_accels", "1024x768x24");
    command_accels_parse_in_gtk_as_they_did_before();
    println!("command_accels_parse_in_gtk_as_they_did_before: ok");
}
