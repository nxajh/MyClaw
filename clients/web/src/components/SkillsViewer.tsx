import { useState } from 'react'
import { ChevronLeft, Pencil, Trash2, Sparkles, Tag } from 'lucide-react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { ErrorBanner } from './PageLayout'
import { parseSkillFrontmatter } from '../lib/skillUtils'

interface Props {
  name: string
  content: string
  error: string | null
  onEdit: () => void
  onBack: () => void
  onDelete: (name: string) => void
}

export default function SkillsViewer({ name, content, error, onEdit, onBack, onDelete }: Props) {
  const [confirmDelete, setConfirmDelete] = useState(false)
  const parsed = parseSkillFrontmatter(content)

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="px-3 sm:px-8 py-4 sm:py-6 space-y-5">
        {/* Nav row */}
        <div className="flex items-center justify-between border-b border-zinc-800 pb-2">
          <button onClick={onBack} className="flex items-center gap-1.5 text-sm text-zinc-500 hover:text-zinc-300 transition-colors">
            <ChevronLeft size={14} /> Back to Skills
          </button>
          <div className="flex items-center gap-2">
            {confirmDelete ? (
              <div className="flex items-center gap-1.5 bg-red-950/20 border border-red-800/30 px-2 py-1 rounded-xl">
                <span className="text-[11px] text-red-400 font-medium">Permanently Delete?</span>
                <button onClick={() => onDelete(name)} className="rounded-lg bg-red-600/80 hover:bg-red-600 px-2.5 py-1 text-xs text-white transition-colors">Yes</button>
                <button onClick={() => setConfirmDelete(false)} className="rounded-lg border border-zinc-800 hover:bg-zinc-800 px-2.5 py-1 text-xs text-zinc-400 hover:text-zinc-200 transition-colors">No</button>
              </div>
            ) : (
              <>
                <button onClick={onEdit} className="flex items-center gap-1.5 rounded-xl border border-zinc-800 hover:bg-zinc-800/60 px-3 py-1.5 text-xs text-zinc-400 hover:text-zinc-200 transition-colors">
                  <Pencil size={12} /> Edit
                </button>
                <button onClick={() => setConfirmDelete(true)} className="flex items-center justify-center rounded-xl border border-zinc-800 hover:bg-zinc-800/60 px-2 py-1.5 text-xs text-zinc-500 hover:text-red-400 hover:border-red-800/50 transition-colors">
                  <Trash2 size={12} />
                </button>
              </>
            )}
          </div>
        </div>

        {error && <ErrorBanner message={error} />}

        {/* Metadata panel */}
        <div className="p-5 rounded-2xl border border-zinc-800 bg-zinc-900/30 space-y-3">
          <div className="flex items-center gap-2">
            <Sparkles size={14} className="text-amber-400 shrink-0" />
            <h1 className="text-lg font-bold text-zinc-100 font-mono tracking-tight">
              {parsed.meta.name || name}
            </h1>
            {parsed.meta.version && (
              <span className="text-[10px] text-zinc-500 font-mono ml-1">v{parsed.meta.version}</span>
            )}
          </div>
          {parsed.meta.description && (
            <p className="text-sm text-zinc-400 leading-relaxed border-l-2 border-zinc-800 pl-3">{parsed.meta.description}</p>
          )}
          {parsed.meta.keywords.length > 0 && (
            <div className="flex flex-wrap gap-1.5 pt-1.5">
              {parsed.meta.keywords.map(k => (
                <span key={k} className="flex items-center gap-1 text-[10px] text-zinc-500 bg-zinc-900/60 px-2 py-0.5 rounded-md border border-zinc-800">
                  <Tag size={9} />{k}
                </span>
              ))}
            </div>
          )}
          {parsed.meta.when_to_use && (
            <div className="text-xs text-zinc-500 border-t border-zinc-800/60 pt-2.5 mt-1">
              <span className="font-semibold text-zinc-400">When to use: </span>
              {parsed.meta.when_to_use}
            </div>
          )}
        </div>

        {/* Body — readable max width on ultra-wide screens */}
        <div className="max-w-5xl">
          <div className="prose prose-invert prose-sm max-w-none bg-zinc-900/30 p-6 rounded-2xl border border-zinc-800
            prose-p:leading-7 prose-headings:text-zinc-100 prose-headings:font-semibold
            prose-code:text-zinc-200 prose-code:bg-zinc-800 prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none
            prose-pre:bg-zinc-950 prose-pre:border prose-pre:border-zinc-800 prose-pre:rounded-xl prose-pre:text-xs
            prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400
            prose-a:text-blue-400 prose-strong:text-zinc-200 prose-hr:border-zinc-800">
            <Markdown remarkPlugins={[remarkGfm]}>{parsed.body}</Markdown>
          </div>
        </div>
      </div>
    </div>
  )
}
