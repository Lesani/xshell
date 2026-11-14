#![cfg(unix)]
//! Actual production-workerd inherited-listener reservation and same-storage restart.
#[path = "desktop/workerd.rs"]
#[allow(dead_code)]
mod workerd;
#[test]
fn owned_listener_remains_bound_through_actual_workerd_stop_and_restart() {
    let mut relay = workerd::Relay::start();
    let url = relay.url();
    assert!(
        matches!(std::net::TcpListener::bind(url.strip_prefix("ws://").unwrap()),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse)
    );
    // restart checks the no-child gap and parent CLOEXEC before spawning, then requires
    // an actual HTTP200 from the replacement child under the unchanged readiness bound.
    relay.restart();
    assert_eq!(relay.url(), url);
    assert!(
        matches!(std::net::TcpListener::bind(url.strip_prefix("ws://").unwrap()),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse)
    );
}
