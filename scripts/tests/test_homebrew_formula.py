"""Check the release-to-Homebrew integrity boundary without network access."""

import hashlib
import importlib.util
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "homebrew_formula.py"
SPEC = importlib.util.spec_from_file_location("homebrew_formula", SCRIPT)
homebrew_formula = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(homebrew_formula)


class HomebrewFormulaTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.artifacts = Path(self.temporary.name)
        self.archives = []
        for target in homebrew_formula.TARGETS:
            archive = self.artifacts / f"polaris-code-0.1.1-{target}.tar.gz"
            archive.write_bytes(f"release archive for {target}".encode())
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            archive.with_suffix(".gz.sha256").write_text(f"{digest}  {archive.name}\n")
            self.archives.append(archive)

    def generate(self, version="0.1.1", repository="RadiantAILabs/polaris_code"):
        return homebrew_formula.generate_formula(version, repository, self.artifacts)

    def test_formula_selects_archives_with_exact_checksums(self):
        formula = self.generate()
        for archive in self.archives:
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            self.assertIn(f"/releases/download/v0.1.1/{archive.name}", formula)
            self.assertIn(f'sha256 "{digest}"', formula)
        self.assertIn('bin.install "polaris"', formula)
        self.assertNotIn('depends_on "rust"', formula)
        self.assertNotIn("bottle do", formula)
        self.assertIn('inventory.fetch("entries")', formula)

    def test_rejects_modified_archive(self):
        self.archives[0].write_bytes(b"modified after packaging")
        with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
            self.generate()

    def test_rejects_checksum_for_another_filename(self):
        archive = self.archives[0]
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        archive.with_suffix(".gz.sha256").write_text(f"{digest}  another-archive.tar.gz\n")
        with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
            self.generate()

    def test_requires_every_platform(self):
        self.archives[1].unlink()
        with self.assertRaises(FileNotFoundError):
            self.generate()

    def test_rejects_unsafe_version_or_repository(self):
        with self.assertRaises(ValueError):
            self.generate(version='0.1.1"; system "false')
        with self.assertRaises(ValueError):
            self.generate(repository='owner/repo"')


if __name__ == "__main__":
    unittest.main()
