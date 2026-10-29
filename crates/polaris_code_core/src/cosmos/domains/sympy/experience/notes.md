## Environment

sympy's source lives at `/testbed`; put it on `sys.path` (or run from that
directory) before importing. A plain `import sympy` after that commonly still
fails until you `pip install mpmath` — do this first, before spending turns
diagnosing an unrelated-looking import error. Depending on the area of the
issue you may also need `pip install numpy` (matrix/array-heavy code),
`pip install pytest` (to run the real test files instead of ad hoc scripts),
or, for anything touching LaTeX parsing, `pip install antlr4-python3-runtime==4.7.2`.
Doing these installs immediately, before exploring the code, avoids losing
several turns to rediscovering them mid-investigation.

## Locate and reproduce

Read the traceback or example in the issue literally: it usually names the
exact function and call chain where the bug lives, so start there instead of
a broad search. Reproduce the reported behavior first, in a small script,
before touching any code — confirm you can see the wrong output the issue
describes.

Once you find the function that needs to change, search for its other
callers and for sibling functions that solve a similar problem correctly.
sympy has a lot of parallel structure (e.g. one function per operation per
type); a nearby sibling that already handles the case correctly is often the
clearest guide to what the buggy function is missing, and checking other
callers of the function you're changing helps avoid a fix that's too narrow
for how it's actually used elsewhere.

## Verify

Prefer checking behavior in place before finalizing: temporarily apply your
change to the file on disk (or patch the loaded module's source and re-import
it), confirm the issue's own example now behaves as expected, then revert the
in-place change once you're satisfied — the edit you actually submit is
separate from this scratch step. This catches mistakes an isolated
reimplementation would miss, since it exercises the real code path.

Where the affected module has its own test file, run it for real
(`pytest <path>` or the project's own test runner) rather than relying only
on a script you wrote — real test suites cover interactions you won't think
to check yourself, and a broader sweep can catch a fix that helps the
reported case but breaks something else. If a run shows a failure, check
whether it already fails without your change before assuming your fix caused
it — you can temporarily set your change aside to check this instead of
assuming.

Running the real suite raises confidence but does not by itself guarantee
your fix matches how correctness will actually be judged — a thorough
verification pass can still miss the specific thing being checked, so don't
treat "the suite passes" as proof beyond what it actually tested. Combined
with that, be careful about how you check correctness: prefer checking that
a result is mathematically/semantically right (e.g. that the difference from
the expected value simplifies to zero, or that two expressions are provably
equal) over checking that its printed string matches a string you expected.
But sympy's own printing, LaTeX, and parsing code is often tested by exact
string or LaTeX comparison rather than semantic equality, so for an issue
about output formatting specifically, also check the exact string or LaTeX
form the issue shows as expected, not only that the value is equivalent —
a fix that's numerically or symbolically correct can still look different
from the exact output the issue expects, and for formatting-related issues
that difference matters.

## General

Fix the root cause named by the traceback or the smallest function that
produces the wrong behavior, rather than special-casing the one example in
the issue. Keep the change scoped to what the issue describes; avoid
rewriting surrounding code that already works.
