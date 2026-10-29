"""The pinned, 500-instance SWE-bench Verified dataset pack.

This pack owns the official Verified dataset pin, its 500-instance denominator,
the official SWE-bench environment and evaluator adapters, and the narrowly
scoped policy that governs those inputs.  The shared evaluation core continues
to own scheduling, accounting, evidence, and run persistence.
"""

__all__ = ["__version__"]

#: Version of this pack's pinned content and adapters.
__version__ = "1.0.0"
