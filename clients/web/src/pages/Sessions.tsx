import { useEffect, useState, useCallback } from 'react'
import { Layers, Plus, ArrowRightLeft, Trash2, CheckCircle } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'

interface Session {
  id: string
  name: string
  created_at?: string
  is_active?: boolean
}

export default function Sessions() {
  const { status, sendRaw, addMessageListener, setMessages } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [sessions, setSessions] = useState<Session[]>([])
  const [newName, setNewName] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const fetchSessions = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    setError(null)
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
    if (!newName.trim() || status !== 'connected') return
    setError(null)
    try {
      await request('sessions.create', { name: newName.trim() })
      setNewName('')
      await fetchSessions()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    }
  }, [newName, status, request, fetchSessions])

  const handleSwitch = useCallback(
    async (id: string) => {
      if (status !== 'connected') return
      setError(null)
      try {
        await request('sessions.switch', { id })
        // Clear chat messages since we've switched to a different session context.
        setMessages([])
        // Refresh list to update is_active highlights.
        await fetchSessions()
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err))
      }
    },
    [status, request, setMessages, fetchSessions],
  )

  const handleDelete = useCallback(
    async (id: string) => {
      if (status !== 'connected') return
      setError(null)
      try {
        await request('sessions.delete', { id })
        await fetchSessions()
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err))
      }
    },
    [status, request, fetchSessions],
  )

  useEffect(() => {
    if (status === 'connected') {
      fetchSessions()
    }
  }, [status, fetchSessions])

  return (
    <>
      {/* Header */}
      <header className="border-b border-zinc-700/50 px-6 py-3 flex items-center justify-between shrink-0">
        <h2 className="text-sm font-semibold text-zinc-300 flex items-center gap-2">
          <Layers size={16} />
          Sessions
        </h2>
        <button
          onClick={fetchSessions}
          disabled={status !== 'connected' || loading}
          className="text-xs text-zinc-500 hover:text-zinc-300 disabled:opacity-40 transition"
        >
          Refresh
        </button>
      </header>

      <div className="flex-1 overflow-y-auto px-6 py-6 max-w-3xl w-full mx-auto space-y-6">
        {/* Create session */}
        <div className="flex gap-2">
          <input
            type="text"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && handleCreate()}
            placeholder="New session name…"
            disabled={status !== 'connected'}
            className="flex-1 rounded-lg border border-zinc-700/50 bg-zinc-800 px-4 py-2.5 text-sm text-zinc-100 placeholder-zinc-500 outline-none focus:border-zinc-600 focus:ring-1 focus:ring-zinc-600 disabled:opacity-50 transition"
          />
          <button
            onClick={handleCreate}
            disabled={status !== 'connected' || !newName.trim()}
            className="flex items-center gap-1.5 rounded-lg bg-blue-600 hover:bg-blue-500 disabled:opacity-40 disabled:cursor-not-allowed px-4 py-2.5 text-sm font-medium transition"
          >
            <Plus size={14} />
            Create
          </button>
        </div>

        {/* Error */}
        {error && (
          <div className="rounded-lg bg-red-900/30 border border-red-700/40 px-4 py-3 text-sm text-red-300">
            {error}
          </div>
        )}

        {/* Loading */}
        {loading && (
          <div className="text-sm text-zinc-500 animate-pulse">Loading sessions…</div>
        )}

        {/* Session list */}
        {!loading && sessions.length === 0 && (
          <div className="text-sm text-zinc-500">No sessions yet. Create one above.</div>
        )}

        <div className="space-y-2">
          {sessions.map((session) => (
            <div
              key={session.id}
              className={`flex items-center gap-3 rounded-lg border px-4 py-3 transition group ${
                session.is_active
                  ? 'border-blue-500/50 bg-blue-900/20'
                  : 'border-zinc-700/40 bg-zinc-800/50 hover:bg-zinc-800'
              }`}
            >
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium text-zinc-200 truncate">{session.name}</span>
                  {session.is_active && (
                    <span className="flex items-center gap-1 text-xs text-blue-400 shrink-0">
                      <CheckCircle size={12} />
                      active
                    </span>
                  )}
                </div>
                <div className="text-xs text-zinc-500 font-mono">{session.id}</div>
                {session.created_at && (
                  <div className="text-xs text-zinc-600">
                    {new Date(session.created_at).toLocaleString()}
                  </div>
                )}
              </div>
              {!session.is_active && (
                <button
                  onClick={() => handleSwitch(session.id)}
                  disabled={status !== 'connected'}
                  className="flex items-center gap-1 rounded-md bg-zinc-700/60 hover:bg-zinc-600 px-3 py-1.5 text-xs text-zinc-300 transition opacity-0 group-hover:opacity-100"
                  title="Switch to session"
                >
                  <ArrowRightLeft size={12} />
                  Switch
                </button>
              )}
              <button
                onClick={() => handleDelete(session.id)}
                disabled={status !== 'connected'}
                className="flex items-center gap-1 rounded-md bg-red-900/40 hover:bg-red-800/60 px-3 py-1.5 text-xs text-red-300 transition opacity-0 group-hover:opacity-100"
                title="Delete session"
              >
                <Trash2 size={12} />
              </button>
            </div>
          ))}
        </div>
      </div>
    </>
  )
}
