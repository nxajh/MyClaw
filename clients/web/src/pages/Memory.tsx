import { useEffect, useState, useCallback } from 'react'
import { Brain, FileText, Loader2, ChevronLeft } from 'lucide-react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'

interface MemoryFile {
  name: string
  size: number
}

interface FileContent {
  name: string
  content: string
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
  const [loadingList, setLoadingList] = useState(false)
  const [selected, setSelected] = useState<FileContent | null>(null)
  const [loadingFile, setLoadingFile] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const fetchFiles = useCallback(async () => {
    if (status !== 'connected') return
    setLoadingList(true)
    setError(null)
    try {
      const result = (await request('memory.list')) as MemoryFile[]
      setFiles(result || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoadingList(false)
    }
  }, [status, request])

  const openFile = useCallback(async (name: string) => {
    if (status !== 'connected') return
    setLoadingFile(true)
    setError(null)
    try {
      const result = (await request('memory.read', { name })) as FileContent
      setSelected(result)
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoadingFile(false)
    }
  }, [status, request])

  useEffect(() => {
    if (status === 'connected') fetchFiles()
  }, [status, fetchFiles])

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <header className="border-b border-zinc-800 px-6 py-3 flex items-center justify-between shrink-0">
        <h2 className="text-sm font-semibold text-zinc-300 flex items-center gap-2">
          {selected ? (
            <button
              onClick={() => setSelected(null)}
              className="flex items-center gap-1.5 text-zinc-400 hover:text-zinc-200 transition-colors"
            >
              <ChevronLeft size={15} />
              <span>Memory</span>
            </button>
          ) : (
            <>
              <Brain size={15} />
              <span>Memory</span>
            </>
          )}
        </h2>
        {selected && (
          <span className="text-xs text-zinc-500 font-mono truncate max-w-xs">{selected.name}</span>
        )}
      </header>

      {error && (
        <div className="mx-6 mt-4 rounded-lg bg-red-900/30 border border-red-700/40 px-4 py-3 text-sm text-red-300 shrink-0">
          {error}
        </div>
      )}

      {/* File viewer */}
      {selected ? (
        <div className="flex-1 overflow-y-auto">
          {loadingFile ? (
            <div className="flex items-center justify-center h-32 text-zinc-500">
              <Loader2 size={18} className="animate-spin" />
            </div>
          ) : (
            <div className="max-w-3xl mx-auto px-6 py-6
              prose prose-invert prose-sm max-w-none
              prose-p:leading-7 prose-headings:text-zinc-100
              prose-code:text-zinc-200 prose-code:bg-zinc-800 prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none
              prose-pre:bg-zinc-950 prose-pre:border prose-pre:border-zinc-800 prose-pre:rounded-xl prose-pre:text-xs
              prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400
              prose-a:text-blue-400 prose-strong:text-zinc-200
              prose-hr:border-zinc-800">
              <Markdown remarkPlugins={[remarkGfm]}>{selected.content}</Markdown>
            </div>
          )}
        </div>
      ) : (
        /* File list */
        <div className="flex-1 overflow-y-auto">
          <div className="max-w-3xl mx-auto px-6 py-6">
            {status !== 'connected' && (
              <p className="text-sm text-zinc-500">Waiting for connection…</p>
            )}
            {loadingList && (
              <div className="flex items-center gap-2 text-sm text-zinc-500">
                <Loader2 size={14} className="animate-spin" />
                Loading…
              </div>
            )}
            {!loadingList && files.length === 0 && status === 'connected' && (
              <p className="text-sm text-zinc-500">No memory files found in workspace/memory/.</p>
            )}
            {!loadingList && files.length > 0 && (
              <div className="space-y-1">
                {files.map((file) => (
                  <button
                    key={file.name}
                    onClick={() => openFile(file.name)}
                    className="w-full flex items-center gap-3 rounded-xl border border-zinc-800 bg-zinc-900/50 px-4 py-3 text-left hover:bg-zinc-800/70 hover:border-zinc-700 transition-colors group"
                  >
                    <FileText size={15} className="text-blue-400 shrink-0" />
                    <span className="flex-1 text-sm text-zinc-200 font-mono truncate group-hover:text-zinc-100">
                      {file.name}
                    </span>
                    <span className="text-xs text-zinc-500 shrink-0">{formatBytes(file.size)}</span>
                  </button>
                ))}
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  )
}
