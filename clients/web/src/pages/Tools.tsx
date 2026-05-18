import { useEffect, useState, useCallback } from 'react'
import { Wrench } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'
import { Page, ErrorBanner, LoadingRow, EmptyState } from '../components/PageLayout'

interface Tool { name: string }

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
      setTools((await request('tools.list') as Tool[]) || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request])

  useEffect(() => { if (status === 'connected') fetchTools() }, [status, fetchTools])

  return (
    <Page
      icon={Wrench}
      title="Tools"
      meta={tools.length ? ` · ${tools.length}` : undefined}
    >
      {error && <ErrorBanner message={error} />}
      {loading && <LoadingRow />}
      {!loading && status !== 'connected' && <EmptyState>Waiting for connection…</EmptyState>}
      {!loading && status === 'connected' && tools.length === 0 && (
        <EmptyState>No tools registered.</EmptyState>
      )}
      {!loading && tools.length > 0 && (
        <div className="space-y-1">
          {tools.map((tool) => (
            <div
              key={tool.name}
              className="flex items-center gap-3 rounded-xl border border-zinc-800 bg-zinc-900/50 px-4 py-3"
            >
              <Wrench size={13} className="text-amber-400 shrink-0" />
              <span className="font-mono text-sm text-zinc-300">{tool.name}</span>
            </div>
          ))}
        </div>
      )}
    </Page>
  )
}
