import { useState, useEffect, useMemo } from 'react'
import { ChevronDown, ChevronRight, Zap, Check, AlertCircle, Loader2, Copy, Square } from 'lucide-react'
import type { ToolCallBlock } from '../hooks/useWebSocket'
import { useWebSocketContext } from '../contexts/WebSocketContext'

function formatOutput(raw: string): string {
  if (!raw) return '(empty)'
  try {
    const parsed = JSON.parse(raw)
    return JSON.stringify(parsed, null, 2)
  } catch {
    return raw
  }
}

/** Build a short arg summary like (path=/foo/bar.rs) for collapsed display. */
function argSummary(_name: string, args: Record<string, unknown>): string {
  if (!args || Object.keys(args).length === 0) return ''
  // Show the most relevant arg first depending on tool name.
  const keys = Object.keys(args)
  const priority = ['path', 'file_path', 'command', 'cmd', 'query', 'url', 'pattern', 'expression', 'name']
  const sorted = [...keys].sort((a, b) => {
    const ai = priority.indexOf(a)
    const bi = priority.indexOf(b)
    return (ai === -1 ? 999 : ai) - (bi === -1 ? 999 : bi)
  })
  const parts = sorted.slice(0, 2).map((k) => {
    const v = args[k]
    let s: string
    if (typeof v === 'string') s = v
    else if (typeof v === 'number' || typeof v === 'boolean') s = String(v)
    else s = JSON.stringify(v)
    if (s.length > 60) s = s.slice(0, 57) + '…'
    return `${k}=${s}`
  })
  return parts.join(', ')
}

function fmtElapsed(ms: number): string {
  const sec = Math.max(0, ms) / 1000
  if (sec < 1) return `${Math.round(ms)}ms`
  if (sec < 10) return `${sec.toFixed(1)}s`
  if (sec < 60) return `${Math.round(sec)}s`
  const m = Math.floor(sec / 60)
  const s = Math.round(sec % 60)
  return s ? `${m}m ${s}s` : `${m}m`
}

type Status = 'running' | 'done' | 'error'

function CopyButton({ text, title }: { text: string; title: string }) {
  const [copied, setCopied] = useState(false)
  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch { /* ignore */ }
  }
  return (
    <button onClick={handleCopy} className="p-1 rounded text-zinc-600 hover:text-zinc-300 hover:bg-zinc-800 transition-colors" title={copied ? 'Copied' : title}>
      {copied ? <Check size={11} className="text-emerald-400" /> : <Copy size={11} />}
    </button>
  )
}


const BORDER_COLOR: Record<Status, string> = {
  running: 'border-amber-900/40',
  done: 'border-zinc-800',
  error: 'border-red-900/50',
}

export default function ToolCallCard({ block }: { block: ToolCallBlock }) {
  const [userExpanded, setUserExpanded] = useState<boolean | null>(null)
  const [inputOpen, setInputOpen] = useState(true)
  const [outputOpen, setOutputOpen] = useState(false)
  const { cancel, isGenerating } = useWebSocketContext()
  const running = block.output === undefined && !block.error
  const error = !!block.error
  const status: Status = running ? 'running' : error ? 'error' : 'done'

  // Live elapsed timer while running.
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!running) return
    const id = window.setInterval(() => setNow(() => Date.now()), 500)
    return () => window.clearInterval(id)
  }, [running])

  const elapsed = block.startedAt
    ? fmtElapsed((block.completedAt ?? now) - block.startedAt)
    : null

  // Auto-expand on error; otherwise default-collapsed when done, expanded when running.
  const expanded = userExpanded !== null
    ? userExpanded
    : error || running

  const summary = useMemo(() => argSummary(block.name, block.args), [block.name, block.args])
  const formattedOutput = useMemo(
    () => (block.output !== undefined ? formatOutput(block.output) : undefined),
    [block.output],
  )

  const inputText = useMemo(() => JSON.stringify(block.args, null, 2), [block.args])
  const outputIsLong = (formattedOutput?.length ?? 0) > 2000 || (formattedOutput?.split('\n').length ?? 0) > 40
  const outputExpanded = outputOpen || !outputIsLong
  const hasBody = Object.keys(block.args).length > 0 || formattedOutput !== undefined

  return (
    <div className={`rounded-xl border ${BORDER_COLOR[status]} bg-zinc-900/60 text-[11px] sm:text-xs overflow-hidden`}>
      <button
        onClick={() => hasBody && setUserExpanded(!expanded)}
        className={`w-full flex items-center gap-2 sm:gap-2.5 px-2.5 sm:px-3.5 py-2 sm:py-2.5 text-left transition-colors ${hasBody ? 'hover:bg-zinc-800/50' : 'cursor-default'}`}
      >
        {hasBody ? (
          expanded
            ? <ChevronDown size={11} className="text-zinc-600 shrink-0" />
            : <ChevronRight size={11} className="text-zinc-600 shrink-0" />
        ) : (
          <span className="w-[11px] shrink-0" />
        )}

        {/* Status icon */}
        {status === 'running' ? (
          <Loader2 size={12} className="text-amber-400 animate-spin shrink-0" />
        ) : status === 'error' ? (
          <AlertCircle size={12} className="text-red-400 shrink-0" />
        ) : (
          <Zap size={12} className="text-emerald-400 shrink-0" />
        )}

        {/* Tool name */}
        <span className="font-mono text-zinc-300 shrink-0">{block.name}</span>

        {/* Arg summary */}
        {summary && (
          <span className="font-mono text-zinc-500 truncate min-w-0 flex-1">({summary})</span>
        )}

        {/* Right side: elapsed + status check */}
        {!summary && <span className="flex-1" />}
        {status === 'done' && (
          <Check size={11} className="text-zinc-600 shrink-0" />
        )}
        {elapsed && (
          <span className="font-mono text-zinc-600 tabular-nums shrink-0">{elapsed}</span>
        )}
        {running && isGenerating && (
          <button
            onClick={(e) => { e.stopPropagation(); cancel() }}
            title="Stop generation"
            className="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] text-amber-400 hover:text-red-400 hover:bg-red-950/40 border border-transparent hover:border-red-800/40 transition-colors shrink-0"
          >
            <Square size={9} /> Stop
          </button>
        )}
      </button>

      {expanded && hasBody && (
        <div className="border-t border-zinc-800">
          {Object.keys(block.args).length > 0 && (
            <div className="px-2.5 sm:px-3.5 py-2 sm:py-2.5 border-b border-zinc-800/60">
              <div className="flex items-center gap-1 mb-1.5 sm:mb-2">
                <button onClick={() => setInputOpen(v => !v)} className="flex items-center gap-1 text-zinc-600 uppercase tracking-widest hover:text-zinc-400 transition-colors" style={{ fontSize: 9 }}>
                  {inputOpen ? <ChevronDown size={10} /> : <ChevronRight size={10} />} Input
                </button>
                <div className="flex-1" />
                <CopyButton text={inputText} title="Copy input" />
              </div>
              {inputOpen && (
                <pre className="tool-call-pre text-zinc-400 whitespace-pre-wrap break-all font-mono text-[10px] sm:text-xs leading-4 sm:leading-5">
                  {inputText}
                </pre>
              )}
            </div>
          )}
          {formattedOutput !== undefined && (
            <div className="px-2.5 sm:px-3.5 py-2 sm:py-2.5">
              <div className="flex items-center gap-1 mb-1.5 sm:mb-2">
                <button onClick={() => setOutputOpen(v => !v)} className="flex items-center gap-1 text-zinc-600 uppercase tracking-widest hover:text-zinc-400 transition-colors" style={{ fontSize: 9 }}>
                  {outputOpen ? <ChevronDown size={10} /> : <ChevronRight size={10} />} Output{outputIsLong ? ' · long' : ''}
                </button>
                <div className="flex-1" />
                <CopyButton text={formattedOutput} title="Copy output" />
              </div>
              <pre className={`tool-call-pre whitespace-pre-wrap break-all font-mono text-[10px] sm:text-xs ${outputExpanded ? 'max-h-40 sm:max-h-52 lg:max-h-64 overflow-y-auto' : 'max-h-28 overflow-hidden'} leading-4 sm:leading-5 ${error ? 'text-red-400' : 'text-zinc-400'}`}>
                {formattedOutput}
              </pre>
              {outputIsLong && !outputOpen && (
                <button onClick={() => setOutputOpen(true)} className="mt-2 text-[10px] text-zinc-500 hover:text-zinc-300">Show full output</button>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  )
}
