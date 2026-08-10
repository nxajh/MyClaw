#!/usr/bin/env python3
"""Add `silenced_override: None,` after `interruption_scope_id: None,` in
ChannelInboundMessage literals (preserving indentation). Skips the two sites
already updated in orchestrator/delegation.rs."""
import re, pathlib, sys

ROOT = pathlib.Path("/home/ubuntu/.myclaw/workspace/MyClaw/src")
FILES = [
    "channels/telegram/channel.rs",
    "channels/client.rs",
    "channels/wechat.rs",
    "channels/qqbot/channel.rs",
    "agents/ask_router.rs",
    "agents/delegation_coordinator.rs",
    "agents/agent.rs",
    "agents/orchestrator/test_support.rs",
    "agents/orchestrator/scheduled.rs",
]

pat = re.compile(r"^(\s*)interruption_scope_id: None,$", re.M)
for rel in FILES:
    p = ROOT / rel
    text = p.read_text()
    out, n = pat.subn(
        lambda m: f"{m.group(1)}interruption_scope_id: None,\n{m.group(1)}silenced_override: None,",
        text,
    )
    p.write_text(out)
    print(f"{rel}: {n} site(s) updated")
    if n == 0:
        sys.exit(f"!! no matches in {rel}")
