#!/usr/bin/env python3
"""Run scripted sessions through two agent binaries and compare what they do.

    python3 experiments/parity/parity.py OLD_BINARY NEW_BINARY

See README.md in this folder.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from parity_tool.cli import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())
