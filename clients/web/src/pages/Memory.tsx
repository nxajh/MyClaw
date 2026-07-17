import { useEffect, useState, useCallback, useMemo } from 'react'
import { Plus, Loader2, Tag, Link2, Brain } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useToast } from '../components/Toast'
import {
  ErrorBanner, EmptyState, SkeletonCards, PageHeader, PageShell, SearchField,
  EntityListItem, EntityMetaChip, btnPrimary,
} from '../components/PageLayout'
import MemoryEditor from '../components/MemoryEditor'
import MemoryViewer from '../components/MemoryViewer'
import {
  getStyle, getInjectStyle, normalizeInject,
  type MemoryFile, type InjectPolicy,
} from '../lib/memoryUtils'

/** Normalize inject for list rows (API may omit until backend ships inject field). */
function enrichMemoryRow(f: MemoryFile): MemoryFile {
  return { ...f, inject: normalizeInject(f.inject) }
}

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

const INJECT_FILTERS = ['all', 'always', 'search'] as const
type InjectFilter = typeof INJECT_FILTERS[number]

const INJECT_FILTER_LABELS: Record<InjectFilter, string> = {
  all: 'Any inject',
  always: 'Always',
  search: 'Search',
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
  const [injectFilter, setInjectFilter] = useState<InjectFilter>('all')

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
      const rows = ((res as MemoryFile[]) || []).map(enrichMemoryRow)
      setFiles(rows)
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

  const alwaysCount = useMemo(
    () => files.filter(f => normalizeInject(f.inject) === 'always').length,
    [files],
  )

  const filteredFiles = useMemo(() => {
    return files.filter(f => {
      const matchesTab = activeTab === 'all' || f.type === activeTab
      const inj = normalizeInject(f.inject)
      const matchesInject = injectFilter === 'all' || inj === injectFilter
      const q = searchQuery.toLowerCase()
      const matchesSearch =
        !q ||
        (f.mem_name || '').toLowerCase().includes(q) ||
        f.name.toLowerCase().includes(q) ||
        (f.description || '').toLowerCase().includes(q) ||
        (f.tags && f.tags.some(t => t.toLowerCase().includes(q))) ||
        (f.content || '').toLowerCase().includes(q) ||
        inj.includes(q)
      return matchesTab && matchesInject && matchesSearch
    })
  }, [files, activeTab, injectFilter, searchQuery])

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
      <PageShell>
        <button onClick={backToList} className="flex items-center gap-1.5 text-sm text-zinc-500 hover:text-zinc-300 transition-colors">
          ← Back to Memories
        </button>
        {error && <ErrorBanner message={error} />}
        <MemoryEditor
          initial={view.mode === 'new' ? { name: '', content: '' } : { name: view.name, content: view.content }}
          onSave={handleSave}
          onCancel={() => view.mode === 'new' ? backToList() : setView({ mode: 'view', name: view.name, content: view.content })}
          saving={saving}
        />
      </PageShell>
    )
  }

  // ── List mode ──
  return (
    <PageShell>
      <PageHeader
        title="Memory"
        subtitle={`${files.length} entries · ${alwaysCount} always-injected · type is semantic, inject controls system-reminder`}
        icon={<Brain size={18} className="text-violet-400" />}
        actions={
          <button onClick={() => setView({ mode: 'new' })} disabled={status !== 'connected'} className={btnPrimary}>
            <Plus size={13} /> New
          </button>
        }
      />

      {/* Type tabs */}
      <div className="flex flex-wrap gap-1 border-b border-zinc-800/60 pb-1">
        {TABS.map(tab => {
          const isActive = activeTab === tab
          const count = tab === 'all' ? files.length : files.filter(f => f.type === tab).length
          const s = tab === 'all' ? null : getStyle(tab)
          const tabColor = isActive
            ? (s ? `${s.badgeBg}` : 'bg-zinc-800 text-zinc-100 border border-zinc-700/60')
            : 'text-zinc-500 hover:text-zinc-300 hover:bg-zinc-900/30 border border-transparent'
          return (
            <button key={tab} onClick={() => setActiveTab(tab)} className={`px-3 py-1.5 rounded-xl text-xs font-medium transition-all ${tabColor}`}>
              {TAB_LABELS[tab]} <span className="opacity-50">{count}</span>
            </button>
          )
        })}
      </div>

      {/* Inject filter — orthogonal to type */}
      <div className="flex flex-wrap items-center gap-1.5">
        <span className="text-xs text-zinc-500 mr-1">Inject</span>
        {INJECT_FILTERS.map(f => {
          const isActive = injectFilter === f
          const count = f === 'all'
            ? files.length
            : files.filter(file => normalizeInject(file.inject) === f).length
          const s = f === 'all' ? null : getInjectStyle(f as InjectPolicy)
          const cls = isActive
            ? (s ? s.badgeBg : 'bg-zinc-800 text-zinc-100 border border-zinc-700/60')
            : 'text-zinc-500 hover:text-zinc-300 hover:bg-zinc-900/30 border border-transparent'
          return (
            <button
              key={f}
              onClick={() => setInjectFilter(f)}
              className={`px-2.5 py-1 rounded-lg text-xs font-medium transition-all border ${cls}`}
            >
              {INJECT_FILTER_LABELS[f]} <span className="opacity-50">{count}</span>
            </button>
          )
        })}
      </div>

      <SearchField
        value={searchQuery}
        onChange={setSearchQuery}
        placeholder="Search by name, description, tags, or inject…"
      />

      {error && <ErrorBanner message={error} />}
      {loadingList && <SkeletonCards count={6} />}
      {!loadingList && status !== 'connected' && (
        <EmptyState icon={<Brain size={28} />}>Waiting for connection to sync memory…</EmptyState>
      )}
      {!loadingList && status === 'connected' && filteredFiles.length === 0 && (
        <EmptyState
          icon={<Brain size={28} />}
          action={
            files.length > 0 ? (
              <button onClick={() => { setSearchQuery(''); setActiveTab('all'); setInjectFilter('all') }} className={btnPrimary}>
                Reset filters
              </button>
            ) : (
              <button onClick={() => setView({ mode: 'new' })} disabled={status !== 'connected'} className={btnPrimary}>
                <Plus size={13} /> New memory
              </button>
            )
          }
        >
          {files.length === 0 ? 'No memories yet. Add a fact to guide MyClaw.' : 'No matching memories'}
        </EmptyState>
      )}

      {/* Browse list — same EntityListItem family as Sessions/Skills */}
      {!loadingList && filteredFiles.length > 0 && (
        <div className="space-y-2">
          {filteredFiles.map((file) => {
            const style = getStyle(file.type)
            const injectStyle = getInjectStyle(file.inject)
            const links = (file.link_count || 0) + (file.backlink_count || 0)
            return (
              <EntityListItem
                key={file.name}
                density="comfortable"
                onClick={() => openFile(file.name)}
                leading={
                  <span className="flex flex-col gap-1 shrink-0 items-start">
                    <span className={`text-xs uppercase font-semibold tracking-wider px-2 py-1 rounded-md ${style.badgeBg}`}>
                      {style.label.split(' ').slice(1).join(' ') || style.label}
                    </span>
                    <span className={`text-[11px] font-semibold px-2 py-0.5 rounded-md ${injectStyle.badgeBg}`}>
                      {injectStyle.label}
                    </span>
                  </span>
                }
                title={
                  <span className="font-mono">{file.mem_name || file.name.replace('.md', '')}</span>
                }
                description={file.description || <span className="italic text-zinc-600">No description</span>}
                tags={
                  file.tags && file.tags.length > 0 ? (
                    <>
                      {file.tags.map(t => (
                        <EntityMetaChip key={t}><Tag size={10} />{t}</EntityMetaChip>
                      ))}
                    </>
                  ) : undefined
                }
                meta={
                  <span className="flex items-center gap-2">
                    {links > 0 && <span className="flex items-center gap-0.5"><Link2 size={11} />{links}</span>}
                    <span>{(file.size / 1024).toFixed(1)} KB</span>
                  </span>
                }
              />
            )
          })}
        </div>
      )}
    </PageShell>
  )
}
