//! Wire compatibility with 1.0 of the items that live in `swapdesk-lan` (flow F-NET-02): the DNS-SD TXT record,
//! the Wake-on-LAN magic packet and the broadcast address, as produced by the code of tag `v1.0.0` and kept in
//! `tests/fixtures/v1.0.0/`. The messages of the session protocol are in `swapdesk-proto/tests/wire_1_0.rs`.
//!
//! (a) every fixture decodes into its sample; (b) encoding the sample gives the fixture byte for byte; (c) the
//! set is complete; (d) a flipped byte fails (a) or (b).

mod wire_samples;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use swapdesk_lan::txt::{KEY_FP, KEY_NAME, KEY_OS, KEY_PAIR};
use swapdesk_proto::Os;
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

/// The TXT keys of minor 0. A key introduced later is added to the record only by a build that knows it; 1.0 ignores
/// unknown keys, so a newer key never breaks a 1.0 reader, and this list stays the keys 1.0 wrote.
const TXT_KEYS_OF_MINOR_0: &[&str] = &[KEY_NAME, KEY_OS, KEY_FP, KEY_PAIR];

#[test]
fn the_fixtures_cover_every_txt_key_os_and_pairing_state_and_the_wake_packets() {
    let all = samples();
    let mut keys = BTreeSet::new();
    let (mut oses, mut pairing) = (BTreeSet::new(), BTreeSet::new());
    for s in &all {
        if let Item::Txt(t) = &s.item {
            for (k, _) in t.encode().expect("encodes") {
                keys.insert(k);
            }
            oses.insert(t.os.tag());
            pairing.insert(t.pairing);
        }
    }
    assert_eq!(keys, TXT_KEYS_OF_MINOR_0.iter().map(|k| k.to_string()).collect::<BTreeSet<_>>());
    assert_eq!(oses, Os::ALL.iter().map(|o| o.tag()).collect::<BTreeSet<_>>(), "an Os without a TXT fixture");
    assert_eq!(pairing, BTreeSet::from([false, true]));
    assert!(all.iter().any(|s| matches!(s.item, Item::MagicPacket(_))));
    assert!(all.iter().any(|s| matches!(s.item, Item::DirectedBroadcast { .. })));
}

#[test]
fn a_txt_record_with_an_unknown_key_still_reads_as_1_0_reads_it() {
    // 1.0 ignores keys it does not know (a later minor may add some): the fixture of a record plus one more key
    // decodes into the same device.
    let s = samples().into_iter().find(|s| s.name == "txt_windows_pairing").expect("sample");
    let mut bytes = fixture(&s.name);
    let extra = b"zz=1";
    bytes.push(extra.len() as u8);
    bytes.extend_from_slice(extra);
    check_decodes(&s, &bytes).expect("an unknown key is ignored");
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
    assert!(flipped > 500, "only {flipped} flips tried");
}
