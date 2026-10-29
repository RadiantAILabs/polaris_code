"""``python -m evals.packs.swebench_multilingual``.

The pack is constructed here and handed to the shared command line, so a
second dataset's entry point is four lines of the same shape.
"""

from __future__ import annotations

import sys

from ...core.cli import main
from .pack import SwebenchMultilingualPack

if __name__ == "__main__":
    sys.exit(main(SwebenchMultilingualPack()))
