"""``python -m evals.packs.swebench_verified``."""

from __future__ import annotations

import sys

from ...core.cli import main
from .pack import SwebenchVerifiedPack

if __name__ == "__main__":
    sys.exit(main(SwebenchVerifiedPack()))
