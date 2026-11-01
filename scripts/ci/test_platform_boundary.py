#!/usr/bin/env python3
"""Self-test for platform_boundary.py: a deliberate violation must fail, exemptions must pass."""
import os, subprocess, sys, tempfile, unittest

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "platform_boundary.py")


def run(tree):
    with tempfile.TemporaryDirectory() as root:
        for path, text in tree.items():
            p = os.path.join(root, path)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w") as f:
                f.write(text)
        r = subprocess.run([sys.executable, SCRIPT, "--root", root], capture_output=True, text=True)
        return r.returncode, r.stdout


class PlatformBoundary(unittest.TestCase):
    def test_std_os_unix_in_scanner_fails(self):
        code, out = run({"crates/scull-scanner/src/lib.rs": "use std::os::unix::fs::MetadataExt;\n"})
        self.assertEqual(code, 1, out)
        self.assertIn("crates/scull-scanner/src/lib.rs:1", out)

    def test_libc_dependency_fails(self):
        code, out = run({"crates/scull-cli/Cargo.toml": '[package]\nname="x"\n[dependencies]\nlibc = "0.2"\n'})
        self.assertEqual(code, 1, out)

    def test_windows_sys_renamed_dependency_fails(self):
        code, out = run({"crates/scull-git/Cargo.toml": '[target.\'cfg(windows)\'.dependencies]\nws = { package = "windows-sys", version = "0.59" }\n'})
        self.assertEqual(code, 1, out)

    def test_extern_c_fails(self):
        code, out = run({"crates/scull-cleaner/src/x.rs": 'extern "C" {\n fn unlinkat(); }\n'})
        self.assertEqual(code, 1, out)

    def test_entry_metadata_outside_the_scanner_fails(self):
        # Listing-derived metadata must never reach revalidation, the executor or purges (#91).
        for path in ("crates/scull-cleaner/src/revalidate.rs", "crates/scull-safety/src/x.rs",
                     "crates/scull-fixtures/src/snap.rs"):
            for call in ("let md = p.entry_metadata(&e)?;\n",
                         "let md = Platform::entry_metadata(&p, &e)?;\n"):
                code, out = run({path: call})
                self.assertEqual(code, 1, (path, call, out))
                self.assertIn("entry_metadata", out)

    def test_entry_metadata_in_the_scanner_platform_and_tests_passes(self):
        code, out = run({
            "crates/scull-scanner/src/walk.rs": "let md = self.platform.entry_metadata(&de);\n",
            "crates/scull-platform/src/lib.rs": "fn entry_metadata(&self, e: &DirEntry) {}\nos::entry_metadata(e)\n",
            "crates/scull-platform/tests/entry_metadata.rs": "p.entry_metadata(&e).unwrap();\n",
            "crates/scull-cleaner/tests/it.rs": "p.entry_metadata(&e).unwrap();\n",
            "crates/scull-cleaner/src/x.rs": "pub fn f() {}\n#[cfg(test)]\nmod t { fn g() { p.entry_metadata(&e); } }\n",
            # Defining it (a fake platform) is not calling it.
            "crates/scull-engine/src/fake.rs": "fn entry_metadata(&self, e: &DirEntry) -> Result<Metadata> { todo!() }\n",
        })
        self.assertEqual(code, 0, out)

    def test_platform_crate_is_exempt(self):
        code, out = run({
            "crates/scull-platform/src/unix.rs": "use std::os::unix::fs::MetadataExt;\nlibc::stat;\n",
            "crates/scull-platform/Cargo.toml": '[dependencies]\nlibc = "0.2"\n',
        })
        self.assertEqual(code, 0, out)

    def test_test_code_and_dev_deps_are_exempt(self):
        code, out = run({
            "crates/scull-core/src/path.rs": "pub fn f() {}\n#[cfg(test)]\nmod tests { use std::os::unix::ffi::OsStrExt; }\n",
            "crates/scull-git/tests/it.rs": "use std::os::unix::fs::symlink;\n",
            "crates/scull-git/Cargo.toml": '[dev-dependencies]\nlibc = "0.2"\n',
        })
        self.assertEqual(code, 0, out)

    def test_code_after_test_module_is_still_checked(self):
        code, out = run({"crates/scull-scanner/src/lib.rs": (
            "pub fn a() {}\n#[cfg(test)]\nmod tests {\n    use std::os::unix::fs::symlink;\n    #[test]\n    fn t() { if true { } }\n}\n"
            "pub fn ino(m: &std::fs::Metadata) -> u64 { use std::os::unix::fs::MetadataExt; m.ino() }\n"
        )})
        self.assertEqual(code, 1, out)
        self.assertIn("lib.rs:8", out)

    def test_cfg_test_use_and_fn_are_exempt(self):
        code, out = run({"crates/scull-core/src/a.rs": (
            "#[cfg(test)]\nuse std::os::unix::ffi::OsStrExt;\n#[cfg(test)]\n#[allow(dead_code)]\nfn helper() {\n    let _ = std::os::unix::fs::symlink;\n}\npub fn ok() {}\n"
        )})
        self.assertEqual(code, 0, out)

    def test_comments_are_ignored(self):
        code, out = run({"crates/scull-core/src/a.rs": "// never use std::os::unix here\n"})
        self.assertEqual(code, 0, out)


if __name__ == "__main__":
    unittest.main(verbosity=2)
