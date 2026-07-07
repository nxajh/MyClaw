import { useEffect, useState, useCallback, useRef, useMemo } from 'react'
import { useNavigate } from 'react-router-dom'
import { Plus, Pencil, Trash2, Check, X, Loader2, Search, Pin } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'
import { useToast } from '../components/Toast'
import { ErrorBanner, LoadingRow, EmptyState, inputCls, btnPrimary } from '../components/PageLayout'

interface Session {
  id: string
  name: string
  created_at?: string
  is_active?: boolean
}

const PINNED_SESSIONS_KEY = 'myclaw_pinned_sessions'

// ── Session row ───────────────────────────────────────────────────────────────

function SessionRow({
  session, disabled,
  onSwitch, onRename, onDelete, onTogglePin, pinned,
}: {
  session: Session
  disabled: boolean
  pinned: boolean
  onSwitch: (id: string) => Promise<void>
  onRename: (id: string, name: string) => Promise<void>
  onDelete: (id: string) => Promise<void>
  onTogglePin: (id: string) => void
}) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(session.name)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [busy, setBusy] = useState(false)
  const inputRef = useRef<HTMLInputElement>(null)

  const startEdit = (e: React.MouseEvent) => {
    e.stopPropagation()
    setDraft(session.name)
    setEditing(true)
    setTimeout(() => inputRef.current?.select(), 0)
  }

  const commitRename = async () => {
    const trimmed = draft.trim()
    if (!trimmed || trimmed === session.name) { setEditing(false); return }
    setBusy(true)
    try { await onRename(session.id, trimmed) } finally { setBusy(false); setEditing(false) }
  }

  const handleSwitch = async () => {
    if (session.is_active || disabled || editing) return
    setBusy(true)
    try { await onSwitch(session.id) } finally { setBusy(false) }
  }

  const handleDelete = async (e: React.MouseEvent) => {
    e.stopPropagation()
    if (!confirmDelete) { setConfirmDelete(true); return }
    setBusy(true)
    try { await onDelete(session.id) } finally { setBusy(false) }
  }

  return (
    <div
      onClick={handleSwitch}
      className={`group flex items-center gap-3 rounded-xl border px-4 py-3 transition-colors ${
        session.is_active
          ? 'border-zinc-700/70 bg-zinc-800/60 cursor-default'
          : 'border-zinc-800 bg-zinc-900/50 hover:bg-zinc-800/60 hover:border-zinc-700 cursor-pointer'
      }`}
    >
      <div className={`h-2 w-2 rounded-full shrink-0 transition-colors ${session.is_active ? 'bg-emerald-700' : 'bg-zinc-700'}`} />

      <div className="flex-1 min-w-0">
        {editing ? (
          <input
            ref={inputRef}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') commitRename(); if (e.key === 'Escape') setEditing(false) }}
            onClick={(e) => e.stopPropagation()}
            className="w-full bg-zinc-800 border border-zinc-700 rounded-lg px-2 py-1 text-sm text-zinc-100 outline-none focus:border-zinc-600"
            autoFocus
          />
        ) : (
          <div>
            <span className={`text-sm font-medium block truncate ${session.is_active ? 'text-zinc-100' : 'text-zinc-300'}`}>
              {session.name}
            </span>
            {session.created_at && (
              <span className="text-xs text-zinc-600">{new Date(session.created_at).toLocaleString()}</span>
            )}
          </div>
        )}
      </div>

      <div className="flex items-center gap-1 shrink-0" onClick={(e) => e.stopPropagation()}>
        {editing ? (
          <>
            <button onClick={commitRename} disabled={busy} className="p-1.5 rounded-lg text-emerald-400 hover:bg-zinc-700 transition-colors" title="Save">
              {busy ? <Loader2 size={13} className="animate-spin" /> : <Check size={13} />}
            </button>
            <button onClick={() => setEditing(false)} className="p-1.5 rounded-lg text-zinc-500 hover:bg-zinc-700 transition-colors" title="Cancel">
              <X size={13} />
            </button>
          </>
        ) : confirmDelete ? (
          <>
            <span className="text-xs text-red-400 mr-1">Delete?</span>
            <button onClick={handleDelete} disabled={busy} className="px-2 py-1 rounded-lg bg-red-600/80 hover:bg-red-600 text-white text-xs transition-colors">
              {busy ? <Loader2 size={11} className="animate-spin" /> : 'Yes'}
            </button>
            <button onClick={(e) => { e.stopPropagation(); setConfirmDelete(false) }} className="px-2 py-1 rounded-lg bg-zinc-700 hover:bg-zinc-600 text-zinc-300 text-xs transition-colors">
              No
            </button>
          </>
        ) : (
          <>
            <button onClick={(e) => { e.stopPropagation(); onTogglePin(session.id) }} className={`p-1.5 rounded-lg ${pinned ? 'text-zinc-200 hover:text-zinc-100 bg-zinc-800/70' : 'text-zinc-600 hover:text-zinc-300'} hover:bg-zinc-700 transition-all`} title={pinned ? 'Unpin' : 'Pin'}>
              <Pin size={13} />
            </button>
            <button onClick={startEdit} disabled={disabled} className="p-1.5 rounded-lg text-zinc-600 hover:text-zinc-300 hover:bg-zinc-700 opacity-0 group-hover:opacity-100 transition-all disabled:pointer-events-none" title="Rename">
              <Pencil size={13} />
            </button>
            <button onClick={handleDelete} disabled={disabled} className="p-1.5 rounded-lg text-zinc-600 hover:text-red-400 hover:bg-zinc-700 opacity-0 group-hover:opacity-100 transition-all disabled:pointer-events-none" title="Delete">
              <Trash2 size={13} />
            </button>
          </>
        )}
      </div>
    </div>
  )
}

// ── Page ──────────────────────────────────────────────────────────────────────

export default function Sessions() {
  const { status, sendRaw, addMessageListener, reloadHistory, setMessages, triggerClearInput } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const { toast } = useToast()
  const navigate = useNavigate()
  const [sessions, setSessions] = useState<Session[]>([])
  const [newName, setNewName] = useState('')
  const [loading, setLoading] = useState(false)
  const [creating, setCreating] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [searchQuery, setSearchQuery] = useState('')
  const [pinnedIds, setPinnedIds] = useState<string[]>(() => {
    try { return JSON.parse(localStorage.getItem(PINNED_SESSIONS_KEY) || '[]') } catch { return [] }
  })

  const fetchSessions = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    try {
      setSessions((await request('sessions.list') as Session[]) || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request])

  const handleCreate = useCallback(async () => {
    if (!newName.trim() || status !== 'connected' || creating) return
    setCreating(true)
    setError(null)
    try {
      await request('sessions.create', { name: newName.trim() })
      toast('Session created', 'success')
      setNewName('')
      await fetchSessions()
      navigate('/')
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      toast('Failed to create session', 'error')
    } finally {
      setCreating(false)
    }
  }, [newName, status, creating, request, fetchSessions])

  const handleSwitch = useCallback(async (id: string) => {
    setError(null)
    try {
      await request('sessions.switch', { id })
      toast('Session switched', 'success')
      triggerClearInput()
      setMessages([])
      await reloadHistory()
      await fetchSessions()
      navigate('/')
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      toast('Failed to switch session', 'error') }
  }, [request, reloadHistory, fetchSessions, navigate, toast, triggerClearInput, setMessages])

  const handleRename = useCallback(async (id: string, name: string) => {
    setError(null)
    try {
      await request('sessions.rename', { id, name })
      toast('Renamed', 'success')
      await fetchSessions()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      toast('Failed to rename', 'error') }
  }, [request, fetchSessions, toast])

  const handleDelete = useCallback(async (id: string) => {
    setError(null)
    try {
      await request('sessions.delete', { id })
      toast('Session deleted', 'success')
      await fetchSessions()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      toast('Failed to delete', 'error') }
  }, [request, fetchSessions, toast])

  useEffect(() => { if (status === 'connected') fetchSessions() }, [status, fetchSessions])
  useEffect(() => { localStorage.setItem(PINNED_SESSIONS_KEY, JSON.stringify(pinnedIds)) }, [pinnedIds])

  const togglePin = useCallback((id: string) => {
    setPinnedIds(prev => prev.includes(id) ? prev.filter(x => x !== id) : [...prev, id])
  }, [])

  const filteredSessions = useMemo(() => {
    const q = searchQuery.toLowerCase().trim()
    const filtered = q ? sessions.filter(s => s.name.toLowerCase().includes(q)) : sessions
    return [...filtered].sort((a, b) => Number(pinnedIds.includes(b.id)) - Number(pinnedIds.includes(a.id)))
  }, [sessions, searchQuery, pinnedIds])

  return (
    <div className="flex flex-col h-full">
      <div className="flex-1 overflow-y-auto">
        <div className="max-w-2xl mx-auto px-3 sm:px-6 py-4 sm:py-6 space-y-4">
      {/* Create */}
      <div className="flex gap-2 pb-4 border-b border-zinc-900">
        <input
          value={newName}
          onChange={(e) => setNewName(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && handleCreate()}
          placeholder="New session name…"
          disabled={status !== 'connected'}
          className={inputCls}
        />
        <button
          onClick={handleCreate}
          disabled={status !== 'connected' || !newName.trim() || creating}
          className={btnPrimary}
        >
          {creating ? <Loader2 size={13} className="animate-spin" /> : <Plus size={13} />}
          New
        </button>
      </div>

      {error && <ErrorBanner message={error} />}
      {loading && <LoadingRow />}
      {!loading && status !== 'connected' && <EmptyState>Waiting for connection…</EmptyState>}
      {!loading && status === 'connected' && sessions.length === 0 && (
        <EmptyState>📝 No sessions yet. Create one above.</EmptyState>
      )}
      {!loading && sessions.length > 0 && (
        <>
          {/* Search filter */}
          <div className="relative">
            <Search size={13} className="absolute left-3 top-1/2 -translate-y-1/2 text-zinc-600 pointer-events-none" />
            <input
              type="text"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Search sessions…"
              className={`${inputCls} pl-9`}
            />
          </div>
          <div className="space-y-1.5">
            {filteredSessions.length === 0 ? (
              <EmptyState>No sessions match “{searchQuery}”</EmptyState>
            ) : filteredSessions.map((s) => (
              <SessionRow
                key={s.id}
                session={s}
                disabled={status !== 'connected'}
                onSwitch={handleSwitch}
                onRename={handleRename}
                onDelete={handleDelete}
                onTogglePin={togglePin}
                pinned={pinnedIds.includes(s.id)}
              />
            ))}
          </div>
        </>
      )}
        </div>
      </div>
    </div>
  )
}
