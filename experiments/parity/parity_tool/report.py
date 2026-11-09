"""Turn comparisons into a readable report and a machine-readable one."""

from __future__ import annotations

from .compare import AGREE, DIFFER, INCOMPLETE, Comparison, Section

WORDS = {AGREE: "AGREE", DIFFER: "DIFFER", INCOMPLETE: "INCOMPLETE"}


def _cell(section: Section) -> str:
    return "same" if section.agrees else "DIFFERS"


def summary_table(comparisons: list[Comparison]) -> list[str]:
    """One line per run: how each part compared and the verdict."""
    names = [section.name for section in comparisons[0].sections] if comparisons else []
    width = max([len(c.spec.id) for c in comparisons] + [3])
    column = max([len(name) for name in names] + [len("DIFFERS")])
    head = "run".ljust(width) + "  " + "  ".join(n.ljust(column) for n in names) + "  verdict"
    lines = [head, "-" * len(head)]
    for c in comparisons:
        cells = "  ".join(_cell(s).ljust(column) for s in c.sections)
        lines.append(f"{c.spec.id.ljust(width)}  {cells}  {WORDS[c.status]}")
    return lines


def detail_lines(c: Comparison) -> list[str]:
    """Everything known about one run that did not agree."""
    lines = [f"== {c.spec.id}: {WORDS[c.status]}"]
    if c.spec.session.description:
        lines.append(f"   {c.spec.session.description}")
    for note in c.notes:
        lines.append(f"   note: {note}")
    for section in c.sections:
        lines.append(f"   [{section.name}] {'same' if section.agrees else 'DIFFERS'}")
        lines.extend("      " + line for line in section.lines)
    return lines


def stock_lines(comparisons: list[Comparison], verbose: bool) -> list[str]:
    """The stock agent's own text, grouped so a shared difference is shown once."""
    lines = [
        "Differences in the stock agent's own text (system prompt other than the rules,",
        "tool definitions, other messages and tool results). Information only: the two",
        "builds have different bases, so these are expected and do not change a verdict.",
        "",
    ]
    groups: dict[tuple, list[Comparison]] = {}
    for c in comparisons:
        groups.setdefault(tuple(c.stock.lines), []).append(c)
    for key, members in groups.items():
        ids = ", ".join(m.spec.id for m in members)
        head = "same" if members[0].stock.agrees else "differs"
        lines.append(f"[{head}] {len(members)} run(s): {ids if verbose or len(members) <= 4 else ids[:200] + ' …'}")
        shown = key if verbose else key[:12]
        lines.extend("      " + line for line in shown)
        if len(key) > len(shown):
            lines.append(f"      … {len(key) - len(shown)} more lines (use --verbose or see report.json)")
        lines.append("")
    return lines


def render(
    comparisons: list[Comparison],
    names: tuple[str, str],
    paths: tuple[str, str],
    verbose: bool = False,
    out_dir: str = "",
) -> str:
    """The whole text report."""
    counts = {key: sum(1 for c in comparisons if c.status == key) for key in WORDS}
    lines = [
        "Parity report",
        f"  {names[0]}: {paths[0]}",
        f"  {names[1]}: {paths[1]}",
        f"  runs: {len(comparisons)}; agree {counts[AGREE]}, differ {counts[DIFFER]}, "
        f"incomplete {counts[INCOMPLETE]}",
        "",
        *summary_table(comparisons),
        "",
    ]
    for c in comparisons:
        if c.status != AGREE:
            lines.extend(detail_lines(c))
            lines.append("")
        elif verbose:
            lines.extend(detail_lines(c))
            lines.append("")
        elif c.notes:
            lines.append(f"{c.spec.id}: " + "; ".join(c.notes))
    lines.extend(stock_lines(comparisons, verbose))
    if out_dir:
        lines.append(f"Files for every run (requests, record, stdout, stderr): {out_dir}")
    return "\n".join(lines).rstrip() + "\n"


def to_json(comparisons: list[Comparison], names: tuple[str, str], paths: tuple[str, str]) -> dict:
    """The report as plain data."""
    return {
        "builds": {names[0]: paths[0], names[1]: paths[1]},
        "runs": [
            {
                "id": c.spec.id,
                "description": c.spec.session.description,
                "exercises": c.spec.session.exercises,
                "verdict": c.status,
                "notes": c.notes,
                "sections": {
                    s.name: {"agrees": s.agrees, "lines": s.lines} for s in c.sections
                },
                "stock_text": {"agrees": c.stock.agrees, "lines": c.stock.lines},
                "facts": c.facts,
            }
            for c in comparisons
        ],
    }
