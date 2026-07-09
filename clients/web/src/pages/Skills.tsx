import { useEffect, useState, useCallback, useMemo } from 'react'
import { Sparkles, Search, Loader2, Plus } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { useToast } from '../components/Toast'
import { ErrorBanner, LoadingRow, EmptyState, btnPrimary, searchInputCls } from '../components/PageLayout'
import SkillsViewer from '../components/SkillsViewer'
import SkillsEditor from '../components/SkillsEditor'

interface Skill { name: string; description: string; keywords: string[] }

export default function Skills() {
  const { status, request } = useWebSocketContext()
  const { toast } = useToast()
  const [skills, setSkills] = useState<Skill[]>([])
  const [loading, setLoading] = useState(false)
  const [loadingFile, setLoadingFile] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [searchQuery, setSearchQuery] = useState('')

  type View =
    | { mode: 'list' }
    | { mode: 'view'; name: string; content: string }
    | { mode: 'edit'; name: string; content: string }
    | { mode: 'new' }

  const [view, setView] = useState<View>({ mode: 'list' })
  const [saving, setSaving] = useState(false)

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

  const fetchSkills = useCallback(async () => {
    if (status !== 'connected') return
    setLoading(true)
    setError(null)
    try {
      setSkills((await request('skills.list') as Skill[]) || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setLoading(false)
    }
  }, [status, request])

  const openFile = useCallback(async (name: string) => {
    setLoadingFile(true)
    setError(null)
    try {
      const res = await request('skills.read', { name }) as { name: string; content: string }
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
      await request('skills.write', { name, content })
      toast('Skill saved', 'success')
      await fetchSkills()
      setView({ mode: 'view', name, content })
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      toast('Failed to save skill', 'error')
    } finally {
      setSaving(false)
    }
  }, [request, fetchSkills, toast])

  const handleDelete = useCallback(async (name: string) => {
    setError(null)
    try {
      await request('skills.delete', { name })
      toast('Skill deleted', 'success')
      setView({ mode: 'list' })
      await fetchSkills()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      toast('Failed to delete', 'error')
    }
  }, [request, fetchSkills, toast])

  useEffect(() => { if (status === 'connected') fetchSkills() }, [status, fetchSkills])

  const backToList = () => setView({ mode: 'list' })

  const filteredSkills = useMemo(() => {
    if (!searchQuery.trim()) return skills
    const q = searchQuery.toLowerCase()
    return skills.filter(s =>
      s.name.toLowerCase().includes(q) ||
      s.description?.toLowerCase().includes(q) ||
      s.keywords?.some(k => k.toLowerCase().includes(q))
    )
  }, [skills, searchQuery])

  // ── View mode: detail viewer ──
  if (view.mode === 'view') {
    if (loadingFile) {
      return <div className="flex-1 flex items-center justify-center"><Loader2 size={24} className="animate-spin text-zinc-600" /></div>
    }
    return (
      <div className="flex flex-col h-full bg-zinc-950">
        <SkillsViewer
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
          <div className="px-3 sm:px-8 py-4 sm:py-6 space-y-4">
            <button onClick={backToList} className="flex items-center gap-1.5 text-sm text-zinc-500 hover:text-zinc-300 transition-colors mb-2">
              ← Back to Skills
            </button>
            {error && <ErrorBanner message={error} />}
            <SkillsEditor
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
        <div className="px-3 sm:px-8 py-4 sm:py-6 space-y-4">
          {/* Header */}
          <div className="flex flex-col sm:flex-row sm:items-center gap-3 justify-between pb-2 border-b border-zinc-800">
            <div>
              <h1 className="text-base font-bold text-zinc-100 flex items-center gap-1.5">
                <Sparkles size={14} className="text-amber-400" />
                Skills Library
              </h1>
              <p className="text-xs text-zinc-500 mt-0.5">{skills.length} loaded · Behavior templates guiding MyClaw's capabilities</p>
            </div>
            <button onClick={() => setView({ mode: 'new' })} disabled={status !== 'connected'} className={btnPrimary}>
              <Plus size={13} /> New
            </button>
          </div>

          {/* Search */}
          {skills.length > 0 && (
            <div className="relative">
              <span className="absolute inset-y-0 left-0 pl-3.5 flex items-center pointer-events-none">
                <Search size={14} className="text-zinc-500" />
              </span>
              <input
                type="text"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder="Search skills by name, description, or keyword…"
                className={searchInputCls}
              />
            </div>
          )}

          {error && <ErrorBanner message={error} />}
          {loading && <LoadingRow />}
          {!loading && status !== 'connected' && <EmptyState>Waiting for connection…</EmptyState>}
          {!loading && status === 'connected' && skills.length === 0 && (
            <EmptyState>📭 No skills loaded. Add SKILL.md files under workspace/skills/.</EmptyState>
          )}
          {!loading && status === 'connected' && skills.length > 0 && filteredSkills.length === 0 && (
            <EmptyState>
              <span className="block text-2xl mb-1">📭</span>
              No skills match "{searchQuery}"
              <button onClick={() => setSearchQuery('')} className="block mx-auto mt-2 text-xs text-blue-400 hover:text-blue-300">Clear filter</button>
            </EmptyState>
          )}
          {!loading && filteredSkills.length > 0 && (
            <div className="space-y-1.5">
              {filteredSkills.map((s) => (
                <button
                  key={s.name}
                  onClick={() => openFile(s.name)}
                  className="w-full text-left rounded-xl border border-zinc-800 bg-zinc-900/50 px-4 py-3 hover:bg-zinc-800/50 hover:border-zinc-700 transition-colors"
                >
                  <div className="flex items-center gap-2">
                    <Sparkles size={13} className="text-amber-400 shrink-0" />
                    <span className="font-mono text-sm text-zinc-200">{s.name}</span>
                  </div>
                  {s.description && (
                    <p className="mt-1.5 text-sm text-zinc-400 leading-relaxed">{s.description}</p>
                  )}
                  {s.keywords?.length > 0 && (
                    <div className="mt-2 flex flex-wrap gap-1.5">
                      {s.keywords.map((k) => (
                        <span key={k} className="px-2 py-0.5 rounded-md bg-zinc-800 text-[11px] text-zinc-500">
                          {k}
                        </span>
                      ))}
                    </div>
                  )}
                </button>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
