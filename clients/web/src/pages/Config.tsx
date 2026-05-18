import { useEffect, useState, useCallback } from 'react'
import { Settings, FolderOpen, Wrench } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'

interface RuntimeConfig {
  tool_count: number
  workspace_dir: string | null
}

export default function Config() {
  const { status, sendRaw, addMessageListener } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [config, setConfig] = useState<RuntimeConfig | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const fetchConfig = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    setError(null)
    try {
      const result = (await request('config.get')) as RuntimeConfig
      setConfig(result)
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request])

  useEffect(() => {
    if (status === 'connected') fetchConfig()
  }, [status, fetchConfig])

  return (
    <>
      <header className="border-b border-zinc-700/50 px-6 py-3 flex items-center gap-2 shrink-0">
        <h2 className="text-sm font-semibold text-zinc-300 flex items-center gap-2">
          <Settings size={16} />
          Config
        </h2>
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
          <div className="text-sm text-zinc-500 animate-pulse">Loading config…</div>
        )}

        {!loading && config && (
          <div className="space-y-3">
            <div className="rounded-lg border border-zinc-700/40 bg-zinc-800/50 overflow-hidden">
              <div className="px-4 py-2 border-b border-zinc-700/30 text-xs font-medium text-zinc-400 uppercase tracking-wide">
                Runtime
              </div>
              <div className="divide-y divide-zinc-700/30">
                <div className="flex items-center gap-3 px-4 py-3">
                  <Wrench size={14} className="text-amber-400 shrink-0" />
                  <span className="text-sm text-zinc-400 flex-1">Tools registered</span>
                  <span className="text-sm font-mono text-zinc-200">{config.tool_count}</span>
                </div>
                <div className="flex items-center gap-3 px-4 py-3">
                  <FolderOpen size={14} className="text-blue-400 shrink-0" />
                  <span className="text-sm text-zinc-400 flex-1">Workspace directory</span>
                  <span className="text-sm font-mono text-zinc-300 truncate max-w-xs text-right">
                    {config.workspace_dir ?? <span className="text-zinc-600 italic">not set</span>}
                  </span>
                </div>
              </div>
            </div>
          </div>
        )}
      </div>
    </>
  )
}
