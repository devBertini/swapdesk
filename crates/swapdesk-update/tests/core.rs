//! Slice U1 of `docs/UPDATES.md` 14: the manifest and its signature, the version rule and the package.

use proptest::prelude::*;
use ring::rand::SystemRandom;
use ring::signature::Ed25519KeyPair;
use swapdesk_update::manifest::{self, MAX_MANIFEST, parse, public_key, sign, verify, verify_with};
use swapdesk_update::package::path_ok;
use swapdesk_update::{
    Apply, Channel, Entry, FileEntry, Manifest, Offer, Package, UpdateError, Version, offer, sha256,
};

/// A fresh Ed25519 key: its PKCS#8 document and its public half.
fn key() -> (Vec<u8>, [u8; 32]) {
    let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).expect("a key");
    let pkcs8 = doc.as_ref().to_vec();
    let public = public_key(&pkcs8).expect("its public half");
    (pkcs8, public)
}

fn v(s: &str) -> Version {
    Version::parse(s).expect(s)
}

fn sample(version: &str, channel: Channel) -> Manifest {
    Manifest {
        version: v(version),
        channel,
        apply: Apply::Auto,
        files: vec![
            FileEntry {
                target: "windows-x64".into(),
                name: format!("Swapdesk-windows-x64-{version}.sdpkg"),
                size: 12_401_664,
                sha256: [0xab; 32],
            },
            FileEntry {
                target: "macos-arm64".into(),
                name: format!("Swapdesk-macos-arm64-{version}.sdpkg"),
                size: 13_107_200,
                sha256: [0xcd; 32],
            },
        ],
    }
}

#[test]
fn a_manifest_signed_by_a_release_key_verifies_and_any_flipped_byte_fails() {
    let (pkcs8, public) = key();
    let text = sample("1.1.1", Channel::Stable).to_text();
    let sig = sign(text.as_bytes(), &pkcs8).unwrap();
    let m = verify_with(text.as_bytes(), sig.as_bytes(), &[public]).expect("verifies");
    assert_eq!(m, sample("1.1.1", Channel::Stable));
    // Every byte of the manifest is covered.
    for i in 0..text.len() {
        let mut bad = text.clone().into_bytes();
        bad[i] ^= 0x01;
        assert!(verify_with(&bad, sig.as_bytes(), &[public]).is_err(), "byte {i} flipped still verified");
    }
    // And of the signature itself (its hex digits).
    let sig_at = sig
        .rfind(
            "
sig ",
        )
        .unwrap()
        + 5;
    let mut bad = sig.clone().into_bytes();
    bad[sig_at] = if bad[sig_at] == b'0' { b'1' } else { b'0' };
    assert_eq!(verify_with(text.as_bytes(), &bad, &[public]), Err(UpdateError::BadSignature));
    // A rotation key verifies as well: either compiled key is enough.
    let (other_pkcs8, other) = key();
    let sig_b = sign(text.as_bytes(), &other_pkcs8).unwrap();
    assert!(verify_with(text.as_bytes(), sig_b.as_bytes(), &[public, other]).is_ok());
}

#[test]
fn a_manifest_signed_by_an_unknown_key_is_refused() {
    let (pkcs8, _) = key();
    let (_, trusted) = key();
    let text = sample("1.1.1", Channel::Stable).to_text();
    let sig = sign(text.as_bytes(), &pkcs8).unwrap();
    assert_eq!(verify_with(text.as_bytes(), sig.as_bytes(), &[trusted]), Err(UpdateError::UnknownKey));
    // This build has no release key yet (the ceremony of slice U2): nothing verifies against the compiled list.
    assert_eq!(verify(text.as_bytes(), sig.as_bytes()), Err(UpdateError::UnknownKey));
}

#[test]
fn a_signature_without_the_domain_prefix_is_refused() {
    let (pkcs8, public) = key();
    let text = sample("1.1.1", Channel::Stable).to_text();
    // The key signed the manifest's bytes alone, as it might for another purpose.
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).unwrap();
    let raw = pair.sign(text.as_bytes());
    let id: String = sha256(&public)[..8].iter().map(|b| format!("{b:02x}")).collect();
    let hexsig: String = raw.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let sig = format!("swapdesk-update-sig 1\nkey {id}\nsig {hexsig}\n");
    assert_eq!(verify_with(text.as_bytes(), sig.as_bytes(), &[public]), Err(UpdateError::BadSignature));
    assert_eq!(manifest::DOMAIN, b"swapdesk-update-manifest\n");
}

#[test]
fn a_version_equal_or_lower_than_the_running_one_is_never_offered() {
    let m = sample("1.1.1", Channel::Stable);
    for running in ["1.1.1", "1.1.2", "1.2.0", "2.0.0", "10.0.0"] {
        assert_eq!(offer(&m, &v(running), "windows-x64", &[]).unwrap(), Offer::Nothing, "running {running}");
    }
    match offer(&m, &v("1.1.0"), "windows-x64", &[]).unwrap() {
        Offer::Install { version, entry } => {
            assert_eq!(version, &v("1.1.1"));
            assert_eq!(entry.target, "windows-x64");
        }
        other => panic!("{other:?}"),
    }
    // A version that was installed and rolled back is not offered again by itself.
    assert_eq!(offer(&m, &v("1.1.0"), "windows-x64", &[v("1.1.1")]).unwrap(), Offer::Nothing);
    // A release that needs the installer is a link, not a button.
    let mut manual = m.clone();
    manual.apply = Apply::Manual;
    assert_eq!(offer(&manual, &v("1.1.0"), "macos-arm64", &[]).unwrap(), Offer::Manual { version: &v("1.1.1") });
    // A manifest without this system's package says so.
    let mut only_mac = m.clone();
    only_mac.files.retain(|f| f.target == "macos-arm64");
    assert_eq!(offer(&only_mac, &v("1.1.0"), "windows-x64", &[]), Err(UpdateError::NoEntry));
}

#[test]
fn ten_is_greater_than_nine_in_every_position() {
    assert!(v("1.10.0") > v("1.9.0"));
    assert!(v("10.0.0") > v("9.9.9"));
    assert!(v("1.1.10") > v("1.1.9"));
    assert!(v("1.1.0") > v("1.1.0-rc.10"));
    assert!(v("1.1.0-rc.10") > v("1.1.0-rc.9"));
    assert!(v("1.1.0-rc.1") > v("1.1.0-preview.9"), "words compare as text");
    assert!(v("1.1.0-rc.1.1") > v("1.1.0-rc.1"), "a longer list sorts after its prefix");
    assert_eq!(v("1.10.0").to_string(), "1.10.0");
    for bad in ["1.1", "1.1.1.1", "01.1.1", "1.1.-1", "a.b.c", "1.1.1-", "1.1.1-rc..1", "1.1.1-rc/1", " 1.1.1", ""] {
        assert!(Version::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_preview_manifest_is_never_offered_on_the_stable_channel() {
    let preview = sample("1.2.0", Channel::Preview);
    assert_eq!(offer(&preview, &v("1.1.0"), "windows-x64", &[]).unwrap(), Offer::Nothing);
    // A pre-release version is not offered even when the manifest calls itself stable.
    let rc = sample("1.2.0-rc.1", Channel::Stable);
    assert_eq!(offer(&rc, &v("1.1.0"), "windows-x64", &[]).unwrap(), Offer::Nothing);
}

#[test]
fn an_oversized_manifest_or_an_unknown_or_repeated_key_is_refused() {
    let good = sample("1.1.1", Channel::Stable).to_text();
    assert!(parse(good.as_bytes()).is_ok());
    let mut big = good.clone();
    while big.len() <= MAX_MANIFEST {
        big += "file other-x64 Swapdesk.sdpkg 1 ";
        big += &"0".repeat(64);
        big += "\n";
    }
    assert_eq!(parse(big.as_bytes()), Err(UpdateError::TooLarge));
    let cases = [
        (good.replace("swapdesk-update 1", "swapdesk-update 2"), "a new first line"),
        (good.clone() + "colour blue\n", "an unknown key"),
        (good.clone() + "channel stable\n", "a repeated key"),
        (good.clone() + "file windows-x64 X.sdpkg 1 " + &"0".repeat(64) + "\n", "a second package for one system"),
        (good.replace("apply auto\n", ""), "a missing key"),
        (good.replace('\n', "\r\n"), "CRLF"),
        (good.trim_end().to_string(), "no last LF"),
        (good.replace("12401664", "0"), "an empty package"),
        (good.replace("12401664", "268435457"), "a package over the cap"),
        (good.replace("abababab", "ABABABAB"), "upper-case hex"),
        (good.replace("Swapdesk-windows", "../Swapdesk-windows"), "a path in the name"),
    ];
    for (text, what) in cases {
        assert!(parse(text.as_bytes()).is_err(), "{what} was read");
    }
    // A signature file that is too long is refused before anything is looked up.
    let (_, public) = key();
    let long_sig = "x".repeat(300);
    assert_eq!(verify_with(good.as_bytes(), long_sig.as_bytes(), &[public]), Err(UpdateError::TooLarge));
}

fn tree() -> Package {
    Package {
        entries: vec![
            Entry { path: "Swapdesk.exe".into(), mode: 0o755, bytes: b"MZ one".to_vec() },
            Entry { path: "swapdesk-service.exe".into(), mode: 0o755, bytes: vec![7; 1000] },
            Entry { path: "Swapdesk.app/Contents/Info.plist".into(), mode: 0o644, bytes: b"<plist/>".to_vec() },
            Entry { path: "empty.txt".into(), mode: 0o644, bytes: Vec::new() },
        ],
    }
}

#[test]
fn a_package_round_trips_with_its_modes() {
    let p = tree();
    let bytes = p.write().unwrap();
    assert_eq!(&bytes[..8], b"SDPKG1\n\0");
    let back = Package::read(&bytes).unwrap();
    assert_eq!(back, p);
    assert_eq!(back.get("Swapdesk.exe").unwrap().mode, 0o755);
    assert_eq!(back.get("Swapdesk.app/Contents/Info.plist").unwrap().mode, 0o644);
    // Another mode is refused both ways.
    let mut odd = tree();
    odd.entries[0].mode = 0o777;
    assert!(odd.write().is_err());
    let mut raw = p.write().unwrap();
    // The first entry's mode sits after the magic, the count, the path length and the path.
    let at = 8 + 4 + 2 + "Swapdesk.exe".len();
    raw[at..at + 2].copy_from_slice(&0o777u16.to_le_bytes());
    assert!(Package::read(&raw).is_err());
    // Trailing bytes are refused.
    let mut trailing = p.write().unwrap();
    trailing.push(0);
    assert!(Package::read(&trailing).is_err());
}

#[test]
fn a_package_path_with_dotdot_absolute_backslash_or_a_duplicate_is_refused() {
    for bad in
        ["../evil.exe", "a/../b", "/abs", "a\\b", "C:evil", "a//b", "./a", "a/.", "", "sp ace", "ünicode", "trailing/"]
    {
        assert!(!path_ok(bad), "{bad:?}");
        let p = Package { entries: vec![Entry { path: bad.into(), mode: 0o644, bytes: vec![1] }] };
        assert!(p.write().is_err(), "{bad:?} was written");
    }
    let dup = Package {
        entries: vec![
            Entry { path: "a.exe".into(), mode: 0o644, bytes: vec![1] },
            Entry { path: "a.exe".into(), mode: 0o644, bytes: vec![2] },
        ],
    };
    assert_eq!(dup.write(), Err(UpdateError::BadPath));
    let file_and_folder = Package {
        entries: vec![
            Entry { path: "a".into(), mode: 0o644, bytes: vec![1] },
            Entry { path: "a/b".into(), mode: 0o644, bytes: vec![2] },
        ],
    };
    assert_eq!(file_and_folder.write(), Err(UpdateError::BadPath));
    // A package written by someone else with a bad path is refused on read too.
    let good = Package { entries: vec![Entry { path: "ab/c.exe".into(), mode: 0o644, bytes: vec![1] }] };
    let mut raw = good.write().unwrap();
    let at = 8 + 4 + 2;
    raw[at..at + 3].copy_from_slice(b"../");
    assert!(Package::read(&raw).is_err());
}

#[test]
fn a_package_whose_size_or_hash_differs_is_refused_before_it_is_opened() {
    let bytes = tree().write().unwrap();
    let entry = FileEntry {
        target: "windows-x64".into(),
        name: "Swapdesk-windows-x64-1.1.1.sdpkg".into(),
        size: bytes.len() as u64,
        sha256: sha256(&bytes),
    };
    assert_eq!(Package::read_verified(&bytes, &entry).unwrap(), tree());
    let mut longer = bytes.clone();
    longer.push(0);
    assert_eq!(Package::read_verified(&longer, &entry), Err(UpdateError::Mismatch));
    let mut changed = bytes.clone();
    *changed.last_mut().unwrap() ^= 1;
    assert_eq!(Package::read_verified(&changed, &entry), Err(UpdateError::Mismatch));
    // A broken layout with the manifest's hash is still checked, but the hash came first: garbage that matches
    // nothing never reaches the reader.
    let garbage = vec![0xff; bytes.len()];
    assert_eq!(Package::read_verified(&garbage, &entry), Err(UpdateError::Mismatch));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn random_bytes_never_panic_the_manifest_signature_or_package_readers(
        a in proptest::collection::vec(any::<u8>(), 0..600),
        b in proptest::collection::vec(any::<u8>(), 0..300),
    ) {
        let _ = parse(&a);
        let _ = verify_with(&a, &b, &[[7u8; 32]]);
        let _ = Package::read(&a);
        let mut framed = b"SDPKG1\n\0".to_vec();
        framed.extend_from_slice(&a);
        let _ = Package::read(&framed);
        let text = String::from_utf8_lossy(&a);
        let _ = Version::parse(&text);
    }
}
