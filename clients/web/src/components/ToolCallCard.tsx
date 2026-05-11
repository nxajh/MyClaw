import { useState } from 'react'
import { ChevronDown, ChevronRight, Terminal } from 'lucide-react'
import type { ToolCall } from '../hooks/useWebSocket'

interface Props {
  toolCall: ToolCall
}

export default function ToolCallCard({ toolCall }: Props) {
  const [expanded, setExpanded] = useState(false)

  return (
    <div className="border border-zinc-700/50 rounded-lg overflow-hidden text-sm">
      {/* Header */}
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full flex items-center gap-2 px-3 py-2 bg-zinc-800/70 hover:bg-zinc-700/70 text-left transition"
      >
        <Terminal size={14} className="text-amber-400 shrink-0" />
        <span className="font-mono text-zinc-300 flex-1 truncate">{toolCall.name}</span>
        {toolCall.output === undefined ? (
          <span className="text-xs text-amber-400 animate-pulse">running…</span>
        ) : (
          <span className="text-xs text-emerald-400">done</span>
        )}
        {expanded ? (
          <ChevronDown size={14} className="text-zinc-500" />
        ) : (
          <ChevronRight size={14} className="text-zinc-500" />
        )}
      </button>

      {/* Body (collapsible) */}
      {expanded && (
        <div className="border-t border-zinc-700/50 bg-zinc-900/50">
          {/* Args */}
          {Object.keys(toolCall.args).length > 0 && (
            <div className="px-3 py-2 border-b border-zinc-700/30">
              <div className="text-xs text-zinc-500 mb-1">Arguments</div>
              <pre className="text-xs text-zinc-300 whitespace-pre-wrap break-all font-mono">
                {JSON.stringify(toolCall.args, null, 2)}
              </pre>
            </div>
          )}

          {/* Output */}
          {toolCall.output !== undefined && (
            <div className="px-3 py-2">
              <div className="text-xs text-zinc-500 mb-1">Output</div>
              <pre className="text-xs text-zinc-300 whitespace-pre-wrap break-all font-mono max-h-64 overflow-y-auto">
                {toolCall.output || '(empty)'}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}
