# Notes for fixing a Sphinx issue

You are fixing one issue in the Sphinx documentation-generator codebase. The repo is at
`/testbed`. You can only run `python *` and `pytest *` commands — no `tox`, no
`sphinx-build`, no `make`. Follow this order.

## 1. Locate: identify the owning layer before reading code

The report's own words almost always name the module directly:

- **A domain** (glossary, cross-references, object indices) → `sphinx/domains/<name>.py`.
  Each domain owns its own per-build data and a `clear_doc`/`process_doc`/
  `merge_domaindata`/`resolve_xref`/`get_objects` lifecycle. Duplicate-object warnings
  come from an exact string-key collision in the domain's own registry dict — not a
  fuzzy or case-folded comparison. Unqualified cross-reference resolution has a fuzzy
  fallback that matches *any* object ending in `.name`, regardless of scope — a wrong
  link often means this fallback matched the wrong candidate.
- **A builder or writer** (a specific output format: html/dirhtml/singlehtml/epub/latex/
  text/...) → `sphinx/builders/<fmt>*.py` + `sphinx/writers/<fmt>.py`. `Builder.name` (the
  `-b` flag) and `Builder.format` (the output family) are separate attributes;
  `dirhtml`/`singlehtml`/`epub` all share `format='html'` under different `name`s. Code
  gated on `name` alone silently misses the format-siblings.
- **Autodoc** (a documented class/function/property/attribute is wrong or missing) →
  `sphinx/ext/autodoc/__init__.py`. Which member "wins" is a priority-ordered competition
  across `Documenter` subclasses (`can_document_member` + `priority`, highest wins), not
  a single if/elif. Typehints are a separate, later mechanism
  (`sphinx/ext/autodoc/typehints.py`): captured on one event, re-attached on another, only
  for the Python domain — a bug can be in either half.
- **An extension or event ordering issue** → check `setup()` in `sphinx/ext/<name>/` and
  the fixed build-event order: `config-inited` → `builder-inited` → per-doc
  `env-purge-doc`/`source-read`/`doctree-read` → `env-merge-info` (parallel only) →
  `env-check-consistency` → per-doc post-transforms → `doctree-resolved` (with
  `missing-reference`/`warn-missing-reference` as its fallback) → `build-finished`.
  Handlers can register with an explicit `priority`.
- **A directive** with several interacting options (e.g. `literalinclude`) →
  `sphinx/directives/`. Options are validated by a mutual-exclusion table, then filters
  run in one fixed order: `pyobject → start/end → lines → prepend → append → dedent`. A
  bug where one option seems to "break" another is usually this fixed order, not either
  option alone.

To find the implementation and its existing tests together: `sphinx/<module>.py` and
`tests/test_<module>.py` follow a fuzzy but real 1:1 convention. Not perfectly
mechanical — autodoc has extra feature-specific test files
(`test_ext_autodoc_autoproperty.py`, `test_ext_autodoc_private_members.py`, ...), and
every domain funnels into one `test_domain_<name>.py`. Confirm with a glob before
assuming a name.

## 2. Reproduce before editing, at the lightest weight that shows the bug

Current contributor guidance: avoid the `app` fixture and `app.build()` unless a full
integration test is required. In increasing weight:

1. **No Sphinx app at all**, for self-contained text-transform code: e.g. napoleon's
   `GoogleDocstring(docstring, config=...)`/`NumpyDocstring(...)` take a plain string,
   `app=None` is supported. Use this tier only when the bug is purely about converting a
   docstring/type-spec string.
2. **`sphinx.testing.restructuredtext.parse(app, text, docname='index')`** — an RST
   string in, a doctree out, no write phase. Covers directive output, autodoc member
   presence, glossary/domain registration, single-document cross-references.
3. **A minimal real build**: `python -m sphinx <srcdir> <outdir> -b <builder>` (this is
   exactly the `sphinx-build` console script, legal here since `sphinx/__main__.py` just
   calls `sphinx.cmd.build.main`). Needed for actual rendered-file output, multi-document/
   environment-level behavior (cross-file resolution, consistency checks, anything after
   `env-updated`), or a builder whose own behavior is the feature (e.g. linkcheck).
4. **Reuse `tests/roots/test-<slug>/`** (132 tiny pre-built fixture projects, one per
   feature area) instead of writing your own — grep `tests/test_*.py` for
   `testroot='<slug>'` to see how it's normally driven.

Read the captured `status`/`warning` output (or stderr for a direct `python -m sphinx`
run) rather than guessing whether a warning fired.

## 3. Recurring bug shapes — check if yours is one

- **Wrong/missing location in a warning**: usually a broken parent-fallback chain for a
  node built without a linked parent. Fix idiom: `logger.warning(msg, location=node)`,
  passing the node itself, not a hand-built string.
- **Directive option interaction** (esp. `literalinclude`/`code-block`): check the fixed
  filter order above before assuming either option is broken alone.
- **Docstring/type-spec tokenizer** (napoleon) not handling a new punctuation shape: fix
  is usually the regex/token-recombination step plus a new case.
- **Duplicated member-kind detection across autodoc `Documenter` subclasses**: a fix for
  one member kind (e.g. `PropertyDocumenter`) often needs the same change in a sibling
  subclass, or it only half-fixes the report. The file is ~2800 lines — grep for the
  specific class/check by name.
- **Changed confval or option**: check backward compatibility for existing usage first.
  A confval's `rebuild` value (`'env'`/`'html'`/`''`) states what changing it invalidates;
  a changed default is a compatibility question before it is a feature question.
- **Builder-gated code restricted by `builders = (...)`**: consider whether it should be
  `formats = (...)` instead, so `dirhtml`/`singlehtml`/`epub` aren't silently excluded
  from an `html`-family fix.

## 4. Fix

Smallest change matching what you reproduced. If the bug is duplicated logic, fix every
sibling call site you found in step 3, not just the one the report names. Don't
restructure a class "while you're in there."

## 5. Verify with a targeted test, not a full suite run

- `python -m pytest tests/test_<module>.py::test_<name>` (or the whole file, no `::`
  filter, to also catch sibling regressions). Should take well under a minute — if not,
  your reproduction is probably wrong.
- Prefer `assert_node(...)` (from `sphinx.testing.util`) over comparing rendered strings
  when the fix touches doctree structure.
- If the fix touches a builder's output, use `@pytest.mark.sphinx(builder, testroot=...)`
  or a direct `python -m sphinx -b <builder>` build and read the produced file.
- Never accept "no crash" as verification. If you cannot get a real assertion to pass or
  fail meaningfully, say so explicitly.

## 6. Before finishing

- Add a CHANGES entry: `* #NNNN: <component>: <one-line description>`. If the issue
  number isn't knowable, still write the component + description line.
- Deprecating instead of removing a public attribute: add/reuse a
  `RemovedInSphinxNNWarning` class, add a `doc/extdev/deprecated.rst` row, keep the
  attribute working via a `@property` that warns before returning the old value, and add
  a CHANGES entry under a `Dependencies` section.
- Re-check: did you fix every sibling call site of a duplicated check? Did you avoid
  changing an unrelated default's behavior as a side effect?

## 7. Work from the checked-out tree only

`/testbed` already has the version you're fixing. Never download or install another copy
of Sphinx (`pip download`/`pip install sphinx`, `pip install --target`, cloning or
fetching the project's own GitHub repo, a PR diff, or a PyPI file) to read its source or
crib a fix from a newer release — that release's fix may not match this version's code
and doing this voids the run regardless of whether the patch looks right. Installing
Sphinx's own *dependencies* (docutils, pytest, jinja2, and the rest of what an import
already needs) is fine and sometimes necessary. If reproduction needs a package, install
the package, not a second Sphinx.
