import { useEffect, useState, useCallback } from 'react'
import { Wrench } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'

interface Tool {
  name: string
}

export default function Tools() {
  const { status, sendRaw, addMessageListener } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [tools, setTools] = useState<Tool[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const fetchTools = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    setError(null)
    try {
      const result = (await request('tools.list')) as Tool[]
      setTools(result || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request])

  useEffect(() => {
    if (status === 'connected') fetchTools()
  }, [status, fetchTools])

  return (
    <>
      <header className="border-b border-zinc-700/50 px-6 py-3 flex items-center justify-between shrink-0">
        <h2 className="text-sm font-semibold text-zinc-300 flex items-center gap-2">
          <Wrench size={16} />
          Tools
        </h2>
        <span className="text-xs text-zinc-500">{tools.length} tool{tools.length !== 1 ? 's' : ''}</span>
      </header>

      <div className="flex-1 overflow-y-auto px-6 py-6 max-w-3xl w-full mx-auto">
        {status !== 'connected' && (
          <div className="text-sm text-zinc-500">Waiting for connection…</div>
        )}

        {error && (
          <div className="rounded-lg bg-red-900/30 border border-red-700/40 px-4 py-3 text-sm text-red-300 mb-4">
            {error}
          </div>
        )}

        {loading && (
          <div className="text-sm text-zinc-500 animate-pulse">Loading tools…</div>
        )}

        {!loading && tools.length === 0 && status === 'connected' && (
          <div className="text-sm text-zinc-500">No tools registered.</div>
        )}

        {!loading && tools.length > 0 && (
          <div className="grid gap-2">
            {tools.map((tool) => (
              <div
                key={tool.name}
                className="flex items-center gap-3 rounded-lg border border-zinc-700/40 bg-zinc-800/50 px-4 py-3"
              >
                <Wrench size={14} className="text-amber-400 shrink-0" />
                <span className="font-mono text-sm text-zinc-200">{tool.name}</span>
              </div>
            ))}
          </div>
        )}
      </div>
    </>
  )
}
