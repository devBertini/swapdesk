//! The helper server on the 1.1 paths (FILES-UI.md 8.1): volume ids, links, protected folders, the bin and the UI's
//! refusal. A fake `SystemDisk` publishes folders of a temporary directory as volumes.

use std::sync::Mutex;

use swapdesk_proto::{PlaceKind, VolumeKind};

use super::*;
use crate::system::SysVolume;
use crate::volumes::Volumes;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-vol-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn volume(identity: &str, root: &Path, has_bin: bool) -> SysVolume {
    SysVolume {
        identity: identity.to_string(),
        root: root.to_path_buf(),
        kind: VolumeKind::Fixed,
        label: format!("label of {identity}"),
        letter: None,
        total_bytes: 1000,
        free_bytes: 500,
        has_bin,
    }
}

/// Publishes what the test puts in `vols`; its bin is the folder `bin`.
struct Fake {
    vols: Mutex<Vec<SysVolume>>,
    places: Vec<(PlaceKind, PathBuf)>,
    protected: Vec<PathBuf>,
    /// What nothing may create or replace (`SystemDisk::protected_entries`).
    entries: Vec<PathBuf>,
    bin: PathBuf,
    trashed: Mutex<Vec<PathBuf>>,
}

impl Fake {
    fn new(vols: Vec<SysVolume>, bin: &Path) -> Arc<Self> {
        std::fs::create_dir_all(bin).unwrap();
        Arc::new(Self {
            vols: Mutex::new(vols),
            places: Vec::new(),
            protected: Vec::new(),
            entries: Vec::new(),
            bin: bin.to_path_buf(),
            trashed: Mutex::default(),
        })
    }

    /// Like `with`, with protected entries too.
    fn guarded(vols: Vec<SysVolume>, bin: &Path, protected: Vec<PathBuf>, entries: Vec<PathBuf>) -> Arc<Self> {
        std::fs::create_dir_all(bin).unwrap();
        Arc::new(Self {
            vols: Mutex::new(vols),
            places: Vec::new(),
            protected,
            entries,
            bin: bin.to_path_buf(),
            trashed: Mutex::default(),
        })
    }

    fn with(vols: Vec<SysVolume>, bin: &Path, places: Vec<(PlaceKind, PathBuf)>, protected: Vec<PathBuf>) -> Arc<Self> {
        std::fs::create_dir_all(bin).unwrap();
        Arc::new(Self {
            vols: Mutex::new(vols),
            places,
            protected,
            entries: Vec::new(),
            bin: bin.to_path_buf(),
            trashed: Mutex::default(),
        })
    }

    fn set(&self, vols: Vec<SysVolume>) {
        *lock(&self.vols) = vols;
    }
}

impl SystemDisk for Fake {
    fn volumes(&self) -> Vec<SysVolume> {
        lock(&self.vols).clone()
    }

    fn places(&self) -> Vec<(PlaceKind, PathBuf)> {
        self.places.clone()
    }

    fn trash(&self, path: &Path) -> Result<(), ErrorCode> {
        lock(&self.trashed).push(path.to_path_buf());
        let name = path.file_name().unwrap();
        std::fs::rename(path, self.bin.join(name)).map_err(|_| ErrorCode::IoFailed)
    }

    fn protected(&self) -> Vec<PathBuf> {
        self.protected.clone()
    }

    fn protected_entries(&self) -> Vec<PathBuf> {
        self.entries.clone()
    }
}

fn t(s: &str) -> Text {
    Text(s.to_string())
}

fn server(home: &Path, system: Arc<Fake>) -> FsServer {
    FsServer::new(Roots::Home { home: home.to_path_buf(), receive: Some("Downloads/Swapdesk".to_string()) })
        .with_system(system)
}

fn call(s: &FsServer, c: FsCall) -> Result<FsAnswer, ErrorCode> {
    s.handle(c).expect("a call with an answer")
}

fn list(s: &FsServer, path: &str) -> Result<Vec<String>, ErrorCode> {
    match call(s, FsCall::ListPage { path: t(path), start: 0, max: 100 })? {
        FsAnswer::Entries2(p) => Ok(p.entries.into_iter().map(|e| e.name.0).collect()),
        other => panic!("not a page: {other:?}"),
    }
}

fn make(s: &FsServer, dir: &str, name: &str) -> Result<FsAnswer, ErrorCode> {
    call(s, FsCall::MakeDir { dir: t(dir), name: t(name) })
}

/// A folder tree: `<root>/home`, `<root>/v1` and `<root>/v2`, with a file in each volume.
struct Rig {
    root: PathBuf,
    home: PathBuf,
    v1: PathBuf,
    v2: PathBuf,
}

fn rig(tag: &str) -> Rig {
    let root = temp(tag);
    let (home, v1, v2) = (root.join("home"), root.join("v1"), root.join("v2"));
    for d in [&home, &v1, &v2] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(home.join("mine.txt"), b"m").unwrap();
    std::fs::write(v1.join("one.txt"), b"1").unwrap();
    std::fs::write(v2.join("two.txt"), b"2").unwrap();
    Rig { root, home, v1, v2 }
}

#[test]
fn a_volume_path_resolves_only_to_a_published_volume() {
    let r = rig("resolve");
    let sys = Fake::new(vec![volume("A", &r.v1, true), volume("B", &r.v2, true)], &r.root.join("bin"));
    let s = server(&r.home, sys);
    assert_eq!(list(&s, "").unwrap(), ["mine.txt"], "the empty first component is the home folder, as in 1.0");
    assert_eq!(list(&s, ":1").unwrap(), ["one.txt"]);
    assert_eq!(list(&s, ":2").unwrap(), ["two.txt"]);
    std::fs::create_dir(r.v1.join("sub")).unwrap();
    std::fs::write(r.v1.join("sub").join("deep.txt"), b"d").unwrap();
    assert_eq!(list(&s, ":1/sub").unwrap(), ["deep.txt"]);
    assert_eq!(list(&s, ":9"), Err(ErrorCode::VolumeGone), "no volume has that id");
    for bad in [":", ":x", ":01", ":-1", ":1x", ":4294967296", ": 1"] {
        assert_eq!(list(&s, bad), Err(ErrorCode::InvalidName), "{bad}");
    }
    // The same files are reached as a folder of the volume that holds the home folder.
    let only_home = Fake::new(vec![volume("H", &r.root, true)], &r.root.join("bin2"));
    let s = server(&r.home, only_home);
    assert_eq!(list(&s, ":1/home").unwrap(), list(&s, "").unwrap());
    // A server with no system part speaks 1.0: a volume path names nothing.
    let plain = FsServer::new(Roots::Home { home: r.home.clone(), receive: None });
    assert_eq!(list(&plain, ":1"), Err(ErrorCode::VolumeGone));
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn a_volume_id_is_never_reused_for_another_volume() {
    let r = rig("ids");
    let sys = Fake::new(vec![volume("stick-1", &r.v1, true)], &r.root.join("bin"));
    let s = server(&r.home, Arc::clone(&sys));
    assert_eq!(list(&s, ":1").unwrap(), ["one.txt"]);
    // The stick is pulled and another one is put in under the same place.
    sys.set(vec![volume("stick-2", &r.v1, true)]);
    assert_eq!(list(&s, ":1"), Err(ErrorCode::VolumeGone), "the old id does not reach the new disk");
    assert_eq!(list(&s, ":2").unwrap(), ["one.txt"], "the new disk has an id of its own");
    // A third disk, then the first one comes back: it gets its old id again.
    sys.set(vec![volume("stick-3", &r.v2, true)]);
    assert_eq!(list(&s, ":3").unwrap(), ["two.txt"]);
    sys.set(vec![volume("stick-1", &r.v1, true), volume("stick-3", &r.v2, true)]);
    assert_eq!(list(&s, ":1").unwrap(), ["one.txt"], "the same identity gets its old id");
    assert_eq!(list(&s, ":2"), Err(ErrorCode::VolumeGone));
    match call(&s, FsCall::Volumes).unwrap() {
        FsAnswer::Volumes(v) => assert_eq!(v.volumes.iter().map(|x| x.id).collect::<Vec<_>>(), [1, 3]),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn a_path_on_a_removed_volume_answers_volume_gone() {
    let r = rig("gone");
    let sys = Fake::new(vec![volume("A", &r.v1, true)], &r.root.join("bin"));
    let s = server(&r.home, Arc::clone(&sys));
    assert!(list(&s, ":1").is_ok());
    sys.set(Vec::new());
    assert_eq!(list(&s, ":1"), Err(ErrorCode::VolumeGone));
    assert_eq!(list(&s, ":1/sub"), Err(ErrorCode::VolumeGone));
    assert_eq!(make(&s, ":1", "new"), Err(ErrorCode::VolumeGone));
    assert_eq!(
        call(&s, FsCall::Trash { dir: t(":1"), name: t("one.txt") }),
        Err(ErrorCode::VolumeGone),
        "nothing is done to a volume that is not there"
    );
    assert!(r.v1.join("one.txt").exists());
    assert_eq!(call(&s, FsCall::OpenRead { path: t(":1/one.txt") }).err(), Some(ErrorCode::VolumeGone));
    assert_eq!(call(&s, FsCall::PartCreate { dir: t(":1"), name: t("x") }), Err(ErrorCode::VolumeGone));
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn no_component_can_form_an_absolute_unc_device_or_stream_path() {
    let r = rig("forms");
    let sys = Fake::new(vec![volume("A", &r.v1, true)], &r.root.join("bin"));
    let s = server(&r.home, sys);
    let bad = [
        "\\\\server\\share",
        "\\\\?\\C:\\Windows",
        "\\\\.\\COM1",
        "C:\\Windows",
        "C:/Windows",
        "a:b",
        "/etc",
        "/",
        "//server/share",
        "..",
        "../x",
        "x/../y",
        "./x",
        ":1/..",
        ":1/../v2",
        ":1/C:",
        ":1/x:stream",
        ":1/a\\b",
        ":1//x",
        ":1/",
        ":1/./x",
        ":1/con",
        ":1/nul.txt",
        ":1/trailing.",
        ":1/ leading",
        ":1/x.swapdesk-part",
        ":1:2",
        "::1",
        ":1/:2",
    ];
    for path in bad {
        assert_eq!(list(&s, path), Err(ErrorCode::InvalidName), "list {path:?}");
        assert_eq!(make(&s, path, "x"), Err(ErrorCode::InvalidName), "make dir in {path:?}");
        assert!(call(&s, FsCall::OpenRead { path: t(path) }).is_err(), "open {path:?}");
    }
    for name in ["..", "a/b", "a\\b", "C:", "x:y", ".", "", "aux", "\\\\x", "d.swapdesk-meta"] {
        assert_eq!(make(&s, ":1", name), Err(ErrorCode::InvalidName), "name {name:?}");
    }
    assert!(!r.root.join("x").exists() && !r.v1.join("x").exists());
    let _ = std::fs::remove_dir_all(&r.root);
}

/// A link to the folder `target` at `link`: a symbolic link on Unix, a junction on Windows (no privilege needed).
/// `false` when the system would not make one.
fn make_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|o| o.status.success())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

#[test]
fn changes_never_pass_through_a_link_at_any_component() {
    let r = rig("links");
    let sys = Fake::new(vec![volume("A", &r.v1, true)], &r.root.join("bin"));
    let s = server(&r.home, sys);
    let outside = r.root.join("outside");
    std::fs::create_dir_all(outside.join("inner")).unwrap();
    std::fs::write(outside.join("file.txt"), b"f").unwrap();
    std::fs::create_dir(r.home.join("real")).unwrap();
    if !make_link(&outside, &r.home.join("lnk")) || !make_link(&outside, &r.home.join("real").join("deep")) {
        eprintln!("no link could be made here: the test cannot run");
        return;
    }
    if !make_link(&outside, &r.v1.join("vlnk")) {
        return;
    }
    // Reading may follow a link, as in 1.0.
    assert_eq!(list(&s, "lnk").unwrap(), ["inner", "file.txt"]);
    // A link as the first, as a middle and as a volume component: every change is `InvalidName`.
    for dir in ["lnk", "lnk/inner", "real/deep", "real/deep/inner", ":1/vlnk", ":1/vlnk/inner"] {
        assert_eq!(make(&s, dir, "new"), Err(ErrorCode::InvalidName), "make dir in {dir}");
        assert_eq!(
            call(&s, FsCall::Rename { dir: t(dir), name: t("file.txt"), new_name: t("f2.txt") }),
            Err(ErrorCode::InvalidName),
            "rename in {dir}"
        );
        assert_eq!(
            call(&s, FsCall::Trash { dir: t(dir), name: t("file.txt") }),
            Err(ErrorCode::InvalidName),
            "trash in {dir}"
        );
        assert_eq!(
            call(&s, FsCall::Move { from_dir: t(dir), name: t("file.txt"), to_dir: t("") }),
            Err(ErrorCode::InvalidName),
            "move out of {dir}"
        );
        assert_eq!(
            call(&s, FsCall::Move { from_dir: t(""), name: t("mine.txt"), to_dir: t(dir) }),
            Err(ErrorCode::InvalidName),
            "move into {dir}"
        );
        assert_eq!(call(&s, FsCall::PartCreate { dir: t(dir), name: t("p") }), Err(ErrorCode::InvalidName), "{dir}");
    }
    assert!(outside.join("file.txt").exists() && !outside.join("new").exists() && !outside.join("f2.txt").exists());
    assert!(r.home.join("mine.txt").exists());
    // An operation on the link itself acts on the link, never on what it points to.
    call(&s, FsCall::Rename { dir: t(""), name: t("lnk"), new_name: t("renamed-link") }).unwrap();
    assert!(r.home.join("renamed-link").exists() && !r.home.join("lnk").exists());
    assert!(outside.join("inner").exists(), "the target was not touched");
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn changes_in_protected_locations_are_refused() {
    let r = rig("protected");
    std::fs::create_dir_all(r.home.join("sys").join("deep")).unwrap();
    std::fs::create_dir_all(r.home.join("outer").join("guarded")).unwrap();
    std::fs::write(r.home.join("sys").join("f.txt"), b"s").unwrap();
    // A folder to guard and one inside a folder.
    let protected = vec![r.home.join("sys"), r.home.join("outer").join("guarded")];
    let sys = Fake::with(vec![volume("A", &r.v1, true)], &r.root.join("bin"), Vec::new(), protected);
    let s = server(&r.home, Arc::clone(&sys));
    let refused = Err(ErrorCode::NotPermitted);
    assert_eq!(make(&s, "sys", "new"), refused);
    assert_eq!(make(&s, "sys/deep", "new"), refused, "anything inside");
    assert_eq!(make(&s, "", "sys"), refused, "the folder's own name");
    assert_eq!(call(&s, FsCall::Rename { dir: t(""), name: t("sys"), new_name: t("sys2") }), refused);
    assert_eq!(call(&s, FsCall::Rename { dir: t(""), name: t("mine.txt"), new_name: t("sys") }), refused);
    assert_eq!(call(&s, FsCall::Trash { dir: t("sys"), name: t("f.txt") }), refused);
    assert_eq!(call(&s, FsCall::Move { from_dir: t(""), name: t("mine.txt"), to_dir: t("sys") }), refused);
    assert_eq!(call(&s, FsCall::Move { from_dir: t("sys"), name: t("f.txt"), to_dir: t("") }), refused);
    assert_eq!(call(&s, FsCall::Rename { dir: t(""), name: t("outer"), new_name: t("o2") }), refused, "it holds one");
    assert_eq!(call(&s, FsCall::PartCreate { dir: t("sys"), name: t("up") }), refused, "receiving counts as a change");
    assert!(
        r.home.join("sys").join("f.txt").exists() && r.home.join("outer").exists() && !r.home.join("sys2").exists()
    );
    assert!(lock(&sys.trashed).is_empty());
    // Reading stays allowed; so does a change next to a protected folder and below the root of the volume.
    assert_eq!(list(&s, "sys").unwrap(), ["deep", "f.txt"]);
    assert!(list(&s, ":1").is_ok());
    std::fs::create_dir(r.v1.join("free")).unwrap();
    assert!(make(&s, ":1/free", "ok").is_ok());
    assert!(make(&s, "outer", "ok").is_ok());
    assert!(r.home.join("outer").join("ok").is_dir());
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn the_root_of_a_volume_protects_its_own_entries_only() {
    // A filesystem root has no parent: `C:\` or `/`.
    let root = std::env::temp_dir().ancestors().last().unwrap().to_path_buf();
    let sys = Fake::with(Vec::new(), &temp("rootbin"), Vec::new(), vec![root.clone()]);
    let v = Volumes::new(sys);
    assert!(v.protected_hit(&root.join("anything")), "an entry of the root");
    assert!(v.protected_hit(&root), "the root itself");
    assert!(!v.protected_hit(&root.join("a").join("b")), "deeper down is the user's");
    assert!(!v.protected_hit(&std::env::temp_dir().join("x")));
}

#[test]
fn rename_and_move_never_replace() {
    let r = rig("replace");
    let sys = Fake::new(vec![volume("A", &r.v1, true)], &r.root.join("bin"));
    let s = server(&r.home, sys);
    std::fs::write(r.home.join("a.txt"), b"AAA").unwrap();
    std::fs::write(r.home.join("b.txt"), b"BBB").unwrap();
    std::fs::create_dir(r.home.join("dir")).unwrap();
    std::fs::write(r.home.join("dir").join("a.txt"), b"in dir").unwrap();
    assert_eq!(call(&s, FsCall::Rename { dir: t(""), name: t("a.txt"), new_name: t("b.txt") }), Err(ErrorCode::Exists));
    assert_eq!(
        call(&s, FsCall::Rename { dir: t(""), name: t("a.txt"), new_name: t("dir") }),
        Err(ErrorCode::Exists),
        "a folder holds that name"
    );
    assert_eq!(call(&s, FsCall::Move { from_dir: t(""), name: t("a.txt"), to_dir: t("dir") }), Err(ErrorCode::Exists));
    assert_eq!(make(&s, "", "b.txt"), Err(ErrorCode::Exists));
    assert_eq!(make(&s, "", "dir"), Err(ErrorCode::Exists));
    assert_eq!(std::fs::read(r.home.join("a.txt")).unwrap(), b"AAA");
    assert_eq!(std::fs::read(r.home.join("b.txt")).unwrap(), b"BBB");
    assert_eq!(std::fs::read(r.home.join("dir").join("a.txt")).unwrap(), b"in dir");
    assert_eq!(call(&s, FsCall::Rename { dir: t(""), name: t("none"), new_name: t("x") }), Err(ErrorCode::NotFound));
    // What is free goes through, from a volume to the home folder as well.
    call(&s, FsCall::Rename { dir: t(""), name: t("a.txt"), new_name: t("c.txt") }).unwrap();
    call(&s, FsCall::Move { from_dir: t(":1"), name: t("one.txt"), to_dir: t("") }).unwrap();
    assert_eq!(std::fs::read(r.home.join("c.txt")).unwrap(), b"AAA");
    assert_eq!(std::fs::read(r.home.join("one.txt")).unwrap(), b"1");
    assert!(!r.v1.join("one.txt").exists());
    // A change of letter case alone is a rename, not a collision with itself (Windows and the Mac's default).
    #[cfg(windows)]
    {
        call(&s, FsCall::Rename { dir: t(""), name: t("c.txt"), new_name: t("C.txt") }).unwrap();
        assert!(list(&s, "").unwrap().contains(&"C.txt".to_string()));
    }
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn move_refuses_a_folder_into_itself() {
    let r = rig("into-itself");
    let sys = Fake::new(vec![volume("H", &r.root, true)], &r.root.join("bin"));
    let s = server(&r.home, sys);
    std::fs::create_dir_all(r.home.join("d").join("sub").join("subsub")).unwrap();
    for to in ["d", "d/sub", "d/sub/subsub"] {
        assert_eq!(
            call(&s, FsCall::Move { from_dir: t(""), name: t("d"), to_dir: t(to) }),
            Err(ErrorCode::InvalidName),
            "into {to}"
        );
    }
    // The same folder reached under its volume spelling is the same folder.
    assert_eq!(
        call(&s, FsCall::Move { from_dir: t(""), name: t("d"), to_dir: t(":1/home/d/sub") }),
        Err(ErrorCode::InvalidName)
    );
    assert!(r.home.join("d").join("sub").join("subsub").is_dir());
    // Next to itself is fine for another folder.
    std::fs::create_dir(r.home.join("e")).unwrap();
    call(&s, FsCall::Move { from_dir: t(""), name: t("d"), to_dir: t("e") }).unwrap();
    assert!(r.home.join("e").join("d").join("sub").is_dir());
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn trash_on_a_volume_without_a_bin_deletes_nothing() {
    let r = rig("bin");
    let sys = Fake::new(vec![volume("with", &r.v1, true), volume("without", &r.v2, false)], &r.root.join("bin"));
    let s = server(&r.home, Arc::clone(&sys));
    assert_eq!(call(&s, FsCall::Trash { dir: t(":2"), name: t("two.txt") }), Err(ErrorCode::NoBin));
    assert!(r.v2.join("two.txt").exists());
    assert!(lock(&sys.trashed).is_empty(), "the system was not even asked");
    // A move away from it would end with the source in the bin: refused before a byte is copied.
    assert_eq!(
        call(&s, FsCall::Move { from_dir: t(":2"), name: t("two.txt"), to_dir: t(":1") }),
        Err(ErrorCode::NoBin)
    );
    assert!(r.v2.join("two.txt").exists() && !r.v1.join("two.txt").exists());
    // A volume with a bin sends it there, never deleting it.
    call(&s, FsCall::Trash { dir: t(":1"), name: t("one.txt") }).unwrap();
    assert!(!r.v1.join("one.txt").exists() && r.root.join("bin").join("one.txt").exists());
    assert_eq!(call(&s, FsCall::Trash { dir: t(":1"), name: t("one.txt") }), Err(ErrorCode::NotFound));
    // Moves inside the volume without a bin are not refused.
    std::fs::create_dir(r.v2.join("sub")).unwrap();
    call(&s, FsCall::Move { from_dir: t(":2"), name: t("two.txt"), to_dir: t(":2/sub") }).unwrap();
    // No system part at all: no bin, nothing happens.
    let plain = FsServer::new(Roots::Home { home: r.home.clone(), receive: None });
    assert_eq!(call(&plain, FsCall::Trash { dir: t(""), name: t("mine.txt") }), Err(ErrorCode::NotPermitted));
    assert!(r.home.join("mine.txt").exists());
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn the_ui_disk_refuses_every_new_call() {
    let tokens = Arc::new(Tokens::new());
    let dir = temp("ui");
    let token = tokens.receive_into(dir.clone());
    let s = FsServer::new(Roots::Tokens(Arc::clone(&tokens)));
    let r = format!("r{token}");
    for c in [
        FsCall::ListPage { path: t(&r), start: 0, max: 10 },
        FsCall::ListPage { path: t(""), start: 0, max: 10 },
        FsCall::MakeDir { dir: t(&r), name: t("x") },
        FsCall::Rename { dir: t(&r), name: t("a"), new_name: t("b") },
        FsCall::Move { from_dir: t(&r), name: t("a"), to_dir: t(&r) },
        FsCall::Trash { dir: t(&r), name: t("a") },
        FsCall::Volumes,
    ] {
        assert_eq!(call(&s, c.clone()), Err(ErrorCode::InvalidName), "{c:?}");
    }
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "nothing was made in the receive folder");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn volumes_answers_home_places_and_volumes_and_the_poll_sees_a_change() {
    let r = rig("view");
    std::fs::create_dir_all(r.home.join("Documents")).unwrap();
    std::fs::create_dir_all(r.home.join("Downloads").join("Swapdesk")).unwrap();
    let elsewhere = r.v2.join("Pictures");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let nowhere = r.root.join("nowhere");
    let places = vec![
        (PlaceKind::Documents, r.home.join("Documents")),
        (PlaceKind::Pictures, elsewhere),
        (PlaceKind::Music, nowhere),
    ];
    let mut long = volume("B", &r.v2, false);
    long.label = "é".repeat(100);
    long.kind = VolumeKind::Removable;
    long.letter = Some(b'E');
    let sys = Fake::with(vec![volume("A", &r.v1, true), long], &r.root.join("bin"), places, Vec::new());
    let s = server(&r.home, Arc::clone(&sys));
    assert!(s.publishes_volumes());
    assert!(!s.poll_volumes_changed(), "the set at creation is the baseline");
    assert!(!s.poll_volumes_changed());
    let FsAnswer::Volumes(v) = call(&s, FsCall::Volumes).unwrap() else { panic!("volumes") };
    assert_eq!(v.home.0, "");
    let places: Vec<_> = v.places.iter().map(|p| (p.kind, p.path.0.as_str())).collect();
    assert_eq!(
        places,
        [
            (PlaceKind::Documents, "Documents"),
            (PlaceKind::Pictures, ":2/Pictures"),
            (PlaceKind::Received, "Downloads/Swapdesk")
        ],
        "under home as it is, else under its volume; one outside every volume is left out"
    );
    assert_eq!(v.volumes.len(), 2);
    assert_eq!((v.volumes[0].id, v.volumes[0].has_bin, v.volumes[0].total_bytes), (1, true, 1000));
    assert_eq!(
        (v.volumes[1].kind, v.volumes[1].letter, v.volumes[1].has_bin),
        (VolumeKind::Removable, Some(b'E'), false)
    );
    assert!(v.volumes[1].label.0.len() <= 64 && v.volumes[1].label.0.chars().all(|c| c == 'é'));
    // The set changes: the poll says so once.
    sys.set(vec![volume("A", &r.v1, true)]);
    assert!(s.poll_volumes_changed());
    assert!(!s.poll_volumes_changed());
    sys.set(vec![volume("A", &r.v1, true), volume("C", &r.v2, true)]);
    assert!(s.poll_volumes_changed());
    // A server with no system part has no volumes to watch and answers only the home folder.
    let plain = FsServer::new(Roots::Home { home: r.home.clone(), receive: None });
    assert!(!plain.publishes_volumes() && !plain.poll_volumes_changed());
    let FsAnswer::Volumes(v) = call(&plain, FsCall::Volumes).unwrap() else { panic!("volumes") };
    assert!(v.places.is_empty() && v.volumes.is_empty());
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn a_page_carries_flags_kinds_and_where_the_next_one_starts() {
    let r = rig("pages");
    for i in 0..5 {
        std::fs::write(r.home.join(format!("f{i}")), vec![0u8; i]).unwrap();
    }
    std::fs::write(r.home.join(".hidden"), b"h").unwrap();
    std::fs::create_dir(r.home.join("adir")).unwrap();
    let sys = Fake::new(vec![volume("H", &r.root, true)], &r.root.join("bin"));
    let s = server(&r.home, sys);
    let page = |start, max| match call(&s, FsCall::ListPage { path: t(""), start, max }).unwrap() {
        FsAnswer::Entries2(p) => p,
        other => panic!("{other:?}"),
    };
    let first = page(0, 3);
    let names: Vec<_> = first.entries.iter().map(|e| e.name.0.as_str()).collect();
    assert_eq!(names, ["adir", ".hidden", "f0"], "folders first, then by name");
    assert_eq!(first.next, Some(3));
    assert_eq!(first.entries[0].kind, swapdesk_proto::EntryKind::Dir);
    assert_eq!(first.free, Some(500), "the free space of the volume that holds the folder");
    let second = page(3, 3);
    assert_eq!(second.entries.iter().map(|e| e.name.0.as_str()).collect::<Vec<_>>(), ["f1", "f2", "f3"]);
    let last = page(6, 3);
    assert_eq!(last.entries.len(), 2);
    assert_eq!(last.next, None);
    assert!(page(100, 3).entries.is_empty());
    #[cfg(unix)]
    assert_eq!(first.entries[1].flags & swapdesk_proto::ENTRY_HIDDEN, swapdesk_proto::ENTRY_HIDDEN);
    let _ = std::fs::remove_dir_all(&r.root);
}

/// The 8.3 short name Windows gave the folder `long` of `dir` (`PROTEC~1`), read from `dir /x`; `None` when the volume
/// makes none (8.3 generation off), in which case the short-name tests have nothing to prove.
#[cfg(windows)]
fn short_name_of(dir: &Path, long: &str) -> Option<String> {
    let out = std::process::Command::new("cmd").args(["/C", "dir", "/x", "/ad"]).arg(dir).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.lines().find_map(|line| {
        let before = line.trim_end().strip_suffix(long)?.trim_end();
        let short = before.rsplit(' ').next()?;
        short.contains('~').then(|| short.to_string())
    })
}

#[cfg(windows)]
#[test]
fn a_short_name_does_not_get_around_the_protected_folders() {
    let r = rig("short-guard");
    let guarded = r.home.join("Protected Folder Long Name");
    std::fs::create_dir_all(guarded.join("inner")).unwrap();
    std::fs::write(guarded.join("f.txt"), b"s").unwrap();
    let Some(short) = short_name_of(&r.home, "Protected Folder Long Name") else {
        eprintln!("this volume makes no 8.3 names: the short-name guard has nothing to prove here");
        let _ = std::fs::remove_dir_all(&r.root);
        return;
    };
    let sys = Fake::with(vec![volume("A", &r.v1, true)], &r.root.join("bin"), Vec::new(), vec![guarded.clone()]);
    let s = server(&r.home, Arc::clone(&sys));
    let refused = Err(ErrorCode::NotPermitted);
    let inner = format!("{short}/inner");
    assert_eq!(make(&s, &short, "new"), refused, "a change in the folder through its short name");
    assert_eq!(make(&s, &inner, "new"), refused, "and deeper");
    assert_eq!(call(&s, FsCall::Rename { dir: t(""), name: t(&short), new_name: t("moved") }), refused);
    assert_eq!(call(&s, FsCall::Trash { dir: t(""), name: t(&short) }), refused);
    assert_eq!(call(&s, FsCall::Trash { dir: t(&short), name: t("f.txt") }), refused);
    assert_eq!(call(&s, FsCall::Move { from_dir: t(""), name: t("mine.txt"), to_dir: t(&short) }), refused);
    assert_eq!(call(&s, FsCall::Move { from_dir: t(&short), name: t("f.txt"), to_dir: t("") }), refused);
    assert_eq!(call(&s, FsCall::PartCreate { dir: t(&short), name: t("up") }), refused);
    assert!(guarded.join("f.txt").exists() && !guarded.join("new").exists() && !r.home.join("moved").exists());
    assert!(lock(&sys.trashed).is_empty());
    // The guarded folder named the long way stays refused, and what is not guarded is still free.
    assert_eq!(make(&s, "Protected Folder Long Name", "new"), refused);
    assert!(make(&s, "", "free").is_ok());
    let _ = std::fs::remove_dir_all(&r.root);
}

#[cfg(windows)]
#[test]
fn a_short_name_of_a_system_folder_is_the_same_folder_for_the_guard() {
    // `C:\PROGRA~3` is ProgramData (Users may write there). The real protected set names it the long way.
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let short = PathBuf::from(format!("{drive}/PROGRA~3"));
    if !short.is_dir() {
        eprintln!("no 8.3 name for ProgramData on this volume");
        return;
    }
    let program_data = PathBuf::from(std::env::var("ProgramData").expect("ProgramData"));
    let sys = Fake::with(Vec::new(), &temp("short-sys-bin"), Vec::new(), vec![program_data.clone()]);
    let v = Volumes::new(sys);
    assert!(v.protected_hit(&program_data.join("Microsoft")), "the long way: refused");
    assert!(v.protected_hit(&short.join("Microsoft")), "the short way is the same place");
    assert!(v.protected_hit(&short.join("swapdesk-not-there").join("x")), "a place that does not exist yet");
    assert!(!v.protected_hit(&std::env::temp_dir().join("x")));
}

#[cfg(windows)]
#[test]
fn a_link_is_found_through_its_short_name_too() {
    let r = rig("short-link");
    let outside = r.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("file.txt"), b"f").unwrap();
    if !make_link(&outside, &r.home.join("Junction With A Long Name")) {
        eprintln!("no link could be made here: the test cannot run");
        return;
    }
    let Some(short) = short_name_of(&r.home, "Junction With A Long Name") else {
        eprintln!("this volume makes no 8.3 names");
        return;
    };
    let sys = Fake::new(vec![volume("A", &r.v1, true)], &r.root.join("bin"));
    let s = server(&r.home, sys);
    assert_eq!(make(&s, &short, "new"), Err(ErrorCode::InvalidName), "the junction by its short name");
    assert_eq!(make(&s, "Junction With A Long Name", "new"), Err(ErrorCode::InvalidName));
    assert!(!outside.join("new").exists());
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn the_autostart_folders_refuse_every_write_and_stay_readable() {
    // FILES alone could otherwise put a program where the system runs it at the next logon: the Startup folders of
    // Windows (the user's, on the home volume, and the common one, on a volume of its own) and the Launch folders of the
    // Mac are in the protected set of the system part; here a fake table names two real temporary folders.
    let r = rig("autostart");
    let user_startup = r.home.join("Startup");
    let common_startup = r.v1.join("CommonStartup");
    for d in [&user_startup, &common_startup] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("existing.lnk"), b"x").unwrap();
    }
    std::fs::write(r.home.join("payload.bat"), b"calc").unwrap();
    std::fs::write(r.v1.join("payload2.bat"), b"calc").unwrap();
    let sys = Fake::with(
        vec![volume("A", &r.v1, true)],
        &r.root.join("bin"),
        Vec::new(),
        vec![user_startup.clone(), common_startup.clone()],
    );
    let s = server(&r.home, Arc::clone(&sys));
    let refused = Err(ErrorCode::NotPermitted);
    for dir in ["Startup", ":1/CommonStartup"] {
        // An upload (the FileOffer `dest`): the partial file is refused before a byte is written.
        assert_eq!(call(&s, FsCall::PartCreate { dir: t(dir), name: t("payload.bat") }), refused, "upload into {dir}");
        assert_eq!(call(&s, FsCall::PartRename { dir: t(dir), name: t("x"), target_name: t("run.bat") }), refused);
        assert_eq!(make(&s, dir, "folder"), refused, "new folder in {dir}");
        assert_eq!(call(&s, FsCall::MetaWrite { dir: t(dir), name: t("p"), data: Bytes(vec![1]) }), refused);
        assert_eq!(
            call(&s, FsCall::Rename { dir: t(dir), name: t("existing.lnk"), new_name: t("other.lnk") }),
            refused,
            "rename in {dir}"
        );
        assert_eq!(call(&s, FsCall::Trash { dir: t(dir), name: t("existing.lnk") }), refused, "trash in {dir}");
    }
    assert_eq!(call(&s, FsCall::Move { from_dir: t(""), name: t("payload.bat"), to_dir: t("Startup") }), refused);
    assert_eq!(
        call(&s, FsCall::Move { from_dir: t(":1"), name: t("payload2.bat"), to_dir: t(":1/CommonStartup") }),
        refused
    );
    assert_eq!(call(&s, FsCall::Rename { dir: t(""), name: t("payload.bat"), new_name: t("Startup") }), refused);
    assert!(!user_startup.join("payload.bat").exists() && !common_startup.join("payload2.bat").exists());
    assert!(!user_startup.join("folder").exists() && !common_startup.join("run.bat").exists());
    // Reading stays allowed: the viewer can list and download what is there.
    assert_eq!(list(&s, "Startup").unwrap(), ["existing.lnk"]);
    assert_eq!(list(&s, ":1/CommonStartup").unwrap(), ["existing.lnk"]);
    assert!(call(&s, FsCall::OpenRead { path: t("Startup/existing.lnk") }).is_ok());
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn the_shell_startup_files_cannot_be_made_or_replaced_at_the_home_root() {
    let r = rig("shellrc");
    let rc = r.home.join(".zshrc");
    let profile = r.home.join(".bash_profile");
    std::fs::write(&rc, b"# mine").unwrap();
    let sys = Fake::guarded(
        vec![volume("A", &r.v1, true)],
        &r.root.join("bin"),
        Vec::new(),
        vec![rc.clone(), profile.clone()],
    );
    let s = server(&r.home, Arc::clone(&sys));
    let refused = Err(ErrorCode::NotPermitted);
    // Making or replacing the names, every way a change can put something there.
    assert_eq!(call(&s, FsCall::PartCreate { dir: t(""), name: t(".bash_profile") }), refused);
    assert_eq!(call(&s, FsCall::PartRename { dir: t(""), name: t("x"), target_name: t(".zshrc") }), refused);
    assert_eq!(make(&s, "", ".bash_profile"), refused);
    assert_eq!(call(&s, FsCall::Rename { dir: t(""), name: t("mine.txt"), new_name: t(".zshrc") }), refused);
    assert_eq!(call(&s, FsCall::Rename { dir: t(""), name: t("mine.txt"), new_name: t(".ZSHRC") }), refused, "case");
    std::fs::create_dir_all(r.home.join("sub")).unwrap();
    std::fs::write(r.home.join("sub").join(".bash_profile"), b"x").unwrap();
    assert_eq!(
        call(&s, FsCall::Move { from_dir: t("sub"), name: t(".bash_profile"), to_dir: t("") }),
        refused,
        "a move into the home root"
    );
    assert_eq!(std::fs::read(&rc).unwrap(), b"# mine", "nothing was replaced");
    assert!(!profile.exists());
    // Reading stays allowed, the same name below the root is free, and the file can be renamed away.
    assert!(call(&s, FsCall::OpenRead { path: t(".zshrc") }).is_ok());
    assert!(list(&s, "").unwrap().contains(&".zshrc".to_string()));
    assert!(call(&s, FsCall::PartCreate { dir: t("sub"), name: t(".zshrc") }).is_ok(), "only the home root is guarded");
    assert!(call(&s, FsCall::Rename { dir: t(""), name: t(".zshrc"), new_name: t(".zshrc.old") }).is_ok());
    assert!(r.home.join(".zshrc.old").exists());
    let _ = std::fs::remove_dir_all(&r.root);
}
