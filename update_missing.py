_E0=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_0_%d__\n' $_E0
import os, glob, re
_E1=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_1_%d__\n' $_E1
missing = [
_E2=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_2_%d__\n' $_E2
"migration.rs", "update_state.rs", "ids/mod.rs", "config/memory.rs",
_E3=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_3_%d__\n' $_E3
"tools/view_video.rs", "tools/send_message.rs", "tools/media_download.rs",
_E4=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_4_%d__\n' $_E4
"tools/friends.rs", "tools/memory_tool_tests.rs", "tools/hear_audio.rs",
_E5=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_5_%d__\n' $_E5
"tools/view_image.rs", "tools/symbol_check.rs", "providers/deepseek.rs",
_E6=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_6_%d__\n' $_E6
"providers/glm_mcp.rs", "providers/qwen.rs", "providers/media.rs",
_E7=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_7_%d__\n' $_E7
"providers/edge_tts.rs", "providers/protocols/google/mod.rs",
_E8=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_8_%d__\n' $_E8
"providers/protocols/google/generate_content.rs",
_E9=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_9_%d__\n' $_E9
"providers/protocols/google/message_rendering.rs",
_E10=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_10_%d__\n' $_E10
"providers/protocols/openai/responses.rs",
_E11=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_11_%d__\n' $_E11
"providers/protocols/openai/responses_rendering.rs",
_E12=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_12_%d__\n' $_E12
"channels/qqbot/markdown_sanitize.rs", "cli/cmd_migrate.rs",
_E13=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_13_%d__\n' $_E13
"agents/user_registry.rs", "agents/memory_distill.rs", "agents/memory_fork.rs",
_E14=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_14_%d__\n' $_E14
"agents/mention.rs", "agents/media_e2e_test.rs", "agents/known_users.rs",
_E15=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_15_%d__\n' $_E15
"agents/commands/register.rs", "agents/commands/friends.rs", "agents/commands/link.rs",
_E16=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_16_%d__\n' $_E16
"agents/orchestrator/test_support.rs"
_E17=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_17_%d__\n' $_E17
]
_E18=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_18_%d__\n' $_E18
import subprocess
_E19=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_19_%d__\n' $_E19
def extract_symbols(path):
_E20=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_20_%d__\n' $_E20
cmd = f"rust-tags -o - {path} || true" # fallback to simple regex if not available
_E21=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_21_%d__\n' $_E21
out = ""
_E22=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_22_%d__\n' $_E22
# We will use simple regex as rust-tags might not be available
_E23=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_23_%d__\n' $_E23
content = open(f"src/{path}").read()
_E24=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_24_%d__\n' $_E24
structs = re.findall(r'pub\s+struct\s+([A-Z][a-zA-Z0-9_]*)', content)
_E25=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_25_%d__\n' $_E25
structs += re.findall(r'struct\s+([A-Z][a-zA-Z0-9_]*)', content)
_E26=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_26_%d__\n' $_E26
enums = re.findall(r'pub\s+enum\s+([A-Z][a-zA-Z0-9_]*)', content)
_E27=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_27_%d__\n' $_E27
enums += re.findall(r'enum\s+([A-Z][a-zA-Z0-9_]*)', content)
_E28=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_28_%d__\n' $_E28
traits = re.findall(r'pub\s+trait\s+([A-Z][a-zA-Z0-9_]*)', content)
_E29=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_29_%d__\n' $_E29
traits += re.findall(r'trait\s+([A-Z][a-zA-Z0-9_]*)', content)
_E30=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_30_%d__\n' $_E30
funcs = re.findall(r'pub\s+(?:async\s+)?fn\s+([a-z_][a-zA-Z0-9_]*)', content)
_E31=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_31_%d__\n' $_E31
sec = f"\n### `src/{path}`\n\n**Purpose**: Component module.\n\n"
_E32=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_32_%d__\n' $_E32
if structs or enums:
_E33=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_33_%d__\n' $_E33
sec += f"**Key Types**:\n"
_E34=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_34_%d__\n' $_E34
for s in set(structs): sec += f"- `struct {s}`\n"
_E35=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_35_%d__\n' $_E35
for e in set(enums): sec += f"- `enum {e}`\n"
_E36=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_36_%d__\n' $_E36
sec += "\n"
_E37=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_37_%d__\n' $_E37
if traits:
_E38=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_38_%d__\n' $_E38
sec += f"**Traits**:\n"
_E39=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_39_%d__\n' $_E39
for t in set(traits): sec += f"- `trait {t}`\n"
_E40=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_40_%d__\n' $_E40
sec += "\n"
_E41=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_41_%d__\n' $_E41
if funcs:
_E42=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_42_%d__\n' $_E42
sec += f"**Public Functions**:\n"
_E43=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_43_%d__\n' $_E43
for f in set(funcs): sec += f"- `fn {f}()`\n"
_E44=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_44_%d__\n' $_E44
sec += "\n"
_E45=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_45_%d__\n' $_E45
return sec
_E46=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_46_%d__\n' $_E46
append_content = ""
_E47=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_47_%d__\n' $_E47
for f in missing:
_E48=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_48_%d__\n' $_E48
if os.path.exists(f"src/{f}"):
_E49=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_49_%d__\n' $_E49
append_content += extract_symbols(f)
_E50=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_50_%d__\n' $_E50
with open("docs/architecture.md", "a") as doc:
_E51=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_51_%d__\n' $_E51
doc.write("\n## 补充模块\n")
_E52=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_52_%d__\n' $_E52
doc.write(append_content)
_E53=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_53_%d__\n' $_E53
print("done")
_E54=$?
printf '\n__MYCLAW_CHK_4464c997bbbe4fe1953c58948d523d83_54_%d__\n' $_E54
