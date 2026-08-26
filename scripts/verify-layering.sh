#!/bin/bash
# 验证分层合规性（docs/architecture-restructure-plan.md §1.2），退出码 0=合规，非 0=违规
set -u
violations=0

# L0 契约层：零外部 crate 引用（api 内部互引豁免）。
# #151 Phase 3c 起匹配文件内任意位置的 crate:: 路径（含函数体内全限定引用），
# 不再只看 use 行——3b 曾从该盲区漏掉 11 处 api→channels 全限定引用。
l0=$(grep -rn "crate::" src/api/ 2>/dev/null | grep -v "crate::api::" || true)
if [ -n "$l0" ]; then
  echo "❌ L0 api 层违规引用："; echo "$l0"; violations=$((violations + 1))
fi

# L1 基础层：仅引 L0 + 基础内部
for mod in ids config str_utils scheduling_types; do
  out=$(grep -rn "crate::\(providers\|memory\|identity\|tools\|agents\|scheduling\|commands\|channels\|daemon\|cli\|webui\)" src/$mod/ 2>/dev/null || true)
  if [ -n "$out" ]; then
    echo "❌ L1 $mod 违规引用："; echo "$out"; violations=$((violations + 1))
  fi
done

# L2 服务层：仅引 L0 + L1
for mod in providers memory identity; do
  out=$(grep -rn "crate::\(tools\|agents\|scheduling\|commands\|channels\|daemon\|cli\|webui\)" src/$mod/ 2>/dev/null || true)
  if [ -n "$out" ]; then
    echo "❌ L2 $mod 违规引用："; echo "$out"; violations=$((violations + 1))
  fi
done

# L2 mcp：不引 L4/L5/L6
out=$(grep -rn "crate::\(agents\|scheduling\|commands\|channels\|daemon\|cli\|webui\)" src/mcp/ 2>/dev/null || true)
if [ -n "$out" ]; then
  echo "❌ L2 mcp 违规引用 L4+："; echo "$out"; violations=$((violations + 1))
fi

# L3 工具层：不引 L4/L5
out=$(grep -rn "crate::\(agents\|scheduling_runtime\|commands\|channels\)" src/tools/ 2>/dev/null || true)
if [ -n "$out" ]; then
  echo "❌ L3 tools 违规引用 L4/L5："; echo "$out"; violations=$((violations + 1))
fi

# L4 运行时层：不引 L5/L6
for mod in agents scheduling_runtime commands; do
  out=$(grep -rn "crate::\(channels\|daemon\|cli\|webui\)" src/$mod/ 2>/dev/null || true)
  if [ -n "$out" ]; then
    echo "❌ L4 $mod 违规引用 L5/L6："; echo "$out"; violations=$((violations + 1))
  fi
done

# L5 渠道层：不引 L6
out=$(grep -rn "crate::\(daemon\|cli\|webui\)" src/channels/ 2>/dev/null || true)
if [ -n "$out" ]; then
  echo "❌ L5 channels 违规引用 L6："; echo "$out"; violations=$((violations + 1))
fi

if [ $violations -eq 0 ]; then
  echo "✅ 分层合规"
  exit 0
else
  echo "❌ $violations 处违规"
  exit 1
fi
