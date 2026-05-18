import { useEffect, useState, useCallback, useRef } from 'react'
import { Layers, Plus, Pencil, Trash2, Check, X, Loader2 } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'

interface Session {
  id: string
  name: string
  created_at?: string
  is_active?: boolean
}

function SessionRow({
  session,
  onSwitch,
  onRename,
  onDelete,
  disabled,
}: {
  session: Session
  onSwitch: (id: string) => Promise<void>
  onRename: (id: string, name: string) => Promise<void>
  onDelete: (id: string) => Promise<void>
  disabled: boolean
}) {
  const [editing, setEditing] = useState(false)
  const [draftName, setDraftName] = useState(session.name)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [busy, setBusy] = useState(false)
  const inputRef = useRef<HTMLInputElement>(null)

  const startEdit = (e: React.MouseEvent) => {
    e.stopPropagation()
    setDraftName(session.name)
    setEditing(true)
    setTimeout(() => inputRef.current?.select(), 0)
  }

  const commitRename = async () => {
    const trimmed = draftName.trim()
    if (!trimmed || trimmed === session.name) { setEditing(false); return }
    setBusy(true)
    try {
      await onRename(session.id, trimmed)
    } finally {
      setBusy(false)
      setEditing(false)
    }
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
          ? 'border-blue-500/40 bg-blue-950/30 cursor-default'
          : 'border-zinc-800 bg-zinc-900/50 hover:bg-zinc-800/60 hover:border-zinc-700 cursor-pointer'
      }`}
    >
      {/* Active dot */}
      <div className={`h-2 w-2 rounded-full shrink-0 ${session.is_active ? 'bg-blue-400' : 'bg-zinc-700'}`} />

      {/* Name / editor */}
      <div className="flex-1 min-w-0">
        {editing ? (
          <input
            ref={inputRef}
            value={draftName}
            onChange={(e) => setDraftName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') commitRename()
              if (e.key === 'Escape') setEditing(false)
            }}
            onClick={(e) => e.stopPropagation()}
            className="w-full bg-zinc-800 border border-zinc-600 rounded-lg px-2 py-1 text-sm text-zinc-100 outline-none focus:border-zinc-500"
            autoFocus
          />
        ) : (
          <div>
            <span className={`text-sm font-medium truncate block ${session.is_active ? 'text-zinc-100' : 'text-zinc-300'}`}>
              {session.name}
            </span>
            {session.created_at && (
              <span className="text-xs text-zinc-600">
                {new Date(session.created_at).toLocaleString()}
              </span>
            )}
          </div>
        )}
      </div>

      {/* Actions */}
      <div
        className="flex items-center gap-1 shrink-0"
        onClick={(e) => e.stopPropagation()}
      >
        {editing ? (
          <>
            <button
              onClick={commitRename}
              disabled={busy}
              className="p-1.5 rounded-lg text-emerald-400 hover:bg-zinc-700 transition-colors"
              title="Save"
            >
              {busy ? <Loader2 size={13} className="animate-spin" /> : <Check size={13} />}
            </button>
            <button
              onClick={() => setEditing(false)}
              className="p-1.5 rounded-lg text-zinc-500 hover:bg-zinc-700 transition-colors"
              title="Cancel"
            >
              <X size={13} />
            </button>
          </>
        ) : confirmDelete ? (
          <>
            <span className="text-xs text-red-400 mr-1">Delete?</span>
            <button
              onClick={handleDelete}
              disabled={busy}
              className="px-2 py-1 rounded-lg bg-red-600/80 hover:bg-red-600 text-white text-xs transition-colors"
            >
              {busy ? <Loader2 size={11} className="animate-spin" /> : 'Yes'}
            </button>
            <button
              onClick={(e) => { e.stopPropagation(); setConfirmDelete(false) }}
              className="px-2 py-1 rounded-lg bg-zinc-700 hover:bg-zinc-600 text-zinc-300 text-xs transition-colors"
            >
              No
            </button>
          </>
        ) : (
          <>
            <button
              onClick={startEdit}
              disabled={disabled}
              className="p-1.5 rounded-lg text-zinc-600 hover:text-zinc-300 hover:bg-zinc-700 opacity-0 group-hover:opacity-100 transition-all disabled:pointer-events-none"
              title="Rename"
            >
              <Pencil size={13} />
            </button>
            <button
              onClick={handleDelete}
              disabled={disabled}
              className="p-1.5 rounded-lg text-zinc-600 hover:text-red-400 hover:bg-zinc-700 opacity-0 group-hover:opacity-100 transition-all disabled:pointer-events-none"
              title="Delete"
            >
              <Trash2 size={13} />
            </button>
          </>
        )}
      </div>
    </div>
  )
}

export default function Sessions() {
  const { status, sendRaw, addMessageListener, setMessages } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [sessions, setSessions] = useState<Session[]>([])
  const [newName, setNewName] = useState('')
  const [loading, setLoading] = useState(false)
  const [creating, setCreating] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const fetchSessions = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    try {
      const result = (await request('sessions.list')) as Session[]
      setSessions(result || [])
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
      setNewName('')
      await fetchSessions()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setCreating(false)
    }
  }, [newName, status, creating, request, fetchSessions])

  const handleSwitch = useCallback(async (id: string) => {
    setError(null)
    try {
      await request('sessions.switch', { id })
      setMessages([])
      await fetchSessions()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    }
  }, [request, setMessages, fetchSessions])

  const handleRename = useCallback(async (id: string, name: string) => {
    setError(null)
    try {
      await request('sessions.rename', { id, name })
      await fetchSessions()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    }
  }, [request, fetchSessions])

  const handleDelete = useCallback(async (id: string) => {
    setError(null)
    try {
      await request('sessions.delete', { id })
      await fetchSessions()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    }
  }, [request, fetchSessions])

  useEffect(() => {
    if (status === 'connected') fetchSessions()
  }, [status, fetchSessions])

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <header className="border-b border-zinc-800 px-6 py-3 flex items-center justify-between shrink-0">
        <h2 className="text-sm font-semibold text-zinc-300 flex items-center gap-2">
          <Layers size={15} />
          Sessions
        </h2>
        <span className="text-xs text-zinc-600">{sessions.length} session{sessions.length !== 1 ? 's' : ''}</span>
      </header>

      <div className="flex-1 overflow-y-auto">
        <div className="max-w-2xl mx-auto px-6 py-6 space-y-4">

          {/* Create */}
          <div className="flex gap-2">
            <input
              type="text"
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && handleCreate()}
              placeholder="New session name…"
              disabled={status !== 'connected'}
              className="flex-1 rounded-xl border border-zinc-800 bg-zinc-900 px-4 py-2.5 text-sm text-zinc-100 placeholder-zinc-600 outline-none focus:border-zinc-600 disabled:opacity-50 transition-colors"
            />
            <button
              onClick={handleCreate}
              disabled={status !== 'connected' || !newName.trim() || creating}
              className="flex items-center gap-1.5 rounded-xl bg-blue-600 hover:bg-blue-500 disabled:opacity-40 disabled:cursor-not-allowed px-4 py-2.5 text-sm font-medium transition-colors"
            >
              {creating ? <Loader2 size={14} className="animate-spin" /> : <Plus size={14} />}
              New
            </button>
          </div>

          {/* Error */}
          {error && (
            <div className="rounded-xl bg-red-900/30 border border-red-700/40 px-4 py-3 text-sm text-red-300">
              {error}
            </div>
          )}

          {/* List */}
          {loading ? (
            <div className="flex items-center gap-2 text-sm text-zinc-500 py-2">
              <Loader2 size={14} className="animate-spin" />
              Loading…
            </div>
          ) : sessions.length === 0 && status === 'connected' ? (
            <p className="text-sm text-zinc-500 py-2">No sessions yet. Create one above.</p>
          ) : (
            <div className="space-y-1.5">
              {sessions.map((session) => (
                <SessionRow
                  key={session.id}
                  session={session}
                  onSwitch={handleSwitch}
                  onRename={handleRename}
                  onDelete={handleDelete}
                  disabled={status !== 'connected'}
                />
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
