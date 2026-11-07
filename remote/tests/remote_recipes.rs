use eitri_remote::serve::config::{Listen, ServeConfig};
use eitri_remote::serve::origin::{check_request, HOST_REFUSED, ORIGIN_REFUSED};

fn recipe(listen: Listen, host: &str, origin: &str, allowed: &[&str]) {
    let cfg = ServeConfig {
        listen,
        port: 8765,
        origins: allowed.iter().map(|s| s.to_string()).collect(),
        audit_keep_days: eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT,
        notify: true,
    };
    assert_eq!(check_request(host, Some(origin), &cfg), Ok(()));
    assert_eq!(check_request(host, None, &cfg), Ok(()));
    assert_eq!(check_request("foreign.example", Some(origin), &cfg), Err(HOST_REFUSED));
    assert_eq!(
        check_request(host, Some("https://foreign.example"), &cfg),
        Err(ORIGIN_REFUSED)
    );
}

#[test]
fn remote_recipe_wireguard_address() {
    recipe(
        Listen::Address {
            addr: "198.51.100.1".parse().unwrap(),
            iface: "wg0".into(),
        },
        "198.51.100.1:8765",
        "http://198.51.100.1:8765",
        &[],
    );
}

#[test]
fn remote_recipe_ssh_another_loopback_port() {
    recipe(Listen::Loopback, "127.0.0.1:18765", "http://127.0.0.1:18765", &[]);
}

#[test]
fn remote_recipe_unix_socket_forward_localhost() {
    recipe(Listen::Socket, "localhost:18765", "http://localhost:18765", &[]);
}

#[test]
fn remote_recipe_tailscale_serve() {
    recipe(
        Listen::Loopback,
        "desk.tailnet.ts.net",
        "https://desk.tailnet.ts.net",
        &["https://desk.tailnet.ts.net"],
    );
}

#[test]
fn remote_recipe_nginx_upstream_host() {
    recipe(
        Listen::Loopback,
        "127.0.0.1:8765",
        "https://eitri.example.org",
        &["https://eitri.example.org"],
    );
}

#[test]
fn remote_recipe_caddy_client_host() {
    recipe(
        Listen::Loopback,
        "eitri.example.org",
        "https://eitri.example.org",
        &["https://eitri.example.org"],
    );
}
