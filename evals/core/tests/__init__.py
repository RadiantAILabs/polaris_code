"""The evaluation core's own test suite.

Run it with everything else, from the repository root:

```bash
python3 -m unittest discover -s evals -t .
```

Nothing here names a dataset. These tests cover the budget guard, the usage
passthrough, the bootstrap and the collation — the parts that must behave
identically whichever pack is running. A test that needs a suite, an image or a
grader belongs in that pack's own ``tests`` package.
"""
