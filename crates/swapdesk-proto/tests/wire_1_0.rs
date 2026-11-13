//! Wire compatibility with 1.0 (flow F-NET-02, PROTOCOL.md "Wire fixtures of 1.0"): the bytes that the code of
//! tag `v1.0.0` produced for every wire item, kept in `tests/fixtures/v1.0.0/`.
//!
//! * (a) every fixture decodes with this tree into exactly its sample;
//! * (b) encoding the sample with this tree gives the fixture byte for byte, which is what this tree sends to a
//!   peer of minor 0 (the one exception is `proto_minor` of `Hello` and `HelloAck`, the sender's own minor);
//! * (c) the fixture set is complete: no message type, enum value, stream kind, datagram kind, input record or
//!   permission bit of minor 0 lacks a fixture;
//! * (d) the comparison bites: one flipped byte of any fixture fails (a) or (b).

mod wire_samples;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use swapdesk_proto::*;
use wire_samples::{
    EnumCoverage, Item, Sample, enum_coverage, file_name, from_hex_text, index_text, sampled_permission_bits, samples,
};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1.0.0")
}

fn fixture(name: &str) -> Vec<u8> {
    let path = fixtures_dir().join(file_name(name));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    from_hex_text(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// (a) The fixture decodes, with this tree, into exactly the sample.
fn check_decodes(sample: &Sample, bytes: &[u8]) -> Result<(), String> {
    let got = sample.item.decode_like(bytes).map_err(|e| format!("{}: does not decode: {e}", sample.name))?;
    if got == sample.item {
        Ok(())
    } else {
        Err(format!("{}: decodes into {got:?}, not {:?}", sample.name, sample.item))
    }
}

/// (b) This tree encodes the sample into the fixture. A `Hello` or `HelloAck` is encoded with this tree's own
/// `PROTO_MINOR`, as it sends it, and every other byte must match: the two minor bytes are the only ones that
/// may differ, and they must hold `PROTO_MINOR`.
fn check_encodes(sample: &Sample, bytes: &[u8]) -> Result<(), String> {
    let sent = sample
        .item
        .with_own_minor(PROTO_MINOR)
        .encode()
        .map_err(|e| format!("{}: does not encode: {e}", sample.name))?;
    let mut want = bytes.to_vec();
    let mut got = sent.clone();
    if let Some(range) = sample.item.own_minor_range(bytes) {
        if sent.len() != bytes.len() {
            return Err(format!("{}: sends {} bytes, the fixture has {}", sample.name, sent.len(), bytes.len()));
        }
        if sent[range.clone()] != PROTO_MINOR.to_le_bytes() {
            return Err(format!("{}: proto_minor on the wire is {:02x?}", sample.name, &sent[range]));
        }
        want[range.clone()].fill(0);
        got[range].fill(0);
    }
    if got == want {
        Ok(())
    } else {
        Err(format!("{}: sends {} but the fixture is {}", sample.name, hex(&sent), hex(bytes)))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
fn a_hello_carries_its_own_minor_and_the_fixture_holds_the_minor_of_1_0() {
    for s in samples() {
        let bytes = fixture(&s.name);
        let Some(range) = s.item.own_minor_range(&bytes) else { continue };
        assert_eq!(bytes[range], 0u16.to_le_bytes(), "{}: 1.0 sent minor 0", s.name);
    }
    let hello =
        samples().into_iter().find(|s| matches!(s.item, Item::Frame(Message::Hello(_)))).expect("a Hello sample");
    for minor in [0u16, 1, 7] {
        let bytes = hello.item.with_own_minor(minor).encode().expect("encodes");
        let range = hello.item.own_minor_range(&bytes).expect("a Hello has a minor");
        assert_eq!(bytes[range.clone()], minor.to_le_bytes());
        // Everything but the minor is what 1.0 sent.
        let mut a = bytes.clone();
        let mut b = fixture(&hello.name);
        a[range.clone()].fill(0);
        b[range].fill(0);
        assert_eq!(a, b, "a Hello of minor {minor} differs from the fixture outside proto_minor");
    }
}

// ---------- (c) the fixture set is complete ----------

/// Message types introduced after minor 0, with the minor that introduced each. A type added in a later minor
/// is listed here, with its minor, in the same change that adds it; until then this test fails for it. A type
/// listed here must still exist, and the crate must name the same minor for it (`Message::type_min_minor`).
const TYPES_AFTER_MINOR_0: &[(u16, u16)] = &[
    (0x000B, 1),
    (0x000C, 1),
    (0x0801, 1),
    (0x040C, 2),
    (0x040D, 2),
    (0x040E, 2),
    (0x040F, 2),
    (0x0410, 2),
    (0x0411, 2),
];

/// Enum values introduced after minor 0: `(enum name, tag, minor)`. The same rule as for the types.
const ENUM_VALUES_AFTER_MINOR_0: &[(&str, u64, u16)] = &[
    ("ErrorCode", 22, 1),
    ("ErrorCode", 23, 2),
    ("ErrorCode", 24, 2),
    ("ErrorCode", 25, 2),
    ("ErrorCode", 26, 2),
    ("Desktop", 4, 1),
    ("Codec", 2, 2),
];

#[test]
fn every_message_type_of_minor_0_has_a_fixture() {
    let covered: BTreeSet<u16> = samples().iter().filter_map(|s| s.item.type_id()).collect();
    let newer: BTreeSet<u16> = TYPES_AFTER_MINOR_0.iter().map(|(id, _)| *id).collect();
    for (id, minor) in TYPES_AFTER_MINOR_0 {
        assert!(*minor >= 1, "type {id:#06x} is listed as newer than minor 0 with minor {minor}");
        assert!(Message::ALL_TYPE_IDS.contains(id), "type {id:#06x} is listed but does not exist");
        assert_eq!(Message::type_min_minor(*id), *minor, "type {id:#06x}: the crate names another minor");
    }
    // And the other way round: a type the crate marks as newer is listed here, a 1.0 type is not.
    for id in Message::ALL_TYPE_IDS {
        assert_eq!(Message::type_min_minor(*id) > 0, newer.contains(id), "type {id:#06x}");
    }
    let missing: Vec<String> = Message::ALL_TYPE_IDS
        .iter()
        .filter(|id| !newer.contains(id) && !covered.contains(id))
        .map(|id| format!("{id:#06x}"))
        .collect();
    assert!(
        missing.is_empty(),
        "message type(s) {missing:?} have no 1.0 fixture: if they are older, add samples (the fixtures come from \
         the code of v1.0.0); if they were introduced after minor 0, list them in TYPES_AFTER_MINOR_0"
    );
    for id in &covered {
        assert!(Message::ALL_TYPE_IDS.contains(id), "a fixture has type {id:#06x}, which the table no longer has");
    }
}

#[test]
fn every_enum_value_of_minor_0_is_carried_by_a_fixture() {
    for EnumCoverage { name, sampled, defined } in enum_coverage() {
        let newer: BTreeSet<u64> =
            ENUM_VALUES_AFTER_MINOR_0.iter().filter(|(n, _, _)| *n == name).map(|(_, tag, _)| *tag).collect();
        for tag in &newer {
            assert!(defined.contains(tag), "{name} {tag} is listed as newer but does not exist");
        }
        // The crate names the same minor for each listed value, and 0 for every other.
        let crate_minor = |tag: u64| match name {
            "ErrorCode" => ErrorCode::from_tag(tag as u16).map(ErrorCode::min_minor),
            "Desktop" => Desktop::from_tag(tag as u8).map(Desktop::min_minor),
            "Codec" => Codec::from_tag(tag as u8).map(Codec::min_minor),
            _ => Some(0),
        };
        for tag in &defined {
            let listed = ENUM_VALUES_AFTER_MINOR_0
                .iter()
                .find(|(n, t, _)| *n == name && t == tag)
                .map_or(0, |(_, _, minor)| *minor);
            assert_eq!(crate_minor(*tag), Some(listed), "{name} {tag}: the crate names another minor");
        }
        let missing: Vec<u64> =
            defined.iter().copied().filter(|t| !newer.contains(t) && !sampled.contains(t)).collect();
        assert!(
            missing.is_empty(),
            "{name} value(s) {missing:?} have no 1.0 fixture: add them to the sample table (the fixtures come \
             from the code of v1.0.0) or list them in ENUM_VALUES_AFTER_MINOR_0"
        );
        for tag in &sampled {
            assert!(defined.contains(tag), "a sample uses {name} {tag}, which the crate no longer defines");
        }
    }
}

#[test]
fn every_stream_kind_datagram_kind_input_record_and_permission_bit_has_a_fixture() {
    let all = samples();
    let streams: BTreeSet<u8> = all.iter().filter_map(|s| s.item.stream_kind()).map(StreamKind::tag).collect();
    let wanted: BTreeSet<u8> = StreamKind::ALL.iter().map(|k| k.tag()).collect();
    // The control stream has no kind; every other kind starts a stream with a fixture.
    assert_eq!(streams, wanted, "stream kinds without a fixture, or fixtures of unknown kinds");
    let datagrams: BTreeSet<u8> = all.iter().filter_map(|s| s.item.datagram_kind()).map(DatagramKind::tag).collect();
    let wanted: BTreeSet<u8> = DatagramKind::ALL.iter().map(|k| k.tag()).collect();
    assert_eq!(datagrams, wanted, "datagram kinds without a fixture");

    // Input records have no registry in the crate: their tags are 1 to 6. A seventh record kind would be
    // accepted by this check only after it is given a fixture (the decoder must refuse tag 7 today).
    let record_tags: BTreeSet<u8> = all
        .iter()
        .flat_map(|s| {
            s.item.input_records().iter().map(|r| match r {
                InputEvent::Key { .. } => 0x01,
                InputEvent::Text(_) => 0x02,
                InputEvent::Pointer { .. } => 0x03,
                InputEvent::Button { .. } => 0x04,
                InputEvent::Wheel { .. } => 0x05,
                InputEvent::ReleaseAll => 0x06,
            })
        })
        .collect();
    assert_eq!(record_tags, (1..=6).collect::<BTreeSet<u8>>(), "input record kinds without a fixture");
    assert!(
        matches!(InputEvent::decode(&[0x07]), Err(CodecError::UnknownTag { .. })),
        "a new input record tag exists: give it a fixture and extend this test"
    );

    assert_eq!(sampled_permission_bits(), 0x00FF, "every permission bit 0 to 7 appears in a fixture");
    assert!(PermissionSet::from_bits(0x0100).is_none(), "a new permission bit exists: give it a fixture");
}

// ---------- (d) the comparison bites ----------

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
    assert!(flipped > 1000, "only {flipped} flips tried");
}

#[test]
fn a_fixture_with_a_byte_missing_or_added_fails() {
    for s in samples() {
        let bytes = fixture(&s.name);
        if bytes.is_empty() {
            continue;
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(check_decodes(&s, &longer).is_err() || check_encodes(&s, &longer).is_err(), "{} longer", s.name);
        let shorter = &bytes[..bytes.len() - 1];
        assert!(check_decodes(&s, shorter).is_err() || check_encodes(&s, shorter).is_err(), "{} shorter", s.name);
    }
}
