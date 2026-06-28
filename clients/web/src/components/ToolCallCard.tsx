import { useState, useMemo } from 'react'
import { ChevronDown, ChevronRight, Loader2, Zap } from 'lucide-react'
import type { ToolCallBlock } from '../hooks/useWebSocket'

function formatOutput(raw: string): string {
  if (!raw) return '(empty)'
  // Try to pretty-print JSON output.
  try {
    const parsed = JSON.parse(raw)
    return JSON.stringify(parsed, null, 2)
  } catch {
    return raw
  }
}

export default function ToolCallCard({ block }: { block: ToolCallBlock }) {
  const [userExpanded, setUserExpanded] = useState<boolean | null>(null)
  const running = block.output === undefined
  const expanded = userExpanded !== null ? userExpanded : running

  const formattedOutput = useMemo(
    () => (block.output !== undefined ? formatOutput(block.output) : undefined),
    [block.output],
  )

  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-900/60 text-[11px] sm:text-xs overflow-hidden">
      <button
        onClick={() => setUserExpanded(!expanded)}
        className="w-full flex items-center gap-2 sm:gap-2.5 px-2.5 sm:px-3.5 py-2 sm:py-2.5 hover:bg-zinc-800/50 text-left transition-colors"
      >
        {running ? (
          <Loader2 size={12} className="text-amber-400 animate-spin shrink-0" />
        ) : (
          <Zap size={12} className="text-emerald-400 shrink-0" />
        )}
        <span className="font-mono text-zinc-300 flex-1 truncate">{block.name}</span>
        <span className={`shrink-0 ${running ? 'text-amber-400' : 'text-zinc-600'}`}>
          {running ? 'running…' : 'done'}
        </span>
        {expanded
          ? <ChevronDown size={11} className="text-zinc-600 shrink-0" />
          : <ChevronRight size={11} className="text-zinc-600 shrink-0" />}
      </button>

      {expanded && (
        <div className="border-t border-zinc-800">
          {Object.keys(block.args).length > 0 && (
            <div className="px-2.5 sm:px-3.5 py-2 sm:py-2.5 border-b border-zinc-800/60">
              <div className="text-zinc-600 uppercase tracking-widest mb-1.5 sm:mb-2" style={{ fontSize: 9 }}>
                Input
              </div>
              <pre className="text-zinc-400 whitespace-pre-wrap break-all font-mono text-[10px] sm:text-xs leading-4 sm:leading-5">
                {JSON.stringify(block.args, null, 2)}
              </pre>
            </div>
          )}
          {formattedOutput !== undefined && (
            <div className="px-2.5 sm:px-3.5 py-2 sm:py-2.5">
              <div className="text-zinc-600 uppercase tracking-widest mb-1.5 sm:mb-2" style={{ fontSize: 9 }}>
                Output
              </div>
              <pre className="text-zinc-400 whitespace-pre-wrap break-all font-mono text-[10px] sm:text-xs max-h-40 sm:max-h-52 lg:max-h-64 overflow-y-auto leading-4 sm:leading-5">
                {formattedOutput}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}
