//! Wire compatibility with 1.0 of the identity and pairing values that `swapdesk-trust` computes (flow F-NET-02):
//! the SPKI of a device key, the device ID, and the hashes of the pairing exchange, as produced by the code of tag
//! `v1.0.0` and kept in `tests/fixtures/v1.0.0/`. The messages of the session protocol are in
//! `swapdesk-proto/tests/wire_1_0.rs`.
//!
//! (a) every fixture decodes into its sample; (b) encoding the sample gives the fixture byte for byte; (c) the
//! set is complete; (d) a flipped byte fails (a) or (b).

mod wire_samples;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use wire_samples::{Item, Sample, file_name, from_hex_text, index_text, samples};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1.0.0")
}

fn fixture(name: &str) -> Vec<u8> {
    let path = fixtures_dir().join(file_name(name));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    from_hex_text(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn check_decodes(sample: &Sample, bytes: &[u8]) -> Result<(), String> {
    let got = sample.item.decode_like(bytes).map_err(|e| format!("{}: does not decode: {e}", sample.name))?;
    if got == sample.item {
        Ok(())
    } else {
        Err(format!("{}: decodes into {got:?}, not {:?}", sample.name, sample.item))
    }
}

fn check_encodes(sample: &Sample, bytes: &[u8]) -> Result<(), String> {
    let sent = sample.item.encode().map_err(|e| format!("{}: does not encode: {e}", sample.name))?;
    if sent == bytes {
        Ok(())
    } else {
        Err(format!("{}: sends {} but the fixture is {}", sample.name, hex(&sent), hex(bytes)))
    }
}

#[test]
fn the_index_names_exactly_the_samples_and_the_fixture_files() {
    let samples = samples();
    let mut names = BTreeSet::new();
    for s in &samples {
        assert!(names.insert(s.name.clone()), "two samples are named {}", s.name);
    }
    let index = fs::read_to_string(fixtures_dir().join("INDEX.txt")).expect("INDEX.txt");
    assert_eq!(index, index_text(&samples), "INDEX.txt differs from the sample table");
    let mut files = BTreeSet::new();
    for entry in fs::read_dir(fixtures_dir()).expect("fixtures folder") {
        let name = entry.expect("entry").file_name().into_string().expect("file name");
        if name != "INDEX.txt" {
            files.insert(name);
        }
    }
    let wanted: BTreeSet<String> = names.iter().map(|n| file_name(n)).collect();
    assert_eq!(files, wanted, "the fixture files and the samples differ");
}

#[test]
fn every_fixture_decodes_into_its_sample() {
    let failures: Vec<String> = samples().iter().filter_map(|s| check_decodes(s, &fixture(&s.name)).err()).collect();
    assert!(failures.is_empty(), "{} fixture(s) fail:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn every_sample_encodes_into_its_fixture_byte_for_byte() {
    let failures: Vec<String> = samples().iter().filter_map(|s| check_encodes(s, &fixture(&s.name)).err()).collect();
    assert!(failures.is_empty(), "{} fixture(s) fail:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn the_fixtures_cover_every_value_this_crate_derives_and_match_published_answers() {
    let all = samples();
    assert!(all.iter().any(|s| matches!(s.item, Item::Identity { .. })));
    assert!(all.iter().any(|s| matches!(s.item, Item::PairingHashes { .. })));
    // SHA-256("abc"), FIPS 180-4 / RFC 6234: an answer that does not come from this code.
    assert_eq!(hex(&fixture("sha256_abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert_eq!(hex(&fixture("sha256_empty")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    // The SPKI of an Ed25519 key starts with the fixed RFC 8410 header, then the key.
    let id = fixture("identity_rfc8032_test1_key");
    assert_eq!(hex(&id[..12]), "302a300506032b6570032100");
    assert_eq!(hex(&id[12..44]), "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
    assert_eq!(id.len(), 44 + 16 + 32);
}

#[test]
fn one_flipped_byte_of_any_fixture_fails_the_decode_or_the_encode_check() {
    let mut flipped = 0usize;
    for s in samples() {
        let bytes = fixture(&s.name);
        assert!(check_decodes(&s, &bytes).is_ok() && check_encodes(&s, &bytes).is_ok(), "{} is not green", s.name);
        for i in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[i] ^= 0x01;
            assert!(
                check_decodes(&s, &bad).is_err() || check_encodes(&s, &bad).is_err(),
                "{}: flipping byte {i} went unnoticed",
                s.name
            );
            flipped += 1;
        }
    }
    assert!(flipped > 300, "only {flipped} flips tried");
}
