# Code Intelligence

- Use CodeGraph when it materially improves understanding of unfamiliar code, execution flows, dependencies, or change impact.
- CodeGraph is optional rather than a pre-edit or pre-commit gate. If it is unavailable, stale, or unhelpful, continue with LSP, direct file reads, and targeted source search.
- Before changing central or high-risk behavior, inspect relevant callers, callees, tests, and request flows using the most effective available tools, then report material risk to the user.
- Use symbol-aware tooling for non-trivial renames; avoid blind repository-wide replacement.
