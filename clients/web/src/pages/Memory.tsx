import { useEffect, useState, useCallback } from 'react'
import { Brain, FileText } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'

interface MemoryFile {
  name: string
  size: number
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

export default function Memory() {
  const { status, sendRaw, addMessageListener } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [files, setFiles] = useState<MemoryFile[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const fetchFiles = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    setError(null)
    try {
      const result = (await request('memory.list')) as MemoryFile[]
      setFiles(result || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request])

  useEffect(() => {
    if (status === 'connected') fetchFiles()
  }, [status, fetchFiles])

  return (
    <>
      <header className="border-b border-zinc-700/50 px-6 py-3 flex items-center justify-between shrink-0">
        <h2 className="text-sm font-semibold text-zinc-300 flex items-center gap-2">
          <Brain size={16} />
          Memory
        </h2>
        <span className="text-xs text-zinc-500">{files.length} file{files.length !== 1 ? 's' : ''}</span>
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
          <div className="text-sm text-zinc-500 animate-pulse">Loading memory files…</div>
        )}

        {!loading && files.length === 0 && status === 'connected' && (
          <div className="text-sm text-zinc-500">No memory files found in workspace/memory/.</div>
        )}

        {!loading && files.length > 0 && (
          <div className="space-y-2">
            {files.map((file) => (
              <div
                key={file.name}
                className="flex items-center gap-3 rounded-lg border border-zinc-700/40 bg-zinc-800/50 px-4 py-3"
              >
                <FileText size={14} className="text-blue-400 shrink-0" />
                <span className="flex-1 font-mono text-sm text-zinc-200 truncate">{file.name}</span>
                <span className="text-xs text-zinc-500 shrink-0">{formatBytes(file.size)}</span>
              </div>
            ))}
          </div>
        )}
      </div>
    </>
  )
}
