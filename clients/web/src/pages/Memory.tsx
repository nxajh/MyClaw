import { useEffect, useState, useCallback, useMemo } from 'react'
import { Plus, Loader2, Search, Tag, AlertTriangle, Link2 } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useToast } from '../components/Toast'
import { ErrorBanner, LoadingRow, EmptyState, btnPrimary } from '../components/PageLayout'
import MemoryEditor from '../components/MemoryEditor'
import MemoryViewer from '../components/MemoryViewer'
import { getStyle, type MemoryFile } from '../lib/memoryUtils'

const TABS = ['all', 'user', 'feedback', 'rule', 'project', 'reference'] as const
type Tab = typeof TABS[number]

const TAB_LABELS: Record<Tab, string> = {
  all: 'All Memories',
  user: '👤 Preferences',
  feedback: '🎯 Alignments',
  rule: '⚙️ Rules',
  project: '📂 Projects',
  reference: '📄 References',
}

export default function Memory() {
  const { status, request } = useWebSocketContext()
  const { toast } = useToast()
  const [files, setFiles] = useState<MemoryFile[]>([])
  const [loadingList, setLoadingList] = useState(false)
  const [loadingFile, setLoadingFile] = useState(false)
  const [error, setError] = useState<string | null>(null)

  type View =
    | { mode: 'list' }
    | { mode: 'view'; name: string; content: string }
    | { mode: 'edit'; name: string; content: string }
    | { mode: 'new' }

  const [view, setView] = useState<View>({ mode: 'list' })
  const [saving, setSaving] = useState(false)
  const [searchQuery, setSearchQuery] = useState('')
  const [activeTab, setActiveTab] = useState<Tab>('all')

  const isEditing = view.mode === 'edit' || view.mode === 'new'

  useEffect(() => {
    ;(window as any).myclawUnsaved = isEditing
    return () => { (window as any).myclawUnsaved = false }
  }, [isEditing])

  useEffect(() => {
    if (!isEditing) return
    const handleBeforeUnload = (e: BeforeUnloadEvent) => {
      e.preventDefault()
      e.returnValue = 'You have unsaved changes. Are you sure you want to leave?'
      return e.returnValue
    }
    window.addEventListener('beforeunload', handleBeforeUnload)
    return () => window.removeEventListener('beforeunload', handleBeforeUnload)
  }, [isEditing])

  const fetchFiles = useCallback(async () => {
    if (status !== 'connected') return
    setLoadingList(true)
    setError(null)
    try {
      const res = await request('memory.list')
      setFiles((res as MemoryFile[]) || [])
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
      toast('Memory fact saved', 'success')
      await fetchFiles()
      setView({ mode: 'view', name, content })
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      toast('Failed to save memory', 'error')
    } finally {
      setSaving(false)
    }
  }, [request, fetchFiles, toast])

  const handleDelete = useCallback(async (name: string) => {
    setError(null)
    try {
      await request('memory.delete', { name })
      toast('Memory fact deleted', 'success')
      setView({ mode: 'list' })
      await fetchFiles()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      toast('Failed to delete', 'error')
    }
  }, [request, fetchFiles, toast])

  useEffect(() => {
    if (status === 'connected') fetchFiles()
  }, [status, fetchFiles])

  const backToList = () => setView({ mode: 'list' })

  const filteredFiles = useMemo(() => {
    return files.filter(f => {
      const matchesTab = activeTab === 'all' || f.type === activeTab
      const q = searchQuery.toLowerCase()
      const matchesSearch =
        !q ||
        (f.mem_name || '').toLowerCase().includes(q) ||
        f.name.toLowerCase().includes(q) ||
        (f.description || '').toLowerCase().includes(q) ||
        (f.tags && f.tags.some(t => t.toLowerCase().includes(q))) ||
        (f.content || '').toLowerCase().includes(q)
      return matchesTab && matchesSearch
    })
  }, [files, activeTab, searchQuery])

  // ── View mode: detail viewer ──
  if (view.mode === 'view') {
    if (loadingFile) {
      return <div className="flex-1 flex items-center justify-center"><Loader2 size={24} className="animate-spin text-zinc-600" /></div>
    }
    return (
      <div className="flex flex-col h-full bg-zinc-950">
        <MemoryViewer
          name={view.name}
          content={view.content}
          error={error}
          onEdit={() => setView({ mode: 'edit', name: view.name, content: view.content })}
          onBack={backToList}
          onDelete={handleDelete}
        />
      </div>
    )
  }

  // ── Edit / New mode ──
  if (view.mode === 'edit' || view.mode === 'new') {
    return (
      <div className="flex flex-col h-full bg-zinc-950">
        <div className="flex-1 overflow-y-auto">
          <div className="max-w-3xl mx-auto px-3 sm:px-6 py-4 sm:py-6 space-y-4">
            <button onClick={backToList} className="flex items-center gap-1.5 text-sm text-zinc-500 hover:text-zinc-300 transition-colors mb-2">
              ← Back to Memories
            </button>
            {error && <ErrorBanner message={error} />}
            <MemoryEditor
              initial={view.mode === 'new' ? { name: '', content: '' } : { name: view.name, content: view.content }}
              onSave={handleSave}
              onCancel={() => view.mode === 'new' ? backToList() : setView({ mode: 'view', name: view.name, content: view.content })}
              saving={saving}
            />
          </div>
        </div>
      </div>
    )
  }

  // ── List mode ──
  return (
    <div className="flex flex-col h-full bg-zinc-950">
      <div className="flex-1 overflow-y-auto">
        <div className="max-w-3xl mx-auto px-3 sm:px-6 py-4 sm:py-6 space-y-4">
          {/* Header */}
          <div className="flex flex-col sm:flex-row sm:items-center gap-3 justify-between pb-2 border-b border-zinc-900">
            <div>
              <h1 className="text-base font-bold text-zinc-100">Memory</h1>
              <p className="text-xs text-zinc-500">{files.length} entries · Manage facts and rules guiding MyClaw</p>
            </div>
            <button onClick={() => setView({ mode: 'new' })} disabled={status !== 'connected'} className={btnPrimary}>
              <Plus size={13} /> New
            </button>
          </div>

          {/* Tabs */}
          <div className="flex flex-wrap gap-1 border-b border-zinc-900/60 pb-1">
            {TABS.map(tab => {
              const isActive = activeTab === tab
              const count = tab === 'all' ? files.length : files.filter(f => f.type === tab).length
              const s = tab === 'all' ? null : getStyle(tab)
              const tabColor = isActive
                ? (s ? `${s.badgeBg}` : 'bg-zinc-800 text-zinc-100 border border-zinc-700/60')
                : 'text-zinc-500 hover:text-zinc-300 hover:bg-zinc-900/30 border border-transparent'
              return (
                <button key={tab} onClick={() => setActiveTab(tab)} className={`px-3 py-1.5 rounded-xl text-xs font-medium transition-all ${tabColor}`}>
                  {TAB_LABELS[tab]} <span className="opacity-60">{count}</span>
                </button>
              )
            })}
          </div>

          {/* Search */}
          <div className="relative">
            <span className="absolute inset-y-0 left-0 pl-3.5 flex items-center pointer-events-none"><Search size={14} className="text-zinc-500" /></span>
            <input type="text" value={searchQuery} onChange={(e) => setSearchQuery(e.target.value)} placeholder="Search by name, description, or tags..." className="w-full rounded-2xl border border-zinc-900 bg-zinc-900/30 pl-10 pr-4 py-2.5 text-xs text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-800 transition-colors" />
          </div>

          {error && <ErrorBanner message={error} />}
          {loadingList && <LoadingRow />}
          {!loadingList && status !== 'connected' && <EmptyState>Waiting for connection to sync memory…</EmptyState>}
          {!loadingList && status === 'connected' && filteredFiles.length === 0 && (
            <div className="rounded-2xl border border-dashed border-zinc-900 p-8 text-center space-y-2">
              <AlertTriangle size={24} className="mx-auto text-zinc-600" />
              <p className="text-xs text-zinc-500 font-medium">No matching memories</p>
              {files.length > 0 && (
                <button onClick={() => { setSearchQuery(''); setActiveTab('all') }} className="text-xs text-blue-400 hover:text-blue-300">Reset filters</button>
              )}
            </div>
          )}

          {/* List */}
          {!loadingList && filteredFiles.length > 0 && (
            <div className="grid grid-cols-1 gap-3">
              {filteredFiles.map((file) => {
                const style = getStyle(file.type)
                const links = (file.link_count || 0) + (file.backlink_count || 0)
                return (
                  <button key={file.name} onClick={() => openFile(file.name)} className={`w-full text-left rounded-2xl border ${style.border} ${style.bg} px-4 py-4 transition-all duration-200 group flex flex-col gap-2.5`}>
                    <div className="flex flex-wrap items-center justify-between gap-2 w-full">
                      <div className="flex items-center gap-2">
                        <span className={`text-[9px] uppercase font-bold tracking-wider px-2 py-0.5 rounded-md ${style.badgeBg}`}>
                          {style.label.split(' ').slice(1).join(' ')}
                        </span>
                        <span className="text-sm font-bold text-zinc-300 font-mono truncate group-hover:text-zinc-100 transition-colors">
                          {file.mem_name || file.name.replace('.md', '')}
                        </span>
                      </div>
                      <div className="flex items-center gap-2 text-[10px] text-zinc-600 font-mono">
                        {links > 0 && <span className="flex items-center gap-0.5"><Link2 size={9} />{links}</span>}
                        <span>{(file.size / 1024).toFixed(1)} KB</span>
                      </div>
                    </div>
                    {file.description ? (
                      <p className="text-xs text-zinc-400 leading-relaxed font-normal">{file.description}</p>
                    ) : (
                      <p className="text-xs text-zinc-600 italic">No description</p>
                    )}
                    {file.tags && file.tags.length > 0 && (
                      <div className="flex flex-wrap gap-1">
                        {file.tags.map(t => (
                          <span key={t} className="flex items-center gap-1 text-[9px] text-zinc-500 bg-zinc-950 px-2 py-0.5 rounded-md border border-zinc-900">
                            <Tag size={8} />{t}
                          </span>
                        ))}
                      </div>
                    )}
                  </button>
                )
              })}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
