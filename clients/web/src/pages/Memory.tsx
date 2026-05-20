import { useEffect, useState, useCallback, useMemo } from 'react'
import { Plus, Pencil, Trash2, Check, X, ChevronLeft, Loader2, Search, Tag, Calendar, AlertTriangle, Edit3 } from 'lucide-react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import { ErrorBanner, LoadingRow, EmptyState, inputCls, btnPrimary, btnGhost } from '../components/PageLayout'

interface MemoryFile {
  name: string
  size: number
  mem_name?: string
  summary?: string
  tags?: string[]
  mem_type?: 'user' | 'feedback' | 'project' | 'reference'
  created_at?: string
}

const typeStyles = {
  user: {
    bg: 'bg-blue-500/5',
    border: 'border-blue-500/20 hover:border-blue-500/30',
    borderActive: 'border-blue-500/50 ring-1 ring-blue-500/20',
    text: 'text-blue-400',
    badgeBg: 'bg-blue-500/10 text-blue-400 border border-blue-500/20',
    label: '👤 User Preference',
    desc: 'Always-injected user preferences (highest scope)'
  },
  feedback: {
    bg: 'bg-red-500/5',
    border: 'border-red-500/20 hover:border-red-500/30',
    borderActive: 'border-red-500/50 ring-1 ring-red-500/20',
    text: 'text-red-400',
    badgeBg: 'bg-red-500/10 text-red-400 border border-red-500/20',
    label: '🎯 Feedback Correction',
    desc: 'Always-injected behavior corrections (strict constraints)'
  },
  project: {
    bg: 'bg-purple-500/5',
    border: 'border-purple-500/20 hover:border-purple-500/30',
    borderActive: 'border-purple-500/50 ring-1 ring-purple-500/20',
    text: 'text-purple-400',
    badgeBg: 'bg-purple-500/10 text-purple-400 border border-purple-500/20',
    label: '📂 Project Context',
    desc: 'On-demand workspace background knowledge'
  },
  reference: {
    bg: 'bg-amber-500/5',
    border: 'border-amber-500/20 hover:border-amber-500/30',
    borderActive: 'border-amber-500/50 ring-1 ring-amber-500/20',
    text: 'text-amber-400',
    badgeBg: 'bg-amber-500/10 text-amber-400 border border-amber-500/20',
    label: '📄 Reference Doc',
    desc: 'On-demand external APIs, specs, and definitions'
  }
}

function formatBytes(b: number) {
  if (b < 1024) return `${b} B`
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`
  return `${(b / (1024 * 1024)).toFixed(1)} MB`
}

// ── Frontmatter Parser & Builder ──────────────────────────────────────────────

function parseFrontmatter(raw: string) {
  const trimmed = raw.trim()
  if (!trimmed.startsWith('---')) {
    return {
      body: raw,
      meta: {
        name: '',
        type: 'project' as const,
        summary: '',
        tags: [] as string[],
        created_at: ''
      }
    }
  }
  const nextDash = trimmed.indexOf('\n---', 3)
  if (nextDash === -1) {
    return {
      body: raw,
      meta: {
        name: '',
        type: 'project' as const,
        summary: '',
        tags: [] as string[],
        created_at: ''
      }
    }
  }
  const yaml = trimmed.slice(3, nextDash)
  const body = trimmed.slice(nextDash + 4).trim()
  const meta = {
    name: '',
    type: 'project' as 'user' | 'feedback' | 'project' | 'reference',
    summary: '',
    tags: [] as string[],
    created_at: ''
  }

  yaml.split('\n').forEach(line => {
    const colon = line.indexOf(':')
    if (colon !== -1) {
      const k = line.slice(0, colon).trim()
      const v = line.slice(colon + 1).trim()
      if (k === 'name') meta.name = v
      else if (k === 'type') {
        if (['user', 'feedback', 'project', 'reference'].includes(v)) {
          meta.type = v as any
        }
      }
      else if (k === 'summary') meta.summary = v
      else if (k === 'created_at') meta.created_at = v
      else if (k === 'tags') {
        const inner = v.startsWith('[') && v.endsWith(']') ? v.slice(1, -1) : v
        meta.tags = inner.split(',').map(x => x.trim()).filter(Boolean)
      }
    }
  })

  return { body, meta }
}

// ── FileEditor (Structured Frontmatter-First Editor) ─────────────────────────

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
  const isNew = !initial.name

  // If editing, extract YAML frontmatter
  const parsed = useMemo(() => {
    if (isNew) {
      return {
        body: '',
        meta: {
          name: '',
          type: 'project' as const,
          summary: '',
          tags: [] as string[],
          created_at: ''
        }
      }
    }
    return parseFrontmatter(initial.content)
  }, [initial, isNew])

  const [name, setName] = useState(isNew ? '' : (parsed.meta.name || initial.name.replace('.md', '')))
  const [memType, setMemType] = useState<'user' | 'feedback' | 'project' | 'reference'>(parsed.meta.type || 'project')
  const [summary, setSummary] = useState(parsed.meta.summary || '')
  const [tagsInput, setTagsInput] = useState(parsed.meta.tags ? parsed.meta.tags.join(', ') : '')
  const [body, setBody] = useState(parsed.body || '')
  const [editorMode, setEditorMode] = useState<'visual' | 'raw'>('visual')

  // Raw fallback state
  const [rawText, setRawText] = useState(initial.content)

  const handleSave = () => {
    if (editorMode === 'raw') {
      const actualName = isNew ? (name.endsWith('.md') ? name : `${name}.md`) : initial.name
      onSave(actualName, rawText)
      return
    }

    // Visual Mode: construct yaml content
    const cleanName = name.trim().toLowerCase().replace(/\s+/g, '_').replace(/[^a-z0-9_-]/g, '')
    const tagsArray = tagsInput.split(',').map(t => t.trim()).filter(Boolean)
    const tagsStr = tagsArray.length > 0 ? `[${tagsArray.join(', ')}]` : '[]'
    const today = parsed.meta.created_at || new Date().toISOString().split('T')[0]

    const fullMarkdown = `---
name: ${cleanName}
type: ${memType}
summary: ${summary.trim()}
tags: ${tagsStr}
created_at: ${today}
---

${body.trim()}`

    const filename = `${cleanName}.md`
    onSave(filename, fullMarkdown)
  }

  // Handle visual-to-raw switch to keep edits synchronized
  const handleToggleMode = (mode: 'visual' | 'raw') => {
    if (mode === 'raw') {
      const cleanName = name.trim().toLowerCase().replace(/\s+/g, '_').replace(/[^a-z0-9_-]/g, '')
      const tagsArray = tagsInput.split(',').map(t => t.trim()).filter(Boolean)
      const tagsStr = tagsArray.length > 0 ? `[${tagsArray.join(', ')}]` : '[]'
      const today = parsed.meta.created_at || new Date().toISOString().split('T')[0]
      const synthesized = `---
name: ${cleanName}
type: ${memType}
summary: ${summary.trim()}
tags: ${tagsStr}
created_at: ${today}
---

${body.trim()}`
      setRawText(synthesized)
    } else {
      // Parse raw back to visual
      const p = parseFrontmatter(rawText)
      setName(p.meta.name || name)
      setMemType(p.meta.type || memType)
      setSummary(p.meta.summary || summary)
      setTagsInput(p.meta.tags ? p.meta.tags.join(', ') : tagsInput)
      setBody(p.body)
    }
    setEditorMode(mode)
  }

  const isValid = () => {
    if (editorMode === 'raw') {
      return rawText.trim().length > 0 && (isNew ? name.trim().endsWith('.md') : true)
    }
    return name.trim().length > 0 && body.trim().length > 0
  }

  return (
    <div className="space-y-4">
      {/* Editor Header Mode Switcher */}
      <div className="flex items-center justify-between border-b border-zinc-800 pb-3">
        <h2 className="text-sm font-semibold text-zinc-300 flex items-center gap-1.5">
          <Edit3 size={14} className="text-blue-400" />
          {isNew ? 'Create New Semantic Fact' : `Edit Fact: ${parsed.meta.name || initial.name}`}
        </h2>
        <div className="flex items-center bg-zinc-950 border border-zinc-800 rounded-lg p-0.5 text-xs">
          <button
            onClick={() => handleToggleMode('visual')}
            className={`px-2.5 py-1 rounded-md transition-colors ${editorMode === 'visual' ? 'bg-zinc-800 text-zinc-100 font-medium' : 'text-zinc-400 hover:text-zinc-200'}`}
          >
            Structured UI
          </button>
          <button
            onClick={() => handleToggleMode('raw')}
            className={`px-2.5 py-1 rounded-md transition-colors ${editorMode === 'raw' ? 'bg-zinc-800 text-zinc-100 font-medium' : 'text-zinc-400 hover:text-zinc-200'}`}
          >
            Raw Markdown
          </button>
        </div>
      </div>

      {editorMode === 'visual' ? (
        <div className="space-y-4 animate-fadeIn">
          {/* Metadata Section */}
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4 bg-zinc-900/40 p-4 rounded-2xl border border-zinc-800/80">
            <div className="space-y-1.5">
              <label className="text-xs font-semibold text-zinc-400">Fact ID / Key Name</label>
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="e.g. delegate_patience"
                className={inputCls}
                disabled={!isNew}
                autoFocus={isNew}
              />
              <p className="text-[10px] text-zinc-500">Unique alphanumeric key using only a-z, 0-9, _, -</p>
            </div>

            <div className="space-y-1.5">
              <label className="text-xs font-semibold text-zinc-400">Memory Scope / Type</label>
              <select
                value={memType}
                onChange={(e) => setMemType(e.target.value as any)}
                className="w-full rounded-xl border border-zinc-800 bg-zinc-950 px-3.5 py-2.5 text-sm text-zinc-300 focus:border-zinc-700 outline-none transition-colors duration-150"
              >
                <option value="user">👤 User Preference (Always Injected)</option>
                <option value="feedback">🎯 Feedback & Alignment (Always Injected)</option>
                <option value="project">📂 Project Context (On-Demand)</option>
                <option value="reference">📄 External Reference (On-Demand)</option>
              </select>
              <p className="text-[10px] text-zinc-500">{typeStyles[memType].desc}</p>
            </div>

            <div className="space-y-1.5 md:col-span-2">
              <label className="text-xs font-semibold text-zinc-400">Brief Summary</label>
              <input
                value={summary}
                onChange={(e) => setSummary(e.target.value)}
                placeholder="1-2 sentences summarizing this memory..."
                className={inputCls}
              />
            </div>

            <div className="space-y-1.5 md:col-span-2">
              <label className="text-xs font-semibold text-zinc-400">Tags (Comma-separated)</label>
              <input
                value={tagsInput}
                onChange={(e) => setTagsInput(e.target.value)}
                placeholder="e.g. rust, qqbot, bug"
                className={inputCls}
              />
            </div>
          </div>

          {/* Markdown Content Section */}
          <div className="space-y-1.5">
            <label className="text-xs font-semibold text-zinc-400">Memory Body (Markdown)</label>
            <textarea
              value={body}
              onChange={(e) => setBody(e.target.value)}
              rows={14}
              className="w-full rounded-xl border border-zinc-800 bg-zinc-950 px-4 py-3 text-sm font-mono text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-700 resize-y transition-colors"
              placeholder="Write detailed rules, facts, or instructions here..."
            />
          </div>
        </div>
      ) : (
        <div className="space-y-4 animate-fadeIn">
          {isNew && (
            <div className="space-y-1.5">
              <label className="text-xs font-semibold text-zinc-400 font-mono">Filename</label>
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="filename.md"
                className={inputCls}
                autoFocus
              />
              <p className="text-[10px] text-zinc-500">File must end with .md extension</p>
            </div>
          )}
          <div className="space-y-1.5">
            <label className="text-xs font-semibold text-zinc-400 font-mono">Full File Text (Includes Frontmatter)</label>
            <textarea
              value={rawText}
              onChange={(e) => setRawText(e.target.value)}
              rows={18}
              className="w-full rounded-xl border border-zinc-800 bg-zinc-950 px-4 py-3 text-sm font-mono text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-700 resize-y transition-colors"
              placeholder="---\nname: my_fact\ntype: project\n---\n\nContent..."
            />
          </div>
        </div>
      )}

      {/* Action Footer */}
      <div className="flex gap-2 justify-end border-t border-zinc-850 pt-3">
        <button onClick={onCancel} className={btnGhost}>
          <X size={13} /> Cancel
        </button>
        <button
          onClick={handleSave}
          disabled={saving || !isValid()}
          className={btnPrimary}
        >
          {saving ? <Loader2 size={13} className="animate-spin" /> : <Check size={13} />}
          Save Fact
        </button>
      </div>

      {isNew && name && editorMode === 'raw' && !name.endsWith('.md') && (
        <p className="text-xs text-amber-400">Filename must end with .md when in raw editing mode</p>
      )}
    </div>
  )
}

// ── Main Page Component ──────────────────────────────────────────────────────

export default function Memory() {
  const { status, request } = useWebSocketContext()
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
  const [confirmDelete, setConfirmDelete] = useState(false)

  // Filters & State
  const [searchQuery, setSearchQuery] = useState('')
  const [activeTab, setActiveTab] = useState<'all' | 'user' | 'feedback' | 'project' | 'reference'>('all')

  const isEditing = view.mode === 'edit' || view.mode === 'new'

  useEffect(() => {
    (window as any).myclawUnsaved = isEditing
    return () => {
      (window as any).myclawUnsaved = false
    }
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

  useEffect(() => {
    if (status === 'connected') fetchFiles()
  }, [status, fetchFiles])

  const backToList = () => {
    setView({ mode: 'list' })
    setConfirmDelete(false)
  }

  // Filtered memory list
  const filteredFiles = useMemo(() => {
    return files.filter(f => {
      const matchesTab = activeTab === 'all' || f.mem_type === activeTab
      const matchesSearch =
        !searchQuery ||
        f.name.toLowerCase().includes(searchQuery.toLowerCase()) ||
        (f.mem_name && f.mem_name.toLowerCase().includes(searchQuery.toLowerCase())) ||
        (f.summary && f.summary.toLowerCase().includes(searchQuery.toLowerCase())) ||
        (f.tags && f.tags.some(t => t.toLowerCase().includes(searchQuery.toLowerCase())))
      return matchesTab && matchesSearch
    })
  }, [files, activeTab, searchQuery])

  // View state yaml separation
  const activeParsed = useMemo(() => {
    if (view.mode !== 'view') return null
    return parseFrontmatter(view.content)
  }, [view])

  // Inline back-nav row (non-list modes)
  const navRow = view.mode !== 'list' && (
    <div className="flex items-center justify-between mb-4 border-b border-zinc-800 pb-2">
      <button
        onClick={backToList}
        className="flex items-center gap-1.5 text-sm text-zinc-500 hover:text-zinc-300 transition-colors"
      >
        <ChevronLeft size={14} />
        Back to Semantic Facts
      </button>

      {view.mode === 'view' && (
        <div className="flex items-center gap-2">
          {confirmDelete ? (
            <div className="flex items-center gap-1.5 bg-red-950/20 border border-red-800/30 px-2 py-1 rounded-xl">
              <span className="text-[11px] text-red-400 font-medium">Permanently Delete?</span>
              <button onClick={() => handleDelete(view.name)} className="rounded-lg bg-red-600/80 hover:bg-red-600 px-2.5 py-1 text-xs text-white transition-colors">Yes</button>
              <button onClick={() => setConfirmDelete(false)} className="rounded-lg border border-zinc-800 hover:bg-zinc-800 px-2.5 py-1 text-xs text-zinc-400 hover:text-zinc-200 transition-colors">No</button>
            </div>
          ) : (
            <>
              <button
                onClick={() => setView({ mode: 'edit', name: view.name, content: view.content })}
                className="flex items-center gap-1.5 rounded-xl border border-zinc-800 hover:bg-zinc-800/60 px-3 py-1.5 text-xs text-zinc-400 hover:text-zinc-200 transition-colors"
              >
                <Pencil size={12} /> Edit
              </button>
              <button
                onClick={() => setConfirmDelete(true)}
                className="flex items-center justify-center rounded-xl border border-zinc-800 hover:bg-zinc-800/60 px-2 py-1.5 text-xs text-zinc-500 hover:text-red-400 hover:border-red-800/50 transition-colors"
              >
                <Trash2 size={12} />
              </button>
            </>
          )}
        </div>
      )}
    </div>
  )

  return (
    <div className="flex flex-col h-full bg-zinc-950">
      {view.mode === 'view' && loadingFile ? (
        <div className="flex-1 flex items-center justify-center">
          <Loader2 size={24} className="animate-spin text-zinc-600 animate-pulse" />
        </div>
      ) : view.mode === 'view' && activeParsed ? (
        <div className="flex-1 overflow-y-auto">
          <div className="max-w-3xl mx-auto px-6 py-6 space-y-5">
            {navRow}
            {error && <ErrorBanner message={error} />}

            {/* Fact Showcase Metadata Panel */}
            <div className={`p-5 rounded-2xl border ${typeStyles[activeParsed.meta.type || 'project'].border} bg-zinc-900/10 space-y-3`}>
              <div className="flex flex-wrap items-center justify-between gap-3">
                <span className={`text-[10px] uppercase font-bold tracking-wider px-2.5 py-1 rounded-full ${typeStyles[activeParsed.meta.type || 'project'].badgeBg}`}>
                  {typeStyles[activeParsed.meta.type || 'project'].label}
                </span>
                <div className="flex items-center gap-3 text-[11px] text-zinc-500 font-mono">
                  {activeParsed.meta.created_at && (
                    <span className="flex items-center gap-1">
                      <Calendar size={11} />
                      {activeParsed.meta.created_at}
                    </span>
                  )}
                  <span>{formatBytes(view.content.length)}</span>
                </div>
              </div>

              <h1 className="text-lg font-bold text-zinc-100 font-mono tracking-tight leading-snug">
                {activeParsed.meta.name || view.name.replace('.md', '')}
              </h1>

              {activeParsed.meta.summary && (
                <p className="text-sm text-zinc-400 leading-relaxed border-l-2 border-zinc-800 pl-3">
                  {activeParsed.meta.summary}
                </p>
              )}

              {activeParsed.meta.tags && activeParsed.meta.tags.length > 0 && (
                <div className="flex flex-wrap gap-1.5 pt-1.5">
                  {activeParsed.meta.tags.map(t => (
                    <span key={t} className="flex items-center gap-1 text-[10px] text-zinc-500 bg-zinc-900/60 px-2 py-0.5 rounded-md border border-zinc-800/80">
                      <Tag size={9} />
                      {t}
                    </span>
                  ))}
                </div>
              )}
            </div>

            {/* Fact Body Document */}
            <div className="prose prose-invert prose-sm max-w-none bg-zinc-900/10 p-6 rounded-2xl border border-zinc-900
              prose-p:leading-7 prose-headings:text-zinc-100 prose-headings:font-semibold
              prose-code:text-zinc-200 prose-code:bg-zinc-800 prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none
              prose-pre:bg-zinc-950 prose-pre:border prose-pre:border-zinc-800 prose-pre:rounded-xl prose-pre:text-xs
              prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400
              prose-a:text-blue-400 prose-strong:text-zinc-200 prose-hr:border-zinc-800">
              <Markdown remarkPlugins={[remarkGfm]}>{activeParsed.body}</Markdown>
            </div>
          </div>
        </div>
      ) : view.mode === 'edit' || view.mode === 'new' ? (
        <div className="flex-1 overflow-y-auto">
          <div className="max-w-3xl mx-auto px-6 py-6 space-y-4">
            {navRow}
            {error && <ErrorBanner message={error} />}
            <FileEditor
              initial={view.mode === 'new' ? { name: '', content: '' } : { name: view.name, content: view.content }}
              onSave={handleSave}
              onCancel={() => view.mode === 'new' ? backToList() : setView({ mode: 'view', name: view.name, content: view.content })}
              saving={saving}
            />
          </div>
        </div>
      ) : (
        /* List Mode with Beautiful Tabs and Cards */
        <div className="flex-1 overflow-y-auto">
          <div className="max-w-3xl mx-auto px-6 py-6 space-y-4">
            {/* Header controls: Search & New File */}
            <div className="flex flex-col sm:flex-row sm:items-center gap-3 justify-between pb-2 border-b border-zinc-900">
              <div>
                <h1 className="text-base font-bold text-zinc-100">Semantic Memory System</h1>
                <p className="text-xs text-zinc-500">Manage structure-first facts and core rules guiding MyClaw</p>
              </div>
              <button
                onClick={() => setView({ mode: 'new' })}
                disabled={status !== 'connected'}
                className={btnPrimary}
              >
                <Plus size={13} /> New Fact
              </button>
            </div>

            {/* Scope / Type Tabs */}
            <div className="flex flex-wrap gap-1 border-b border-zinc-900/60 pb-1">
              {(['all', 'user', 'feedback', 'project', 'reference'] as const).map(tab => {
                const isActive = activeTab === tab
                let tabLabel = 'All Memories'
                let tabColor = 'text-zinc-400 hover:text-zinc-200 hover:bg-zinc-900/30'
                if (tab === 'user') {
                  tabLabel = '👤 Preferences'
                  tabColor = isActive ? 'bg-blue-500/10 text-blue-400 border border-blue-500/20' : 'text-zinc-500 hover:text-blue-400 hover:bg-blue-500/5'
                } else if (tab === 'feedback') {
                  tabLabel = '🎯 Alignments'
                  tabColor = isActive ? 'bg-red-500/10 text-red-400 border border-red-500/20' : 'text-zinc-500 hover:text-red-400 hover:bg-red-500/5'
                } else if (tab === 'project') {
                  tabLabel = '📂 Projects'
                  tabColor = isActive ? 'bg-purple-500/10 text-purple-400 border border-purple-500/20' : 'text-zinc-500 hover:text-purple-400 hover:bg-purple-500/5'
                } else if (tab === 'reference') {
                  tabLabel = '📄 References'
                  tabColor = isActive ? 'bg-amber-500/10 text-amber-400 border border-amber-500/20' : 'text-zinc-500 hover:text-amber-400 hover:bg-amber-500/5'
                } else if (isActive) {
                  tabColor = 'bg-zinc-800 text-zinc-100 border border-zinc-700/60'
                }

                return (
                  <button
                    key={tab}
                    onClick={() => setActiveTab(tab)}
                    className={`px-3 py-1.5 rounded-xl text-xs font-medium border border-transparent transition-all ${tabColor}`}
                  >
                    {tabLabel}
                  </button>
                )
              })}
            </div>

            {/* Keyword Search Input */}
            <div className="relative">
              <span className="absolute inset-y-0 left-0 pl-3.5 flex items-center pointer-events-none">
                <Search size={14} className="text-zinc-500" />
              </span>
              <input
                type="text"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder="Search semantic facts by ID, summary, or tags..."
                className="w-full rounded-2xl border border-zinc-900 bg-zinc-900/30 pl-10 pr-4 py-2.5 text-xs text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-800 transition-colors"
              />
            </div>

            {error && <ErrorBanner message={error} />}
            {loadingList && <LoadingRow />}

            {!loadingList && status !== 'connected' && (
              <EmptyState>Waiting for connection to sync memory facts…</EmptyState>
            )}

            {!loadingList && status === 'connected' && filteredFiles.length === 0 && (
              <div className="rounded-2xl border border-dashed border-zinc-900 p-8 text-center space-y-2">
                <AlertTriangle size={24} className="mx-auto text-zinc-600" />
                <p className="text-xs text-zinc-500 font-medium">No matching semantic facts found</p>
                {files.length > 0 && (
                  <button onClick={() => { setSearchQuery(''); setActiveTab('all') }} className="text-xs text-blue-400 hover:text-blue-300">
                    Reset all filters
                  </button>
                )}
              </div>
            )}

            {/* List Showcase */}
            {!loadingList && filteredFiles.length > 0 && (
              <div className="grid grid-cols-1 gap-3">
                {filteredFiles.map((file) => {
                  const style = typeStyles[file.mem_type || 'project']
                  return (
                    <button
                      key={file.name}
                      onClick={() => openFile(file.name)}
                      className={`w-full text-left rounded-2xl border ${style.border} ${style.bg} px-4 py-4 transition-all duration-200 group flex flex-col gap-2.5`}
                    >
                      <div className="flex flex-wrap items-center justify-between gap-2 w-full">
                        <div className="flex items-center gap-2">
                          <span className={`text-[9px] uppercase font-bold tracking-wider px-2 py-0.5 rounded-md ${style.badgeBg}`}>
                            {style.label.split(' ').slice(1).join(' ')}
                          </span>
                          <span className="text-sm font-bold text-zinc-300 font-mono truncate group-hover:text-zinc-100 transition-colors">
                            {file.mem_name || file.name.replace('.md', '')}
                          </span>
                        </div>
                        <span className="text-[10px] text-zinc-600 font-mono">{formatBytes(file.size)}</span>
                      </div>

                      {file.summary ? (
                        <p className="text-xs text-zinc-400 leading-relaxed font-normal">
                          {file.summary}
                        </p>
                      ) : (
                        <p className="text-xs text-zinc-600 italic">No summary provided</p>
                      )}

                      {file.tags && file.tags.length > 0 && (
                        <div className="flex flex-wrap gap-1">
                          {file.tags.map(t => (
                            <span key={t} className="flex items-center gap-1 text-[9px] text-zinc-500 bg-zinc-950 px-2 py-0.5 rounded-md border border-zinc-900">
                              <Tag size={8} />
                              {t}
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
      )}
    </div>
  )
}