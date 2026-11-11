//! Slice U2 of `docs/UPDATES.md` 14: the pack and sign tools.

use std::fs;
use std::path::{Path, PathBuf};

use ring::rand::SystemRandom;
use ring::signature::Ed25519KeyPair;
#[cfg(unix)]
use swapdesk_release_tools::ToolError;
use swapdesk_release_tools::{manifest_for, pack, sign_release};
use swapdesk_update::manifest::{public_key, verify_with};
use swapdesk_update::{Apply, Channel, Package, sha256};

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("swapdesk-release-tools-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

#[test]
fn pack_then_read_gives_the_same_tree() {
    let root = temp("tree");
    put(&root, "Swapdesk.exe", b"MZ app");
    put(&root, "swapdesk-service.exe", &[3; 5000]);
    put(&root, "Swapdesk.app/Contents/Info.plist", b"<plist/>");
    put(&root, "Swapdesk.app/Contents/MacOS/Swapdesk", b"\xcf\xfa\xed\xfe");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let exe = root.join("Swapdesk.app/Contents/MacOS/Swapdesk");
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        for f in ["Swapdesk.exe", "swapdesk-service.exe"] {
            fs::set_permissions(root.join(f), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let package = pack(&root).unwrap();
    let back = Package::read(&package.write().unwrap()).unwrap();
    assert_eq!(back, package);
    let mut paths: Vec<&str> = back.entries.iter().map(|e| e.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "Swapdesk.app/Contents/Info.plist",
            "Swapdesk.app/Contents/MacOS/Swapdesk",
            "Swapdesk.exe",
            "swapdesk-service.exe"
        ]
    );
    for (path, mode) in [
        ("Swapdesk.exe", 0o755),
        ("swapdesk-service.exe", 0o755),
        ("Swapdesk.app/Contents/MacOS/Swapdesk", 0o755),
        ("Swapdesk.app/Contents/Info.plist", 0o644),
    ] {
        assert_eq!(back.get(path).unwrap().mode, mode, "{path}");
    }
    assert_eq!(back.get("swapdesk-service.exe").unwrap().bytes, vec![3; 5000]);
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn the_pack_tool_refuses_a_symlink() {
    let root = temp("link");
    put(&root, "a.txt", b"a");
    std::os::unix::fs::symlink(root.join("a.txt"), root.join("b.txt")).unwrap();
    assert!(matches!(pack(&root), Err(ToolError::Symlink(p)) if p.ends_with("b.txt")));
    let _ = fs::remove_dir_all(&root);
}

fn key() -> Vec<u8> {
    Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap().as_ref().to_vec()
}

#[test]
fn sign_then_verify_with_the_compiled_key_format() {
    let root = temp("sign");
    let tree = temp("sign-tree");
    put(&tree, "Swapdesk.exe", b"MZ");
    let pkg = root.join("Swapdesk-windows-x64-1.1.1.sdpkg");
    fs::write(&pkg, pack(&tree).unwrap().write().unwrap()).unwrap();
    let pkcs8 = key();
    let manifest = manifest_for("1.1.1", Apply::Auto, &[("windows-x64".into(), pkg.clone())]).unwrap();
    assert_eq!(manifest.channel, Channel::Stable);
    let (text, sig) = sign_release(&manifest, &pkcs8).unwrap();
    // The key as keys.rs holds it: 32 bytes.
    let public: [u8; 32] = public_key(&pkcs8).unwrap();
    let line = swapdesk_release_tools::key_line(&public, "A");
    assert_eq!(line.matches("0x").count(), 32);
    let read = verify_with(text.as_bytes(), sig.as_bytes(), &[public]).unwrap();
    assert_eq!(read.files[0].name, "Swapdesk-windows-x64-1.1.1.sdpkg");
    // A pre-release names the preview channel.
    let rc = manifest_for("1.2.0-rc.1", Apply::Auto, &[("windows-x64".into(), pkg)]).unwrap();
    assert_eq!(rc.channel, Channel::Preview);
    for d in [root, tree] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn the_sign_tool_hashes_the_files_it_was_given_not_their_sha256_files() {
    let root = temp("hash");
    let tree = temp("hash-tree");
    put(&tree, "Swapdesk.exe", b"MZ real");
    let pkg = root.join("Swapdesk-windows-x64-1.1.1.sdpkg");
    let bytes = pack(&tree).unwrap().write().unwrap();
    fs::write(&pkg, &bytes).unwrap();
    // A .sha256 beside it that lies.
    fs::write(root.join("Swapdesk-windows-x64-1.1.1.sdpkg.sha256"), format!("{}  x\n", "0".repeat(64))).unwrap();
    let m = manifest_for("1.1.1", Apply::Auto, &[("windows-x64".into(), pkg.clone())]).unwrap();
    assert_eq!(m.files[0].sha256, sha256(&bytes));
    assert_eq!(m.files[0].size, bytes.len() as u64);
    // A file that is not a package is refused: a release never ships one its installs would refuse.
    fs::write(&pkg, b"not a package").unwrap();
    assert!(manifest_for("1.1.1", Apply::Auto, &[("windows-x64".into(), pkg)]).is_err());
    for d in [root, tree] {
        let _ = fs::remove_dir_all(d);
    }
}
