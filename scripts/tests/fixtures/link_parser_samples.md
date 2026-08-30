# Shared link-parser fixture (Python ⇄ Rust)

This file pins the markdown link-parsing contract shared by two consumers:

- Rust `src/memory::extract_links` / `parse_md_link` (graph edges)
- Python `scripts/migrate-memory-split.py` `parse_md_href` (link rewrite)

Prose outside `## See Also` is ignored by `extract_links` — including
canonical memory links, which pins the section gating:

- [Ignored outside See Also](agent:outside_section.md)

An inline [also ignored](user:outside_inline.md) reference.

## See Also

- [Related: bare same layer](alpha_agent.md)
- [Related: cross to agent](agent:beta_user.md)
- [Related: cross to user](user:gamma_user.md)
- [External http](http://example.com/plain)
- [External https with md](https://example.com/doc.md)
- [Anchor](#section-heading)
- [Mail](mailto:someone@example.com)
- [Bare name](alpha_agent)
- [Empty target]()
- [Case variant suffix](Mixed_Case.MD)
- [Path qualified](docs/notes/alpha_agent.md)
- [First of two](alpha_agent.md) and [second on the line is ignored](user:gamma_user.md)

## Notes

Any `## ` heading ends the See Also section, so this canonical link is
ignored too: [After section ends](agent:after_section.md)
