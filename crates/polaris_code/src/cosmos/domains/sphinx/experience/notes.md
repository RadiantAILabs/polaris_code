Sphinx notes: practical advice for fixing a bug in a Sphinx checkout.

## Environment first, before you search for the bug

This checkout's own runtime dependencies (`docutils`, `Jinja2`, `Pygments`, `snowballstemmer`,
`imagesize`, `requests`, `packaging`, `Babel`, `alabaster`, and the `sphinxcontrib-{applehelp,
devhelp,htmlhelp,jsmath,qthelp,serializinghtml}` family) are often not preinstalled. If any import of
`sphinx` or a build fails with `ModuleNotFoundError`/`ImportError`, install the whole set in one `pip
install` call rather than discovering them one at a time as each new import fails — that one-at-a-time
loop is a common source of wasted turns. If a fresh `alabaster` from PyPI
raises a version-requirement error against this older checkout, pin `alabaster==0.7.13`. Installing
these ordinary third-party dependencies is always fine; work only with the Sphinx checkout already in
the workspace.

## Locating the change

Sphinx's own naming is literal: the option, directive, or role name in the bug report is usually the
exact string to search for, and it is usually enough on its own — search for the exact term from the statement in the most relevant module first (`sphinx/domains/
python.py` for autodoc/xref behavior, `sphinx/ext/autodoc/__init__.py` for member/documentation
filtering, `sphinx/builders/*.py` for a specific output format, `sphinx/util/*.py` for a shared helper).
The `CHANGES` file at the repo root is useful background for when a regression was introduced, but
searching it is not a substitute for finding the actual source location — treat a `CHANGES` hit as a
pointer to go re-read the real module, not as the place to make the edit. If a `read_file` call is
truncated, read the specific remaining range next rather than re-reading the same prefix — rereading a
file's start over and over without narrowing wastes turns without adding information.

## Reproducing and checking: exercise the same layer the bug is actually about

This matters most. Before submitting, run the change through the real code, not a hand-written
stand-in for it:

- When the fix is a self-contained expression or a two-branch decision (the whole bug fits in one
  function or one regex), it is enough to exercise that exact expression directly, live, against the
  before/after cases in the statement.
- When the bug is about how a lower-level result gets **used or scoped by its caller** — a resolver
  choosing a search mode, a documenter deciding whether to render a member, a domain merging annotations
  — testing only the inner function is not enough, even if that inner function is real and unmocked.
  Trace one level further out to the actual caller that makes the decision the bug report is about, and
  exercise the change through that caller, not around it.
- When a real Sphinx build is feasible (a minimal `conf.py` plus one source file matching the reported
  shape, run through `sphinx-build`), prefer it over any reimplementation: it is the only way to see the
  same rendered output, or the same xref resolution, that the shipped test suite and any real user would
  see. A check that only asserts on the on-disk source text (e.g., "the regex now says X") is weaker
  evidence than actually running the changed code and observing its output.
- A check that reimplements the logic being fixed, standalone, is the least reliable form: it can only
  ever assert what its author remembered to include, and it is exactly the form that has missed a wider
  regression scope.

## Read the project's own tests before deciding your check is complete

Sphinx ships an extensive `tests/test_*.py` suite plus fixture roots under `tests/roots/test-*` for
almost every feature area. Search `tests/` for the function, class, or option name you are changing
before finishing your own check. The shipped tests routinely cover more of a feature than one bug report
describes — anchor validation living in the same test file as broken-link detection, an abstract or
inherited member needing the same treatment as a plain one, a prefixed reference needing the same
scoping as a bare one. A patch and a check that only address the one example in the statement, when the
project's own visible tests already assert a wider contract for the same feature, is a narrower fix than
what is actually being asked for. If you find such a test, treat what it asserts as part of the target
behavior, not merely as confirmation to read and move past.

## Before submitting, check for siblings

Once you have found the function or class that needs to change, do one more scoped search for sibling
implementations sharing the same name or role — another subclass, another builder, another domain
handling the analogous case — that may need the identical change. An update applied to only one of
several similar call sites, when the others share the same defect, is a common failure mode.

## If a repair round fires because your own check crashed

A failure that shows an exception raised while *constructing your check's own fixtures or imports*
(`ModuleNotFoundError`, `ImportError`, a `TypeError` from a hand-built fake object's constructor) is a
bug in the check, not evidence against the patch. Fix that specific problem — swap the failing import
for a working equivalent, correct the fixture's construction — but do not use the opportunity to retreat
to a narrower or more fully mocked check just to make the crash go away. After fixing an environment
problem, keep calling the real target function through its real entry point; do not spend the repair
budget deepening a hand-built fixture instead of adding a real build or a call through the actual
rendering path. If you cannot get a real dependency working after one focused attempt, prefer
a check that still calls the real function directly over one that reimplements its logic.

## Use the required second confirmation to add coverage, not just to resubmit

You will be asked, once, to reply again before your first submission is accepted. When your first check
was already a real, adequately scoped verification, resubmitting it unchanged is correct. But when your
first check was thin — a single case, a source-text assertion instead of a real run — this is the moment
to broaden it: add the adjacent cases the statement implies (a too-large value alongside the reported
too-small one, an inherited case alongside the direct one, a full build alongside a unit assertion). Both
of these improve the eventual check's coverage; neither makes a check worse.
