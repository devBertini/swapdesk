//! Names that mean the same file (F-FILE-03): a pure comparison under Unicode normalization and letter case, and the
//! choice of a new name, `name (2).ext`, when the offered one collides with an existing one.
//!
//! Why: macOS writes names decomposed (NFD: `e` + U+0301) while Windows and most senders write them composed (NFC:
//! U+00E9). NTFS keeps both as two files that look the same; APFS and NTFS compare letter case loosely by default.
//! A receiver that only asked the file system "does this exact name exist?" would show two identical names on one
//! system and overwrite on the other. The receiver never replaces a file, and asks the disk about a name in each
//! form it can be written in ([`forms`]: as given, composed, decomposed); letter case is the disk's own rule (NTFS
//! and APFS find a name in any case). It does not read the folder to compare [`name_key`]s: that cost one listing
//! per finished file, minutes for a folder of thousands, and on macOS a folder read needs the "Files and Folders"
//! permission that writing a file there does not (review of 22/11/2026).
//!
//! Limit (no normalization crate is a dependency, a new one needs the owner's yes): [`compose`] is canonical
//! composition for the Latin letters with the marks in `MARKS` (grave, acute, circumflex, tilde, macron, breve, dot
//! above, diaeresis, ring, double acute, caron, cedilla, ogonek) and for Hangul jamo. A name using another
//! decomposed sequence (Japanese dakuten, Vietnamese stacked marks, Greek, Cyrillic) is compared as written; on
//! APFS the file system itself still treats the two forms as equal, so only a Windows receiver can miss one of them.

use std::collections::HashSet;

/// Combining mark, then pairs of (base letter, composed letter).
const MARKS: &[(char, &str)] = &[
    ('\u{300}', "AÀEÈIÌOÒUÙaàeèiìoòuù"),
    ('\u{301}', "AÁEÉIÍOÓUÚYÝaáeéiíoóuúyýCĆcćLĹlĺNŃnńRŔrŕSŚsśZŹzź"),
    ('\u{302}', "AÂEÊIÎOÔUÛaâeêiîoôuûCĈcĉGĜgĝHĤhĥJĴjĵSŜsŝWŴwŵYŶyŷ"),
    ('\u{303}', "AÃNÑOÕaãnñoõIĨiĩUŨuũEẼeẽYỸyỹ"),
    ('\u{304}', "AĀaāEĒeēIĪiīOŌoōUŪuū"),
    ('\u{306}', "AĂaăEĔeĕGĞgğIĬiĭOŎoŏUŬuŭ"),
    ('\u{307}', "CĊcċEĖeėGĠgġZŻzżIİ"),
    ('\u{308}', "AÄEËIÏOÖUÜaäeëiïoöuüyÿYŸ"),
    ('\u{30A}', "AÅaåUŮuů"),
    ('\u{30B}', "OŐoőUŰuű"),
    ('\u{30C}', "CČcčDĎdďEĚeěNŇnňRŘrřSŠsšTŤtťZŽzž"),
    ('\u{327}', "CÇcçGĢgģKĶkķLĻlļNŅnņRŖrŗSŞsşTŢtţ"),
    ('\u{328}', "AĄaąEĘeęIĮiįUŲuų"),
];

const HANGUL_S_BASE: u32 = 0xAC00;
const HANGUL_L_BASE: u32 = 0x1100;
const HANGUL_V_BASE: u32 = 0x1161;
const HANGUL_T_BASE: u32 = 0x11A7;
const HANGUL_L_COUNT: u32 = 19;
const HANGUL_V_COUNT: u32 = 21;
const HANGUL_T_COUNT: u32 = 28;
const HANGUL_S_COUNT: u32 = HANGUL_L_COUNT * HANGUL_V_COUNT * HANGUL_T_COUNT;

fn compose_pair(base: char, next: char) -> Option<char> {
    let (b, n) = (base as u32, next as u32);
    // Hangul: leading + vowel, then + trailing.
    if (HANGUL_L_BASE..HANGUL_L_BASE + HANGUL_L_COUNT).contains(&b)
        && (HANGUL_V_BASE..HANGUL_V_BASE + HANGUL_V_COUNT).contains(&n)
    {
        let s = HANGUL_S_BASE + ((b - HANGUL_L_BASE) * HANGUL_V_COUNT + (n - HANGUL_V_BASE)) * HANGUL_T_COUNT;
        return char::from_u32(s);
    }
    if (HANGUL_S_BASE..HANGUL_S_BASE + HANGUL_S_COUNT).contains(&b)
        && (b - HANGUL_S_BASE).is_multiple_of(HANGUL_T_COUNT)
        && (HANGUL_T_BASE + 1..HANGUL_T_BASE + HANGUL_T_COUNT).contains(&n)
    {
        return char::from_u32(b + (n - HANGUL_T_BASE));
    }
    let (_, pairs) = MARKS.iter().find(|(m, _)| *m == next)?;
    let chars: Vec<char> = pairs.chars().collect();
    chars.chunks(2).find(|p| p[0] == base).map(|p| p[1])
}

/// The name with each letter followed by one of the marks above, and each Hangul jamo run, composed (NFC for those).
pub fn compose(name: &str) -> String {
    let mut out: Vec<char> = Vec::with_capacity(name.len());
    for c in name.chars() {
        if let Some(last) = out.last().copied()
            && let Some(m) = compose_pair(last, c)
        {
            if let Some(slot) = out.last_mut() {
                *slot = m;
            }
            continue;
        }
        out.push(c);
    }
    out.into_iter().collect()
}

/// The base letter and the combining mark of a composed letter of `MARKS`.
fn decompose_letter(c: char) -> Option<(char, char)> {
    MARKS.iter().find_map(|(mark, pairs)| {
        let chars: Vec<char> = pairs.chars().collect();
        chars.chunks(2).find(|p| p[1] == c).map(|p| (p[0], *mark))
    })
}

/// The name with each letter [`compose`] would make written as its base letter and mark, and each Hangul syllable
/// as its jamo (NFD for those): the form macOS wrote names in on HFS+, and still gives to names that came from it.
pub fn decompose(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    for c in name.chars() {
        let s = c as u32;
        if (HANGUL_S_BASE..HANGUL_S_BASE + HANGUL_S_COUNT).contains(&s) {
            let i = s - HANGUL_S_BASE;
            let (l, v, t) =
                (i / (HANGUL_V_COUNT * HANGUL_T_COUNT), (i / HANGUL_T_COUNT) % HANGUL_V_COUNT, i % HANGUL_T_COUNT);
            out.extend(char::from_u32(HANGUL_L_BASE + l));
            out.extend(char::from_u32(HANGUL_V_BASE + v));
            if t != 0 {
                out.extend(char::from_u32(HANGUL_T_BASE + t));
            }
        } else if let Some((base, mark)) = decompose_letter(c) {
            out.push(base);
            out.push(mark);
        } else {
            out.push(c);
        }
    }
    out
}

/// The ways `name` can be written and still be the same file: as given, composed and decomposed, each once. A name
/// with nothing to compose or decompose (most names) has one form, so asking a disk about every form costs one
/// question; any other name costs two or three, whatever the folder holds.
pub fn forms(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    for other in [compose(name), decompose(name)] {
        if !out.contains(&other) {
            out.push(other);
        }
    }
    out
}

/// Whether a disk holds `name` in any of its [`forms`]. `there` asks the disk about one exact name; its first error
/// ends the question and is returned.
pub fn held_in_any_form<E>(name: &str, there: impl Fn(&str) -> Result<bool, E>) -> Result<bool, E> {
    for form in forms(name) {
        if there(&form)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What two names must have in common to be the same file: composed, then lower-cased.
pub fn name_key(name: &str) -> String {
    compose(name).to_lowercase()
}

/// True when `a` and `b` are the same name for [`name_key`].
pub fn same_name(a: &str, b: &str) -> bool {
    name_key(a) == name_key(b)
}

/// Splits `name.ext` at its last dot (a leading dot is part of the stem).
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// The last n of `stem (n).ext` that [`unique_name`] tries. A folder that already holds every number up to it was
/// not made by a person, and the bound keeps a disk that calls every name taken from holding the receiver for ever.
pub const MAX_COPY_NUMBER: u32 = 9999;

/// `candidate` when no `existing` name is the same file; otherwise `stem (n).ext` for the first n from 2 to
/// [`MAX_COPY_NUMBER`] that is not, and `None` when all of them are. `also_taken` answers for names the listing may
/// not hold (a folder read cut short): true means taken. Its first error ends the search and is returned: a disk
/// that cannot say whether a name is free cannot say it for the next one either.
pub fn unique_name<'a, E>(
    candidate: &str,
    existing: impl IntoIterator<Item = &'a str>,
    also_taken: impl Fn(&str) -> Result<bool, E>,
) -> Result<Option<String>, E> {
    let keys: HashSet<String> = existing.into_iter().map(name_key).collect();
    let free = |n: &str| -> Result<bool, E> { Ok(!keys.contains(&name_key(n)) && !also_taken(n)?) };
    if free(candidate)? {
        return Ok(Some(candidate.to_string()));
    }
    let (stem, ext) = split_ext(candidate);
    for n in 2..=MAX_COPY_NUMBER {
        let c = format!("{stem} ({n}){ext}");
        if free(&c)? {
            return Ok(Some(c));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NFC: &str = "relat\u{f3}rio \u{e7}\u{e3}o.pdf";
    const NFD: &str = "relato\u{301}rio c\u{327}a\u{303}o.pdf";

    #[test]
    fn nfd_composes_to_nfc() {
        assert_eq!(compose(NFD), NFC);
        assert_eq!(compose(NFC), NFC);
        assert!(same_name(NFC, NFD));
    }

    #[test]
    fn different_letters_stay_different() {
        assert!(!same_name("resume.txt", "r\u{e9}sum\u{e9}.txt"));
        assert!(!same_name("a.txt", "b.txt"));
    }

    #[test]
    fn case_is_folded_with_accents() {
        assert!(same_name("Relat\u{d3}rio \u{c7}\u{c3}o.PDF", NFC));
        assert!(same_name("README.md", "readme.MD"));
    }

    #[test]
    fn hangul_jamo_compose() {
        // U+D55C from U+1112 U+1161 U+11AB (macOS writes Korean names decomposed).
        assert_eq!(compose("\u{1112}\u{1161}\u{11ab}"), "\u{d55c}");
        assert!(same_name("\u{d55c}\u{ae00}.txt", "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}.txt"));
    }

    /// Review of 22/11/2026: the forms a disk is asked about instead of reading the folder.
    #[test]
    fn a_name_has_one_form_unless_it_can_be_composed_or_decomposed() {
        assert_eq!(decompose(NFC), NFD);
        assert_eq!(decompose(NFD), NFD);
        assert_eq!(decompose("\u{d55c}\u{ae00}"), "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}");
        assert_eq!(forms("report (2).txt"), ["report (2).txt"], "nothing to compose: one question");
        assert_eq!(forms(NFC), [NFC, NFD]);
        assert_eq!(forms(NFD), [NFD, NFC]);
        // Half composed: as given, all composed, all decomposed.
        let mixed = "relat\u{f3}rio c\u{327}a\u{303}o.pdf";
        assert_eq!(forms(mixed), [mixed, NFC, NFD]);
    }

    #[test]
    fn a_disk_is_asked_about_each_form_until_one_is_there_and_stops_at_an_error() {
        let asked = std::cell::RefCell::new(Vec::new());
        let disk = |holds: &'static str| {
            let asked = &asked;
            move |n: &str| {
                asked.borrow_mut().push(n.to_string());
                Ok::<_, ()>(n == holds)
            }
        };
        assert_eq!(held_in_any_form(NFC, disk(NFD)), Ok(true), "the Mac's form of the name is there");
        assert_eq!(*asked.borrow(), [NFC, NFD]);
        asked.borrow_mut().clear();
        assert_eq!(held_in_any_form(NFD, disk(NFD)), Ok(true));
        assert_eq!(*asked.borrow(), [NFD], "found as given: nothing else is asked");
        assert_eq!(held_in_any_form("a.txt", disk("b.txt")), Ok(false));
        assert_eq!(held_in_any_form(NFC, |_| Err::<bool, _>("gone")), Err("gone"));
    }

    #[test]
    fn stray_marks_are_kept() {
        assert_eq!(compose("\u{301}x"), "\u{301}x");
        assert_eq!(compose("x\u{301}"), "x\u{301}");
    }

    /// `unique_name` over a disk that answers every question.
    fn pick<'a>(candidate: &str, existing: impl IntoIterator<Item = &'a str>, taken: impl Fn(&str) -> bool) -> String {
        unique_name(candidate, existing, |n| Ok::<_, ()>(taken(n))).unwrap().unwrap()
    }

    #[test]
    fn free_name_is_kept() {
        assert_eq!(pick("a.txt", ["b.txt"], |_| false), "a.txt");
    }

    #[test]
    fn exact_collision_gets_two() {
        assert_eq!(pick("a.txt", ["a.txt"], |_| false), "a (2).txt");
        assert_eq!(pick("noext", ["noext"], |_| false), "noext (2)");
        assert_eq!(pick(".bashrc", [".bashrc"], |_| false), ".bashrc (2)");
        assert_eq!(pick("a.tar.gz", ["a.tar.gz"], |_| false), "a.tar (2).gz");
    }

    #[test]
    fn normalization_and_case_collide() {
        assert_eq!(pick(NFD, [NFC], |_| false), "relato\u{301}rio c\u{327}a\u{303}o (2).pdf");
        assert_eq!(pick(NFC, [NFD], |_| false), "relat\u{f3}rio \u{e7}\u{e3}o (2).pdf");
        assert_eq!(pick("Foto.JPG", ["foto.jpg"], |_| false), "Foto (2).JPG");
    }

    #[test]
    fn skips_taken_numbers_in_any_form() {
        let existing = ["a.txt", "A (2).TXT", "a (3).txt"];
        assert_eq!(pick("a.txt", existing, |_| false), "a (4).txt");
        assert_eq!(pick("a.txt", ["a.txt"], |n| n == "a (2).txt"), "a (3).txt");
    }

    /// Review of 22/11/2026, F12: the numbers had no end and a failed answer counted as "taken", so a disk that
    /// could not answer kept the search going through every `u32`. Now the first error ends it and the numbers stop
    /// at [`MAX_COPY_NUMBER`].
    #[test]
    fn the_search_stops_at_the_first_error_and_after_the_last_number() {
        let asked = std::cell::Cell::new(0u32);
        let failing = unique_name("a.txt", ["a.txt"], |_| {
            asked.set(asked.get() + 1);
            Err::<bool, _>("gone")
        });
        assert_eq!(failing, Err("gone"));
        assert_eq!(asked.get(), 1, "one failed answer ends the search");

        asked.set(0);
        let full = unique_name("a.txt", [], |_| {
            asked.set(asked.get() + 1);
            Ok::<_, ()>(true)
        });
        assert_eq!(full, Ok(None), "every number taken: no name, rather than a name that is taken");
        assert_eq!(asked.get(), MAX_COPY_NUMBER, "the name itself and 2 to the last number");
    }
}
