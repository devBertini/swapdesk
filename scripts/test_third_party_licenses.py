"""Tests for the pure functions of third_party_licenses.py (fake metadata, no cargo)."""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import third_party_licenses as t  # noqa: E402


def crate(name, version="1.0.0", license="MIT", files=()):
    return {"name": name, "version": version, "license": license, "repository": None, "files": list(files)}


class Tests(unittest.TestCase):
    def test_normalize_text(self):
        self.assertEqual(t.normalize_text("a  \r\nb\r\n\r\n"), "a\nb\n")

    def test_dedup_identical_texts_once(self):
        crates = [crate("b", files=[("LICENSE", "x\n")]), crate("a", files=[("LICENSE-MIT", "x\n"), ("NOTICE", "n\n")])]
        texts, refs = t.dedup_texts(crates)
        self.assertEqual(texts, ["n\n", "x\n"])
        self.assertEqual(refs[0], [("LICENSE", 1)])
        self.assertEqual(refs[1], [("LICENSE-MIT", 1), ("NOTICE", 0)])

    def test_render_is_sorted_and_deterministic(self):
        a = crate("zeta", files=[("LICENSE", "x\n")])
        b = crate("alpha", "2.0.0", files=[("LICENSE", "x\n")])
        c = crate("alpha", "1.0.0", files=[("LICENSE", "y\n")])
        first = t.render([a, b, c])
        self.assertEqual(first, t.render([c, a, b]))
        self.assertLess(first.index("### alpha 1.0.0"), first.index("### alpha 2.0.0"))
        self.assertLess(first.index("### alpha 2.0.0"), first.index("### zeta 1.0.0"))
        self.assertEqual(first.count("````text"), 2)

    def test_missing_license_text(self):
        doc = t.render([crate("bare", license="MIT OR Apache-2.0")])
        self.assertIn("MIT OR Apache-2.0", doc)
        self.assertIn(t.NO_TEXT, doc)

    def test_undeclared_license(self):
        self.assertIn("not declared", t.render([crate("odd", license=None)]))

    def test_reachable_skips_dev_build_and_workspace(self):
        meta = {
            "workspace_members": ["ws1", "ws2"],
            "resolve": {
                "nodes": [
                    {
                        "id": "ws1",
                        "deps": [
                            {"pkg": "ws2", "dep_kinds": [{"kind": None}]},
                            {"pkg": "norm", "dep_kinds": [{"kind": None}]},
                            {"pkg": "dev", "dep_kinds": [{"kind": "dev"}]},
                            {"pkg": "bld", "dep_kinds": [{"kind": "build"}]},
                        ],
                    },
                    {"id": "ws2", "deps": [{"pkg": "deep", "dep_kinds": [{"kind": "build"}, {"kind": None}]}]},
                    {"id": "norm", "deps": [{"pkg": "leaf", "dep_kinds": [{"kind": None}]}]},
                    {"id": "dev", "deps": [{"pkg": "devleaf", "dep_kinds": [{"kind": None}]}]},
                ]
            },
        }
        self.assertEqual(t.reachable_ids(meta), {"norm", "leaf", "deep"})

    def test_license_file_names(self):
        listing = [
            ("LICENSE-MIT", True),
            ("COPYING", True),
            ("licensed.rs", True),
            ("LICENSE", False),
            ("NOTICE.md", True),
            ("README", True),
        ]
        self.assertEqual(t.license_file_names("x", listing), ["COPYING", "LICENSE-MIT", "NOTICE.md"])


if __name__ == "__main__":
    unittest.main()
