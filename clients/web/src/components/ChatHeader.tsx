import { useEffect, useState, useCallback, useRef } from 'react'
import { ChevronDown, Plus, Check, Cpu, Layers, Loader2, Download } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'

interface Session { id: string; name: string; is_active?: boolean }
interface ModelInfo { id: string; active: boolean; supports_image: boolean }

function useOutsideClose(onClose: () => void) {
  const ref = useRef<HTMLDivElement>(null)
  useEffect(() => {
    const h = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose()
    }
    document.addEventListener('mousedown', h)
    return () => document.removeEventListener('mousedown', h)
  }, [onClose])
  return ref
}

export default function ChatHeader() {
  const { status, messages, request, reloadHistory, setActiveSessionId } = useWebSocketContext()
  const [sessions, setSessions] = useState<Session[]>([])
  const [models, setModels] = useState<ModelInfo[]>([])
  const [activeModel, setActiveModel] = useState<string | null>(null)
  const [sessOpen, setSessOpen] = useState(false)
  const [modelOpen, setModelOpen] = useState(false)
  const [busy, setBusy] = useState(false)

  const sessRef = useOutsideClose(useCallback(() => setSessOpen(false), []))
  const modelRef = useOutsideClose(useCallback(() => setModelOpen(false), []))

  const refresh = useCallback(async () => {
    if (status !== 'connected') return
    try {
      const [s, m] = await Promise.all([
        request('sessions.list') as Promise<Session[]>,
        request('models.list') as Promise<{ models: ModelInfo[]; active: string | null }>,
      ])
      setSessions(s || [])
      setModels(m?.models || [])
      setActiveModel(m?.active ?? null)
      // Sync active session ID
      const active = (s || []).find((sess) => sess.is_active)
      if (active) setActiveSessionId(active.id)
    } catch { /* ignore */ }
  }, [status, request, setActiveSessionId])

  useEffect(() => { if (status === 'connected') refresh() }, [status, refresh])

  const activeSession = sessions.find((s) => s.is_active)

  const switchSession = async (id: string) => {
    setSessOpen(false)
    if (busy) return
    setBusy(true)
    try {
      await request('sessions.switch', { id })
      setActiveSessionId(id)
      await reloadHistory()
      await refresh()
    } finally { setBusy(false) }
  }

  const newSession = async () => {
    setSessOpen(false)
    setBusy(true)
    try {
      const res = await request('sessions.create', { name: `Chat ${new Date().toLocaleString()}` }) as { id: string } | undefined
      if (res?.id) setActiveSessionId(res.id)
      await reloadHistory()
      await refresh()
    } finally { setBusy(false) }
  }

  const pickModel = async (id: string) => {
    setModelOpen(false)
    setBusy(true)
    try {
      await request('models.set', { model: id })
      await refresh()
    } finally { setBusy(false) }
  }

  const exportChat = () => {
    const lines: string[] = [`# ${activeSession?.name ?? 'Chat Export'}`, '', `*Exported: ${new Date().toLocaleString()}*`, '']
    for (const msg of messages) {
      if (msg.role === 'user') {
        lines.push('## 👤 User', '')
        const content = msg.content.replace(/<system-reminder>\s*([\s\S]*?)\s*<\/system-reminder>/g, (_m, body) => `<details><summary>system-reminder</summary>\n\n${String(body).trim()}\n\n</details>`)
        lines.push(content, '')
        if (msg.images?.length) lines.push(...msg.images.map((f) => `- Image: ${f.name ?? f.path} (${f.mime ?? 'image'})`), '')
        if (msg.files?.length) lines.push(...msg.files.map((f) => `- File: ${f.name ?? f.path} (${f.mime ?? 'file'})`), '')
      } else {
        lines.push('## 🦀 Assistant', '')
        for (const block of msg.blocks) {
          if (block.type === 'content') lines.push(block.text, '')
          else if (block.type === 'thinking') lines.push('<details><summary>Thinking</summary>', '', block.text, '', '</details>', '')
          else if (block.type === 'tool_call') {
            lines.push('<details><summary>🔧 Tool: `' + block.name + '`' + (block.error ? ' (error)' : '') + '</summary>', '')
            lines.push('**Input**', '', '```json', JSON.stringify(block.args, null, 2), '```', '')
            if (block.output !== undefined) lines.push('**Output**', '', '```text', block.output, '```', '')
            lines.push('</details>', '')
          }
        }
      }
      lines.push('---', '')
    }
    const blob = new Blob([lines.join('\n')], { type: 'text/markdown' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `${(activeSession?.name ?? 'chat').replace(/[^a-zA-Z0-9_-]/g, '_')}.md`
    document.body.appendChild(a); a.click(); document.body.removeChild(a)
    URL.revokeObjectURL(url)
  }

  const btn = 'flex items-center gap-1.5 px-2 py-1.5 rounded-lg text-xs text-zinc-300 hover:bg-zinc-800 border border-zinc-800 transition-colors disabled:opacity-50 min-w-0'
  const menu = 'absolute z-20 mt-1 w-56 sm:w-64 max-h-80 overflow-y-auto rounded-xl border border-zinc-800 bg-zinc-900 shadow-2xl py-1 right-0 sm:left-0'
  const item = 'w-full flex items-center gap-2 px-3 py-2 text-xs text-zinc-300 hover:bg-zinc-800 text-left transition-colors'

  return (
    <header className="border-b border-zinc-800 px-2 sm:px-4 h-10 sm:h-12 flex items-center gap-1.5 sm:gap-2 shrink-0">
      {/* Session selector */}
      <div className="relative min-w-0 flex-1 sm:flex-none" ref={sessRef}>
        <button
          className={btn + ' w-full sm:w-auto'}
          disabled={status !== 'connected' || busy}
          onClick={() => { setSessOpen((v) => !v); setModelOpen(false) }}
        >
          <Layers size={13} className="text-zinc-500 shrink-0" />
          <span className="truncate">{activeSession?.name ?? 'No session'}</span>
          <ChevronDown size={12} className="text-zinc-600 shrink-0" />
        </button>
        {sessOpen && (
          <div className={menu}>
            <button className={item + ' text-blue-400'} onClick={newSession}>
              <Plus size={13} /> New session
            </button>
            <div className="my-1 border-t border-zinc-800" />
            {sessions.length === 0 && (
              <div className="px-3 py-2 text-xs text-zinc-600">No sessions</div>
            )}
            {sessions.map((s) => (
              <button key={s.id} className={item} onClick={() => switchSession(s.id)}>
                {s.is_active ? <Check size={13} className="text-emerald-400 shrink-0" /> : <span className="w-[13px] shrink-0" />}
                <span className="truncate">{s.name}</span>
              </button>
            ))}
          </div>
        )}
      </div>

      {/* Model selector */}
      <div className="relative min-w-0" ref={modelRef}>
        <button
          className={btn + ' w-full sm:w-auto'}
          disabled={status !== 'connected' || busy}
          onClick={() => { setModelOpen((v) => !v); setSessOpen(false) }}
        >
          <Cpu size={13} className="text-zinc-500 shrink-0" />
          <span className="truncate font-mono">{activeModel ?? 'default'}</span>
          <ChevronDown size={12} className="text-zinc-600 shrink-0" />
        </button>
        {modelOpen && (
          <div className={menu}>
            {models.length === 0 && (
              <div className="px-3 py-2 text-xs text-zinc-600">No models available</div>
            )}
            {models.map((m) => (
              <button key={m.id} className={item} onClick={() => pickModel(m.id)}>
                {m.active ? <Check size={13} className="text-emerald-400 shrink-0" /> : <span className="w-[13px] shrink-0" />}
                <span className="truncate font-mono">{m.id}</span>
                {m.supports_image && <span className="ml-auto text-[10px] text-zinc-600 shrink-0">vision</span>}
              </button>
            ))}
            {models.length > 0 && !models.some(m => m.id === activeModel) && activeModel && (
              <div className="px-3 py-1.5 text-[10px] text-zinc-600 border-t border-zinc-800 mt-1">
                Active: <span className="font-mono text-zinc-500">{activeModel}</span> (not in fallback chain)
              </div>
            )}
          </div>
        )}
      </div>

      {busy && <Loader2 size={13} className="animate-spin text-zinc-600" />}

      {/* Export button */}
      <button
        onClick={exportChat}
        disabled={status !== 'connected' || messages.length === 0}
        className={btn + ' ml-auto'}
        title="Export chat as Markdown"
      >
        <Download size={13} />
      </button>
    </header>
  )
}
