//! The `Store` fixture itself: iets and Nix build into the same private store.
#![cfg(all(unix, feature = "cli"))]

mod e2e;
mod hermetic;

use std::fs;

use e2e::{ok, stdout};

#[test]
fn iets_builds_a_chain_that_nix_sees() {
    let s = store!();
    let chain = s.pkgs().join("chain.nix");
    let out = stdout(&ok(s.iets().arg("build").arg("-f").arg(&chain)));
    assert!(out.starts_with(s.store_dir().to_str().unwrap()), "{out}");
    let closure = ok(s.nix("nix-store").args(["-qR", &out]));
    assert_eq!(stdout(&closure).lines().count(), 4);
    assert_eq!(fs::read_to_string(format!("{out}/n")).unwrap(), "3\n");

    // warm: nothing left to build, same path
    let again = stdout(&ok(s.iets().arg("build").arg("-f").arg(&chain)));
    assert_eq!(again, out);
}

#[test]
fn nix_build_and_iets_nix_build_agree() {
    let s = store!();
    let multi = s.pkgs().join("multi.nix");
    let theirs = ok(s.nix("nix-build").arg("--no-out-link").arg(&multi));
    let ours = ok(s.iets().arg("nix-build").arg("--no-out-link").arg(&multi));
    assert_eq!(stdout(&ours), stdout(&theirs));
    let drv = ok(s.iets().arg("nix-instantiate").arg(&multi));
    let nix_drv = ok(s.nix("nix-instantiate").arg(&multi));
    assert_eq!(stdout(&drv), stdout(&nix_drv));
}

#[test]
fn drop_stops_every_daemon_process() {
    let s = store!();
    ok(s.iets()
        .arg("build")
        .arg("-f")
        .arg(s.pkgs().join("multi.nix")));
    let pgid = s.daemon_pid() as i32;
    drop(s);
    // SAFETY: signal 0 only checks that the group exists
    assert_eq!(unsafe { libc::kill(-pgid, 0) }, -1);
}
