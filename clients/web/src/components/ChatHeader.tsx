import { useEffect, useState, useCallback, useRef } from 'react'
import { ChevronDown, Plus, Check, Cpu, Layers, Loader2 } from 'lucide-react'
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
  const { status, request, reloadHistory } = useWebSocketContext()
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
    } catch { /* ignore */ }
  }, [status, request])

  useEffect(() => { if (status === 'connected') refresh() }, [status, refresh])

  const activeSession = sessions.find((s) => s.is_active)

  const switchSession = async (id: string) => {
    setSessOpen(false)
    if (busy) return
    setBusy(true)
    try {
      await request('sessions.switch', { id })
      await reloadHistory()
      await refresh()
    } finally { setBusy(false) }
  }

  const newSession = async () => {
    setSessOpen(false)
    setBusy(true)
    try {
      await request('sessions.create', { name: `Chat ${new Date().toLocaleString()}` })
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

  const btn = 'flex items-center gap-1.5 px-2.5 py-1.5 rounded-lg text-xs text-zinc-300 hover:bg-zinc-800 border border-zinc-800 transition-colors disabled:opacity-50'
  const menu = 'absolute z-20 mt-1 w-64 max-h-80 overflow-y-auto rounded-xl border border-zinc-800 bg-zinc-900 shadow-2xl py-1'
  const item = 'w-full flex items-center gap-2 px-3 py-2 text-xs text-zinc-300 hover:bg-zinc-800 text-left transition-colors'

  return (
    <header className="border-b border-zinc-800 px-4 h-12 flex items-center gap-2 shrink-0">
      {/* Session selector */}
      <div className="relative" ref={sessRef}>
        <button
          className={btn}
          disabled={status !== 'connected' || busy}
          onClick={() => { setSessOpen((v) => !v); setModelOpen(false) }}
        >
          <Layers size={13} className="text-zinc-500" />
          <span className="max-w-[180px] truncate">{activeSession?.name ?? 'No session'}</span>
          <ChevronDown size={12} className="text-zinc-600" />
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
      <div className="relative" ref={modelRef}>
        <button
          className={btn}
          disabled={status !== 'connected' || busy}
          onClick={() => { setModelOpen((v) => !v); setSessOpen(false) }}
        >
          <Cpu size={13} className="text-zinc-500" />
          <span className="max-w-[200px] truncate font-mono">{activeModel ?? 'default'}</span>
          <ChevronDown size={12} className="text-zinc-600" />
        </button>
        {modelOpen && (
          <div className={menu}>
            {models.length === 0 && (
              <div className="px-3 py-2 text-xs text-zinc-600">No models</div>
            )}
            {models.map((m) => (
              <button key={m.id} className={item} onClick={() => pickModel(m.id)}>
                {m.active ? <Check size={13} className="text-emerald-400 shrink-0" /> : <span className="w-[13px] shrink-0" />}
                <span className="truncate font-mono">{m.id}</span>
                {m.supports_image && <span className="ml-auto text-[10px] text-zinc-600 shrink-0">vision</span>}
              </button>
            ))}
          </div>
        )}
      </div>

      {busy && <Loader2 size={13} className="animate-spin text-zinc-600" />}
    </header>
  )
}
