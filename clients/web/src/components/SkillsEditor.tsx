import { useState, useMemo } from 'react'
import { Edit3, Loader2, Check, X } from 'lucide-react'
import { inputCls, btnPrimary, btnGhost } from './PageLayout'
import { parseSkillFrontmatter } from '../lib/skillUtils'

interface Props {
  initial: { name: string; content: string }
  onSave: (name: string, content: string) => void
  onCancel: () => void
  saving: boolean
}

export default function SkillsEditor({ initial, onSave, onCancel, saving }: Props) {
  const isNew = !initial.name

  const parsed = useMemo(() => {
    if (isNew) return { body: '', meta: { name: '', description: '', keywords: [] as string[] } }
    return parseSkillFrontmatter(initial.content)
  }, [initial, isNew])

  const [name, setName] = useState(isNew ? '' : (parsed.meta.name || initial.name))
  const [description, setDescription] = useState(parsed.meta.description || '')
  const [keywordsInput, setKeywordsInput] = useState(parsed.meta.keywords?.join(', ') || '')
  const [version, setVersion] = useState(parsed.meta.version || '')
  const [whenToUse, setWhenToUse] = useState(parsed.meta.when_to_use || '')
  const [body, setBody] = useState(parsed.body || '')
  const [editorMode, setEditorMode] = useState<'visual' | 'raw'>('visual')
  const [rawText, setRawText] = useState(initial.content)

  const buildMarkdown = () => {
    const cleanName = name.trim()
    const desc = description.trim()
    const kw = keywordsInput.split(',').map(t => t.trim()).filter(Boolean)
    const kwStr = kw.length > 0 ? `[${kw.map(k => `"${k}"`).join(', ')}]` : '[]'

    let fm = `---\nname: "${cleanName}"\ndescription: "${desc}"\nkeywords: ${kwStr}\n`
    if (version.trim()) fm += `version: "${version.trim()}"\n`
    if (whenToUse.trim()) fm += `when_to_use: "${whenToUse.trim()}"\n`
    fm += '---\n\n'
    fm += body.trim()
    return fm
  }

  const handleSave = () => {
    if (editorMode === 'raw') {
      onSave(isNew ? name : initial.name, rawText)
      return
    }
    const cleanName = name.trim()
    onSave(cleanName, buildMarkdown())
  }

  const handleToggleMode = (mode: 'visual' | 'raw') => {
    if (mode === 'raw') {
      setRawText(buildMarkdown())
    } else {
      const p = parseSkillFrontmatter(rawText)
      setName(p.meta.name || name)
      setDescription(p.meta.description || description)
      setKeywordsInput(p.meta.keywords?.join(', ') || keywordsInput)
      setVersion(p.meta.version || version)
      setWhenToUse(p.meta.when_to_use || whenToUse)
      setBody(p.body)
    }
    setEditorMode(mode)
  }

  const isValid = () => {
    if (editorMode === 'raw') return rawText.trim().length > 0 && (isNew ? name.trim().length > 0 : true)
    return name.trim().length > 0 && body.trim().length > 0
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between border-b border-zinc-800 pb-3">
        <h2 className="text-sm font-semibold text-zinc-300 flex items-center gap-1.5">
          <Edit3 size={14} className="text-amber-400" />
          {isNew ? 'Create New Skill' : `Edit: ${parsed.meta.name || initial.name}`}
        </h2>
        <div className="flex items-center bg-zinc-950 border border-zinc-800 rounded-lg p-0.5 text-xs">
          <button onClick={() => handleToggleMode('visual')} className={`px-2.5 py-1 rounded-md transition-colors ${editorMode === 'visual' ? 'bg-zinc-800 text-zinc-100 font-medium' : 'text-zinc-400 hover:text-zinc-200'}`}>
            Structured
          </button>
          <button onClick={() => handleToggleMode('raw')} className={`px-2.5 py-1 rounded-md transition-colors ${editorMode === 'raw' ? 'bg-zinc-800 text-zinc-100 font-medium' : 'text-zinc-400 hover:text-zinc-200'}`}>
            Raw
          </button>
        </div>
      </div>

      {editorMode === 'visual' ? (
        <div className="space-y-4">
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4 bg-zinc-900/40 p-4 rounded-2xl border border-zinc-800/80">
            <div className="space-y-1.5">
              <label className="text-xs font-semibold text-zinc-400">Name</label>
              <input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. weather" className={inputCls} disabled={!isNew} autoFocus={isNew} />
              <p className="text-[10px] text-zinc-500">Unique key using only a-z, 0-9, _, -, .</p>
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-semibold text-zinc-400">Version (optional)</label>
              <input value={version} onChange={(e) => setVersion(e.target.value)} placeholder="e.g. 1.0.0" className={inputCls} />
            </div>
            <div className="space-y-1.5 md:col-span-2">
              <label className="text-xs font-semibold text-zinc-400">Description</label>
              <input value={description} onChange={(e) => setDescription(e.target.value)} placeholder="1-2 sentences summarizing this skill..." className={inputCls} />
            </div>
            <div className="space-y-1.5 md:col-span-2">
              <label className="text-xs font-semibold text-zinc-400">Keywords (Comma-separated)</label>
              <input value={keywordsInput} onChange={(e) => setKeywordsInput(e.target.value)} placeholder="e.g. weather, forecast, API" className={inputCls} />
            </div>
            <div className="space-y-1.5 md:col-span-2">
              <label className="text-xs font-semibold text-zinc-400">When To Use (optional)</label>
              <input value={whenToUse} onChange={(e) => setWhenToUse(e.target.value)} placeholder="e.g. User asks about weather conditions..." className={inputCls} />
            </div>
          </div>
          <div className="space-y-1.5">
            <label className="text-xs font-semibold text-zinc-400">Body (Markdown)</label>
            <textarea value={body} onChange={(e) => setBody(e.target.value)} rows={14} className="w-full rounded-xl border border-zinc-800 bg-zinc-950 px-4 py-3 text-sm font-mono text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-700 resize-y transition-colors" placeholder="Write content here..." />
          </div>
        </div>
      ) : (
        <div className="space-y-4">
          {isNew && (
            <div className="space-y-1.5">
              <label className="text-xs font-semibold text-zinc-400 font-mono">Skill Name</label>
              <input value={name} onChange={(e) => setName(e.target.value)} placeholder="my-skill-name" className={inputCls} autoFocus />
            </div>
          )}
          <div className="space-y-1.5">
            <label className="text-xs font-semibold text-zinc-400 font-mono">SKILL.md Content</label>
            <textarea value={rawText} onChange={(e) => setRawText(e.target.value)} rows={18} className="w-full rounded-xl border border-zinc-800 bg-zinc-950 px-4 py-3 text-sm font-mono text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-700 resize-y transition-colors" placeholder={'---\nname: "my-skill"\ndescription: "What this skill does."\nkeywords: ["example"]\n---\n\n# My Skill\n\nInstructions here...'} />
          </div>
        </div>
      )}

      <div className="flex gap-2 justify-end border-t border-zinc-800 pt-3">
        <button onClick={onCancel} className={btnGhost}><X size={13} /> Cancel</button>
        <button onClick={handleSave} disabled={saving || !isValid()} className={btnPrimary}>
          {saving ? <Loader2 size={13} className="animate-spin" /> : <Check size={13} />} Save
        </button>
      </div>
    </div>
  )
}
