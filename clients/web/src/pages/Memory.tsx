import { useEffect, useState, useCallback } from 'react'
import { FileText, Plus, Pencil, Trash2, Check, X, ChevronLeft, Loader2 } from 'lucide-react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useApi } from '../lib/api'
import { ErrorBanner, LoadingRow, EmptyState, inputCls, btnPrimary, btnGhost, btnDanger } from '../components/PageLayout'

interface MemoryFile { name: string; size: number }

function formatBytes(b: number) {
  if (b < 1024) return `${b} B`
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`
  return `${(b / (1024 * 1024)).toFixed(1)} MB`
}

// ── Editor ────────────────────────────────────────────────────────────────────

function FileEditor({
  initial,
  onSave,
  onCancel,
  saving,
}: {
  initial: { name: string; content: string }
  onSave: (name: string, content: string) => void
  onCancel: () => void
  saving: boolean
}) {
  const [name, setName] = useState(initial.name)
  const [content, setContent] = useState(initial.content)
  const isNew = !initial.name

  return (
    <div className="space-y-3">
      {isNew && (
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="filename.md"
          className={inputCls}
          autoFocus
        />
      )}
      <textarea
        value={content}
        onChange={(e) => setContent(e.target.value)}
        rows={16}
        className="w-full rounded-xl border border-zinc-800 bg-zinc-900 px-4 py-3 text-sm font-mono text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-700 resize-y transition-colors"
        placeholder="Write markdown here…"
        autoFocus={!isNew}
      />
      <div className="flex gap-2 justify-end">
        <button onClick={onCancel} className={btnGhost}>
          <X size={13} /> Cancel
        </button>
        <button
          onClick={() => onSave(name, content)}
          disabled={saving || !name.trim() || (!name.endsWith('.md') && isNew)}
          className={btnPrimary}
        >
          {saving ? <Loader2 size={13} className="animate-spin" /> : <Check size={13} />}
          Save
        </button>
      </div>
      {isNew && name && !name.endsWith('.md') && (
        <p className="text-xs text-amber-400">Filename must end with .md</p>
      )}
    </div>
  )
}

// ── Main page ─────────────────────────────────────────────────────────────────

export default function Memory() {
  const { status, sendRaw, addMessageListener } = useWebSocketContext()
  const { request } = useApi(sendRaw, addMessageListener)
  const [files, setFiles] = useState<MemoryFile[]>([])
  const [loadingList, setLoadingList] = useState(false)
  const [error, setError] = useState<string | null>(null)

  type View = { mode: 'list' } | { mode: 'view'; name: string; content: string } | { mode: 'edit'; name: string; content: string } | { mode: 'new' }
  const [view, setView] = useState<View>({ mode: 'list' })
  const [loadingFile, setLoadingFile] = useState(false)
  const [saving, setSaving] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)

  const fetchFiles = useCallback(async () => {
    if (status !== 'connected') return
    setLoadingList(true)
    setError(null)
    try {
      setFiles((await request('memory.list') as MemoryFile[]) || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoadingList(false)
    }
  }, [status, request])

  const openFile = useCallback(async (name: string) => {
    setLoadingFile(true)
    setError(null)
    try {
      const res = await request('memory.read', { name }) as { name: string; content: string }
      setView({ mode: 'view', name: res.name, content: res.content })
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoadingFile(false)
    }
  }, [request])

  const handleSave = useCallback(async (name: string, content: string) => {
    setSaving(true)
    setError(null)
    try {
      await request('memory.write', { name, content })
      await fetchFiles()
      setView({ mode: 'view', name, content })
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setSaving(false)
    }
  }, [request, fetchFiles])

  const handleDelete = useCallback(async (name: string) => {
    setError(null)
    try {
      await request('memory.delete', { name })
      setView({ mode: 'list' })
      setConfirmDelete(false)
      await fetchFiles()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    }
  }, [request, fetchFiles])

  useEffect(() => { if (status === 'connected') fetchFiles() }, [status, fetchFiles])

  const backToList = () => { setView({ mode: 'list' }); setConfirmDelete(false) }

  // ── Inline back-nav row (non-list modes) ──────────────────────────────────
  const navRow = view.mode !== 'list' && (
    <div className="flex items-center justify-between mb-2">
      <button
        onClick={backToList}
        className="flex items-center gap-1.5 text-sm text-zinc-500 hover:text-zinc-200 transition-colors"
      >
        <ChevronLeft size={14} />
        Memory
      </button>

      {view.mode === 'view' && (
        <div className="flex items-center gap-2">
          {confirmDelete ? (
            <>
              <span className="text-xs text-red-400">Delete?</span>
              <button onClick={() => handleDelete((view as { name: string }).name)} className={btnDanger}>Yes</button>
              <button onClick={() => setConfirmDelete(false)} className={btnGhost}>No</button>
            </>
          ) : (
            <>
              <button
                onClick={() => setView({ mode: 'edit', name: (view as any).name, content: (view as any).content })}
                className={btnGhost}
              >
                <Pencil size={13} /> Edit
              </button>
              <button
                onClick={() => setConfirmDelete(true)}
                className={btnGhost + ' hover:text-red-400 hover:border-red-800/50'}
              >
                <Trash2 size={13} />
              </button>
            </>
          )}
        </div>
      )}
    </div>
  )

  return (
    <div className="flex flex-col h-full">
      {/* Loading spinner (file open) */}
      {view.mode === 'view' && loadingFile ? (
        <div className="flex-1 flex items-center justify-center">
          <Loader2 size={20} className="animate-spin text-zinc-500" />
        </div>
      ) : view.mode === 'view' ? (
        <div className="flex-1 overflow-y-auto">
          <div className="max-w-2xl mx-auto px-6 py-6">
            {navRow}
            {error && <ErrorBanner message={error} />}
            <h1 className="text-base font-semibold text-zinc-100 font-mono mb-5">
              {(view as any).name}
            </h1>
            <div className="prose prose-invert prose-sm max-w-none
              prose-p:leading-7 prose-headings:text-zinc-100 prose-headings:font-semibold
              prose-code:text-zinc-200 prose-code:bg-zinc-800 prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none
              prose-pre:bg-zinc-950 prose-pre:border prose-pre:border-zinc-800 prose-pre:rounded-xl prose-pre:text-xs
              prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400
              prose-a:text-blue-400 prose-strong:text-zinc-200 prose-hr:border-zinc-800">
              <Markdown remarkPlugins={[remarkGfm]}>{(view as any).content}</Markdown>
            </div>
          </div>
        </div>
      ) : view.mode === 'edit' || view.mode === 'new' ? (
        <div className="flex-1 overflow-y-auto">
          <div className="max-w-2xl mx-auto px-6 py-6 space-y-4">
            {navRow}
            {error && <ErrorBanner message={error} />}
            <FileEditor
              initial={view.mode === 'new' ? { name: '', content: '' } : { name: (view as any).name, content: (view as any).content }}
              onSave={handleSave}
              onCancel={() => view.mode === 'new' ? backToList() : setView({ mode: 'view', name: (view as any).name, content: (view as any).content })}
              saving={saving}
            />
          </div>
        </div>
      ) : (
        /* List */
        <div className="flex-1 overflow-y-auto">
          <div className="max-w-2xl mx-auto px-6 py-6 space-y-4">
            {/* New file button */}
            <div className="flex justify-end">
              <button
                onClick={() => setView({ mode: 'new' })}
                disabled={status !== 'connected'}
                className={btnPrimary}
              >
                <Plus size={13} /> New file
              </button>
            </div>

            {error && <ErrorBanner message={error} />}
            {loadingList && <LoadingRow />}
            {!loadingList && status !== 'connected' && <EmptyState>Waiting for connection…</EmptyState>}
            {!loadingList && status === 'connected' && files.length === 0 && (
              <EmptyState>No memory files yet. Click "New file" to create one.</EmptyState>
            )}
            {!loadingList && files.length > 0 && (
              <div className="space-y-1">
                {files.map((file) => (
                  <button
                    key={file.name}
                    onClick={() => openFile(file.name)}
                    className="w-full flex items-center gap-3 rounded-xl border border-zinc-800 bg-zinc-900/50 px-4 py-3 text-left hover:bg-zinc-800/60 hover:border-zinc-700 transition-colors group"
                  >
                    <FileText size={14} className="text-blue-400 shrink-0" />
                    <span className="flex-1 text-sm text-zinc-300 font-mono truncate group-hover:text-zinc-100">{file.name}</span>
                    <span className="text-xs text-zinc-600 shrink-0">{formatBytes(file.size)}</span>
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
