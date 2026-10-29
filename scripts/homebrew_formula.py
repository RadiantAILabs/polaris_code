#!/usr/bin/env python3
"""Generate a binary Homebrew formula from the exact release archive bytes."""

import argparse
import hashlib
import re
from pathlib import Path


TARGETS = ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu")


def generate_formula(version: str, repository: str, artifacts: Path) -> str:
    """Validate both archives and their checksums before rendering a formula."""
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("Expected an application version such as 0.1.1")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("Expected a GitHub owner/repository")

    checksums = {}
    for target in TARGETS:
        filename = f"polaris-code-{version}-{target}.tar.gz"
        archive = artifacts / filename
        with archive.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        checksum = (artifacts / f"{filename}.sha256").read_text().split()
        if checksum != [digest, filename]:
            raise ValueError(f"Checksum mismatch for {filename}")
        checksums[target] = digest

    base_url = f"https://github.com/{repository}"
    release_url = f"{base_url}/releases/download/v{version}"
    return f'''# Generated from verified release archives by scripts/homebrew_formula.py.
class PolarisCode < Formula
  desc "Composable coding agent with replaceable plugins and typed execution graphs"
  homepage "{base_url}"
  version "{version}"
  license "Apache-2.0"

  on_macos do
    depends_on macos: :sequoia
    depends_on arch: :arm64

    on_arm do
      url "{release_url}/polaris-code-{version}-aarch64-apple-darwin.tar.gz"
      sha256 "{checksums['aarch64-apple-darwin']}"
    end
  end

  on_linux do
    depends_on arch: :x86_64

    on_intel do
      url "{release_url}/polaris-code-{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "{checksums['x86_64-unknown-linux-gnu']}"
    end
  end

  conflicts_with "polaris", because: "both install a polaris executable"

  def install
    bin.install "polaris"
  end

  test do
    assert_match "polaris #{{version}}", shell_output("#{{bin}}/polaris --version")
    assert_match "Usage: polaris", shell_output("#{{bin}}/polaris --help")
    inventory = JSON.parse(shell_output("#{{bin}}/polaris #{{testpath}} --list-topologies --output-format json"))
    assert_includes inventory.fetch("entries").map {{ |entry| entry.fetch("id") }}, "code.direct-edit"
  end
end
'''


def main() -> None:
    """Write the formula alongside the release archives."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("repository")
    parser.add_argument("artifacts", type=Path)
    args = parser.parse_args()
    formula = generate_formula(args.version, args.repository, args.artifacts)
    output = args.artifacts / "polaris-code.rb"
    output.write_text(formula)
    print(output)


if __name__ == "__main__":
    main()
