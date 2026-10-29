"""This pack's own test suite.

Run it with everything else, from the repository root:

```bash
python3 -m unittest discover -s evals -t .
```

Everything here needs a suite, an image or the official grader, which is why it
is not in :mod:`evals.core.tests`. Nothing opens a socket, reads a provider key
or starts a container: every test drives the pack through
:class:`~evals.core.process.ReplayRunner`, so what is under test is the exact
argv a measured sweep would issue and the exact records it would write — the
only substitution is the kernel call.
"""
