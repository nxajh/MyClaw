import { useState } from 'react'
import { ChevronLeft, Pencil, Trash2, Calendar, Tag, Link2 } from 'lucide-react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { ErrorBanner } from './PageLayout'
import { parseFrontmatter, getStyle, getInjectStyle, formatBytes } from '../lib/memoryUtils'

interface Props {
  name: string
  content: string
  error: string | null
  onEdit: () => void
  onBack: () => void
  onDelete: (name: string) => void
}

export default function MemoryViewer({ name, content, error, onEdit, onBack, onDelete }: Props) {
  const [confirmDelete, setConfirmDelete] = useState(false)
  const parsed = parseFrontmatter(content)
  const style = getStyle(parsed.meta.type)
  const injectStyle = getInjectStyle(parsed.meta.inject)

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="px-3 sm:px-8 py-4 sm:py-6 space-y-5">
        {/* Nav row */}
        <div className="flex items-center justify-between border-b border-zinc-800 pb-2">
          <button onClick={onBack} className="flex items-center gap-1.5 text-sm text-zinc-500 hover:text-zinc-300 transition-colors">
            <ChevronLeft size={14} /> Back to Memories
          </button>
          <div className="flex items-center gap-2">
            {confirmDelete ? (
              <div className="flex items-center gap-1.5 bg-red-950/20 border border-red-800/30 px-2 py-1 rounded-xl">
                <span className="text-xs text-red-400 font-medium">Permanently Delete?</span>
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
        <div className={`p-5 rounded-2xl border ${style.border} bg-zinc-900/30 space-y-3`}>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex flex-wrap items-center gap-2">
              <span className={`text-xs uppercase font-semibold tracking-wider px-2.5 py-1 rounded-full ${style.badgeBg}`}>
                {style.label}
              </span>
              <span
                className={`text-xs font-semibold tracking-wide px-2.5 py-1 rounded-full ${injectStyle.badgeBg}`}
                title={injectStyle.desc}
              >
                Inject · {injectStyle.label}
              </span>
            </div>
            <div className="flex items-center gap-3 text-xs text-zinc-500 font-mono">
              {parsed.meta.updated_at || parsed.meta.created_at ? (
                <span className="flex items-center gap-1"><Calendar size={11} />{parsed.meta.updated_at || parsed.meta.created_at}</span>
              ) : null}
              <span>{formatBytes(content.length)}</span>
            </div>
          </div>
          <h1 className="text-lg font-semibold text-zinc-100 font-mono tracking-tight leading-snug">
            {parsed.meta.name || name.replace('.md', '')}
          </h1>
          {parsed.meta.description && (
            <p className="text-sm text-zinc-400 leading-relaxed border-l-2 border-zinc-800 pl-3">{parsed.meta.description}</p>
          )}
          {parsed.meta.tags && parsed.meta.tags.length > 0 && (
            <div className="flex flex-wrap gap-1.5 pt-1.5">
              {parsed.meta.tags.map(t => (
                <span key={t} className="flex items-center gap-1 text-xs text-zinc-500 bg-zinc-900/60 px-2 py-0.5 rounded-md border border-zinc-800">
                  <Tag size={10} />{t}
                </span>
              ))}
            </div>
          )}
          {/* See Also links rendered from body */}
          {(() => {
            const seeAlso = extractSeeAlso(parsed.body)
            if (seeAlso.length === 0) return null
            return (
              <div className="flex flex-wrap gap-1.5 pt-1.5">
                {seeAlso.map(link => (
                  <span key={link} className="flex items-center gap-1 text-xs text-zinc-500 bg-zinc-900/40 px-2 py-0.5 rounded-md border border-zinc-800/60">
                    <Link2 size={10} />{link}
                  </span>
                ))}
              </div>
            )
          })()}
        </div>

        {/* Body */}
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
  )
}

function extractSeeAlso(body: string): string[] {
  const section = body.match(/^##\s+See\s+Also\s*$/im)
  if (!section) return []
  const afterSection = body.slice(section.index! + section[0].length)
  // Match markdown links: [label](memory-name) or [label](memory-name.md)
  const links = [...afterSection.matchAll(/\[([^\]]+)\]\(memory-?([^)]+)\)/gi)]
  return links.map(m => m[2].replace(/\.md$/, ''))
}
