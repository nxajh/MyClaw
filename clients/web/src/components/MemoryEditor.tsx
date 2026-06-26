import { useState, useMemo } from 'react'
import { Edit3, Loader2, Check, X } from 'lucide-react'
import { inputCls, btnPrimary, btnGhost } from './PageLayout'
import { parseFrontmatter, typeStyles } from '../lib/memoryUtils'

interface Props {
  initial: { name: string; content: string }
  onSave: (name: string, content: string) => void
  onCancel: () => void
  saving: boolean
}

export default function MemoryEditor({ initial, onSave, onCancel, saving }: Props) {
  const isNew = !initial.name

  const parsed = useMemo(() => {
    if (isNew) return { body: '', meta: { name: '', type: 'project' as const, summary: '', tags: [] as string[], created_at: '' } }
    return parseFrontmatter(initial.content)
  }, [initial, isNew])

  const [name, setName] = useState(isNew ? '' : (parsed.meta.name || initial.name.replace('.md', '')))
  const [memType, setMemType] = useState<'user' | 'feedback' | 'project' | 'reference'>(parsed.meta.type || 'project')
  const [summary, setSummary] = useState(parsed.meta.summary || '')
  const [tagsInput, setTagsInput] = useState(parsed.meta.tags ? parsed.meta.tags.join(', ') : '')
  const [body, setBody] = useState(parsed.body || '')
  const [editorMode, setEditorMode] = useState<'visual' | 'raw'>('visual')
  const [rawText, setRawText] = useState(initial.content)

  const handleSave = () => {
    if (editorMode === 'raw') {
      const actualName = isNew ? (name.endsWith('.md') ? name : `${name}.md`) : initial.name
      onSave(actualName, rawText)
      return
    }
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
    onSave(`${cleanName}.md`, fullMarkdown)
  }

  const handleToggleMode = (mode: 'visual' | 'raw') => {
    if (mode === 'raw') {
      const cleanName = name.trim().toLowerCase().replace(/\s+/g, '_').replace(/[^a-z0-9_-]/g, '')
      const tagsArray = tagsInput.split(',').map(t => t.trim()).filter(Boolean)
      const tagsStr = tagsArray.length > 0 ? `[${tagsArray.join(', ')}]` : '[]'
      const today = parsed.meta.created_at || new Date().toISOString().split('T')[0]
      setRawText(`---
name: ${cleanName}
type: ${memType}
summary: ${summary.trim()}
tags: ${tagsStr}
created_at: ${today}
---

${body.trim()}`)
    } else {
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
    if (editorMode === 'raw') return rawText.trim().length > 0 && (isNew ? name.trim().endsWith('.md') : true)
    return name.trim().length > 0 && body.trim().length > 0
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between border-b border-zinc-800 pb-3">
        <h2 className="text-sm font-semibold text-zinc-300 flex items-center gap-1.5">
          <Edit3 size={14} className="text-blue-400" />
          {isNew ? 'Create New Semantic Fact' : `Edit Fact: ${parsed.meta.name || initial.name}`}
        </h2>
        <div className="flex items-center bg-zinc-950 border border-zinc-800 rounded-lg p-0.5 text-xs">
          <button onClick={() => handleToggleMode('visual')} className={`px-2.5 py-1 rounded-md transition-colors ${editorMode === 'visual' ? 'bg-zinc-800 text-zinc-100 font-medium' : 'text-zinc-400 hover:text-zinc-200'}`}>
            Structured UI
          </button>
          <button onClick={() => handleToggleMode('raw')} className={`px-2.5 py-1 rounded-md transition-colors ${editorMode === 'raw' ? 'bg-zinc-800 text-zinc-100 font-medium' : 'text-zinc-400 hover:text-zinc-200'}`}>
            Raw Markdown
          </button>
        </div>
      </div>

      {editorMode === 'visual' ? (
        <div className="space-y-4">
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4 bg-zinc-900/40 p-4 rounded-2xl border border-zinc-800/80">
            <div className="space-y-1.5">
              <label className="text-xs font-semibold text-zinc-400">Fact ID / Key Name</label>
              <input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. delegate_patience" className={inputCls} disabled={!isNew} autoFocus={isNew} />
              <p className="text-[10px] text-zinc-500">Unique alphanumeric key using only a-z, 0-9, _, -</p>
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-semibold text-zinc-400">Memory Scope / Type</label>
              <select value={memType} onChange={(e) => setMemType(e.target.value as any)} className="w-full rounded-xl border border-zinc-800 bg-zinc-950 px-3.5 py-2.5 text-sm text-zinc-300 focus:border-zinc-700 outline-none transition-colors duration-150">
                <option value="user">👤 User Preference (Always Injected)</option>
                <option value="feedback">🎯 Feedback & Alignment (Always Injected)</option>
                <option value="project">📂 Project Context (On-Demand)</option>
                <option value="reference">📄 External Reference (On-Demand)</option>
              </select>
              <p className="text-[10px] text-zinc-500">{typeStyles[memType].desc}</p>
            </div>
            <div className="space-y-1.5 md:col-span-2">
              <label className="text-xs font-semibold text-zinc-400">Brief Summary</label>
              <input value={summary} onChange={(e) => setSummary(e.target.value)} placeholder="1-2 sentences summarizing this memory..." className={inputCls} />
            </div>
            <div className="space-y-1.5 md:col-span-2">
              <label className="text-xs font-semibold text-zinc-400">Tags (Comma-separated)</label>
              <input value={tagsInput} onChange={(e) => setTagsInput(e.target.value)} placeholder="e.g. rust, qqbot, bug" className={inputCls} />
            </div>
          </div>
          <div className="space-y-1.5">
            <label className="text-xs font-semibold text-zinc-400">Memory Body (Markdown)</label>
            <textarea value={body} onChange={(e) => setBody(e.target.value)} rows={14} className="w-full rounded-xl border border-zinc-800 bg-zinc-950 px-4 py-3 text-sm font-mono text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-700 resize-y transition-colors" placeholder="Write detailed rules, facts, or instructions here..." />
          </div>
        </div>
      ) : (
        <div className="space-y-4">
          {isNew && (
            <div className="space-y-1.5">
              <label className="text-xs font-semibold text-zinc-400 font-mono">Filename</label>
              <input value={name} onChange={(e) => setName(e.target.value)} placeholder="filename.md" className={inputCls} autoFocus />
              <p className="text-[10px] text-zinc-500">File must end with .md extension</p>
            </div>
          )}
          <div className="space-y-1.5">
            <label className="text-xs font-semibold text-zinc-400 font-mono">Full File Text (Includes Frontmatter)</label>
            <textarea value={rawText} onChange={(e) => setRawText(e.target.value)} rows={18} className="w-full rounded-xl border border-zinc-800 bg-zinc-950 px-4 py-3 text-sm font-mono text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-700 resize-y transition-colors" placeholder={'---\nname: my_fact\ntype: project\n---\n\nContent...'} />
          </div>
        </div>
      )}

      <div className="flex gap-2 justify-end border-t border-zinc-850 pt-3">
        <button onClick={onCancel} className={btnGhost}><X size={13} /> Cancel</button>
        <button onClick={handleSave} disabled={saving || !isValid()} className={btnPrimary}>
          {saving ? <Loader2 size={13} className="animate-spin" /> : <Check size={13} />} Save Fact
        </button>
      </div>

      {isNew && name && editorMode === 'raw' && !name.endsWith('.md') && (
        <p className="text-xs text-amber-400">Filename must end with .md when in raw editing mode</p>
      )}
    </div>
  )
}
