//! Pure evaluation is the native default, and a checkout with no flake is
//! what a pure run may read (docs/explanation/flakes-and-sources.md).

mod hermetic;

use hermetic::{Iets, bin, stderr, stdout};
use rstest::rstest;
use serde_json::Value;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Output;
use tempfile::{TempDir, tempdir};

fn run(bin: &Iets, dir: &Path, args: &[&str]) -> Output {
    bin.cmd()
        .args(args)
        .current_dir(dir)
        .env("IETS_CACHE", "0")
        .output()
        .expect("run iets")
}

/// A project holding `repo/read.nix`, which reads `repo/in.txt`, and
/// `outside.txt` beside the project. The marker is an `iets.toml`: a working
/// tree would need a real SCM here, and `project::tests` covers the markers
/// that `source::scm` supplies.
fn checkout() -> TempDir {
    let d = tempdir().unwrap();
    let repo = d.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("iets.toml"), "").unwrap();
    fs::write(d.path().join("outside.txt"), "out").unwrap();
    fs::write(repo.join("in.txt"), "in").unwrap();
    fs::write(repo.join("read.nix"), "builtins.readFile ./in.txt").unwrap();
    fs::write(repo.join("up.nix"), "builtins.readFile ../outside.txt").unwrap();
    fs::write(repo.join("env.nix"), "builtins.getEnv \"IETS_TEST_VAR\"").unwrap();
    d
}

#[rstest]
fn a_read_in_the_project_is_pure_and_one_above_it_is_not(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");

    let o = run(&bin, &repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "in");

    let o = run(&bin, &repo, &["eval", "--raw", "-f", "up.nix"]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(
        e.contains("outside.txt' is forbidden in pure evaluation mode"),
        "{e}"
    );

    let o = run(&bin, &repo, &["eval", "--raw", "--impure", "-f", "up.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "out");
}

/// A link or a `..` that leads out of the project is read as what it reaches.
#[cfg(unix)]
#[rstest]
#[case::file_link("builtins.readFile ./file-link")]
#[case::dir_link("builtins.readFile ./dir-link/outside.txt")]
#[case::import_link("import ./nix-link")]
#[case::home_dots("builtins.readFile ~/repo/../outside.txt")]
#[case::path_exists("if builtins.pathExists ./dir-link/outside.txt then \"\" else 0")]
fn a_link_out_of_the_project_is_forbidden(bin: Iets, #[case] expr: &str) {
    let d = checkout();
    let repo = d.path().join("repo");
    fs::write(d.path().join("outside.nix"), "1").unwrap();
    symlink("../outside.txt", repo.join("file-link")).unwrap();
    symlink("..", repo.join("dir-link")).unwrap();
    symlink("../outside.nix", repo.join("nix-link")).unwrap();
    fs::write(repo.join("e.nix"), expr).unwrap();
    let o = bin
        .cmd()
        .args(["eval", "--raw", "-f", "e.nix"])
        .current_dir(&repo)
        .env("HOME", d.path())
        .env("IETS_CACHE", "0")
        .output()
        .expect("run iets");
    if expr.contains("pathExists") {
        assert!(
            stderr(&o).contains("cannot coerce an integer"),
            "{}",
            stderr(&o)
        );
        return;
    }
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(e.contains("is forbidden in pure evaluation mode"), "{e}");
}

/// No marker above the entry file: its own directory is the project, so a
/// one-off expression in a scratch directory still evaluates.
#[rstest]
fn a_directory_with_no_marker_is_its_own_project(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");
    fs::remove_file(repo.join("iets.toml")).unwrap();
    let o = run(&bin, &repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "in");
}

#[rstest]
fn the_other_purity_rules_hold_without_a_flag(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");

    let o = run(&bin, &repo, &["eval", "--raw", "-f", "env.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");

    let o = run(&bin, &repo, &["eval", "-E", "builtins ? currentSystem"]);
    assert_eq!(stdout(&o), "false");
}

/// The user's file turns it off for every run; a project's file may not.
#[rstest]
fn the_pure_setting_is_the_users_alone(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");
    let conf = d.path().join("config/iets");
    fs::create_dir_all(&conf).unwrap();
    fs::write(conf.join("iets.toml"), "pure = false\n").unwrap();

    let o = bin
        .cmd()
        .args(["eval", "--raw", "-f", "up.nix"])
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", d.path().join("config"))
        .env("IETS_CACHE", "0")
        .output()
        .expect("run iets");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "out");

    fs::write(repo.join("iets.toml"), "pure = false\n").unwrap();
    let o = run(&bin, &repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(e.contains("'pure' may not be set in a project file"), "{e}");
}

/// A `narHash` locks a `path:` reference, so a pure evaluation takes it when
/// the tree matches, refuses a tree that does not, and refuses no hash at all.
#[rstest]
fn get_flake_takes_a_path_its_nar_hash_locks(bin: Iets) {
    let d = tempdir().unwrap();
    fs::write(d.path().join("flake.nix"), "{ outputs = _: { x = 42; }; }").unwrap();
    fs::write(
        d.path().join("flake.lock"),
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    )
    .unwrap();
    // the evaluator's form, `/C:/...` on Windows, keeps backslashes out of the string
    let at = String::from_utf8(iets::sys::path_to_internal(d.path())).unwrap();

    let tree = format!(r#"(builtins.fetchTree {{ type = "path"; path = "{at}"; }}).narHash"#);
    let o = run(
        &bin,
        d.path(),
        &["eval", "--impure", "--raw", "--expr", &tree],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let hash = stdout(&o);

    let get = |query: &str| {
        let e = format!(r#"(builtins.getFlake "path:{at}{query}").x"#);
        run(&bin, d.path(), &["eval", "--expr", &e])
    };
    let right = get(&format!("?narHash={hash}"));
    let wrong = get("?narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    let unpinned = get("");

    assert_eq!(right.status.code(), Some(0), "{}", stderr(&right));
    assert_eq!(right.stdout, "42\n".as_bytes());
    assert_eq!(wrong.status.code(), Some(1), "{}", stdout(&wrong));
    let e = stderr(&wrong);
    assert!(e.contains("NAR hash mismatch"), "{e}");
    let e = stderr(&unpinned);
    assert!(e.contains("unlocked flake reference"), "{e}");

    let tree = format!(r#"builtins.fetchTree {{ type = "path"; path = "{at}"; narHash = true; }}"#);
    let o = run(&bin, d.path(), &["eval", "--expr", &tree]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(
        e.contains("input attribute 'narHash' is not a string"),
        "{e}"
    );
}

/// A `path:` reference may carry the `rev`, `revCount` and `lastModified` of
/// the repository its tree came from, as path.cc takes them, and `self`
/// shows them as emitTreeAttrs does: the rev in lowercase hex, its first
/// seven digits, the count, the time and its date. The values are what Nix
/// 2.34.8 gives for the same references.
#[rstest]
fn get_flake_gives_self_the_source_information_of_a_path(bin: Iets) {
    let d = tempdir().unwrap();
    fs::write(d.path().join("flake.nix"), "{ outputs = _: { }; }").unwrap();
    fs::write(
        d.path().join("flake.lock"),
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    )
    .unwrap();
    let at = String::from_utf8(iets::sys::path_to_internal(d.path())).unwrap();
    let tree = format!(r#"(builtins.fetchTree {{ type = "path"; path = "{at}"; }}).narHash"#);
    let o = run(
        &bin,
        d.path(),
        &["eval", "--impure", "--raw", "--expr", &tree],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let hash = stdout(&o);

    let get = |query: &str, flags: &[&str]| {
        let e = format!(
            r#"let f = builtins.getFlake "path:{at}{query}"; in builtins.toJSON {{
              names = builtins.attrNames f.sourceInfo;
              rev = f.rev or null; shortRev = f.shortRev or null; revCount = f.revCount or null;
              lastModified = if f.lastModified == f.sourceInfo.lastModified then f.lastModified else null;
              lastModifiedDate = f.lastModifiedDate;
            }}"#
        );
        let args = [&["eval", "--raw"][..], flags, &["--expr", e.as_str()][..]].concat();
        let o = run(&bin, d.path(), &args);
        (o.status.code(), stdout(&o), stderr(&o))
    };
    let json = |query: &str, flags: &[&str]| {
        let (code, out, err) = get(query, flags);
        assert_eq!(code, Some(0), "{query}: {err}");
        serde_json::from_str::<serde_json::Value>(&out).unwrap()
    };
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let pinned = format!("?narHash={hash}");

    let all = json(
        &format!("{pinned}&rev={rev}&revCount=42&lastModified=1585388205"),
        &[],
    );
    assert_eq!(
        all,
        serde_json::json!({
            "names": ["lastModified", "lastModifiedDate", "narHash", "outPath", "rev", "revCount", "shortRev"],
            "rev": rev, "shortRev": "0123456", "revCount": 42,
            "lastModified": 1585388205, "lastModifiedDate": "20200328093645",
        })
    );

    // each alone; a tree outside the store has its newest mtime
    let none = json(&pinned, &[]);
    assert_eq!(
        none["names"],
        serde_json::json!(["lastModified", "lastModifiedDate", "narHash", "outPath"])
    );
    assert_eq!(
        (&none["rev"], &none["revCount"]),
        (&Value::Null, &Value::Null)
    );
    assert!(none["lastModified"].as_u64().unwrap() > 1);
    let one = json(&format!("{pinned}&rev={rev}"), &[]);
    assert_eq!(
        (&one["rev"], &one["shortRev"]),
        (&all["rev"], &all["shortRev"])
    );
    assert_eq!(one["revCount"], Value::Null);
    let one = json(&format!("{pinned}&revCount=42"), &[]);
    assert_eq!(
        (&one["rev"], &one["revCount"]),
        (&Value::Null, &all["revCount"])
    );
    let one = json(&format!("{pinned}&lastModified=1585388205"), &[]);
    assert_eq!(one["lastModifiedDate"], all["lastModifiedDate"]);
    assert_eq!(one["rev"], Value::Null);

    // Input::getRev reads any SHA-1 form, and a prefixed hash of another type
    for given in [
        "0123456789ABCDEF0123456789abcdef01234567",
        "sha1-ASNFZ4mrze8BI0VniavN7wEjRWc=",
    ] {
        let one = json(&format!("{pinned}&rev={given}"), &[]);
        assert_eq!(one["rev"], rev, "{given}");
    }
    let long = format!("sha256:{}", "a".repeat(64));
    let one = json(&format!("{pinned}&rev={long}"), &[]);
    assert_eq!(one["shortRev"], "aaaaaaa");
    let (code, _, err) = get(&format!("{pinned}&rev=0123"), &[]);
    assert_eq!(code, Some(1));
    assert!(err.contains("'0123'"), "{err}");

    // without a narHash it is not locked, rev or not
    let (code, _, err) = get(&format!("?rev={rev}"), &[]);
    assert_eq!(code, Some(1));
    assert!(err.contains("unlocked flake reference"), "{err}");
    assert_eq!(json(&format!("?rev={rev}"), &["--impure"])["rev"], rev);
}
