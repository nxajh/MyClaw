import { useState, useEffect, useRef, useMemo, memo } from 'react'
import { Pencil, Pin, Trash2, RotateCcw, Copy, Check } from 'lucide-react'
import type { MessageBlock } from '../hooks/useWebSocket'
import { LazyImage, renderFileRef } from '../lib/fileUtils'
import { highlightText } from '../lib/searchUtils'
import { SearchContext } from './MessageList'
import { splitSystemReminders, SystemReminderCard, renderBlock } from './MessageBlocks'

// ── Generating dots ──────────────────────────────────────────────────────

function GeneratingDots() {
  return (
    <div className="space-y-2 py-1">
      <div className="skeleton-line w-full" />
      <div className="skeleton-line w-full" />
      <div className="skeleton-line w-3/5" />
    </div>
  )
}

// ── Message actions ──────────────────────────────────────────────────────

export function extractText(blocks: MessageBlock[]): string {
  return blocks.filter((b): b is { type: 'content'; text: string } => b.type === 'content').map((b) => b.text).join('\n\n')
}

function MessageActions({ blocks, isLast, isGenerating, onRetry, onDelete, onPin, pinned }: { blocks: MessageBlock[]; isLast: boolean; isGenerating: boolean; onRetry?: () => void; onDelete?: () => void; onPin?: () => void; pinned?: boolean }) {
  const [copied, setCopied] = useState(false)
  const handleCopy = async () => {
    const text = extractText(blocks)
    if (!text) return
    try { await navigator.clipboard.writeText(text); setCopied(true); setTimeout(() => setCopied(false), 2000) } catch { /* ignore */ }
  }
  return (
    <div className="flex items-center gap-0.5 mt-1">
      <button onClick={handleCopy} className="p-1.5 rounded-md text-zinc-500 hover:text-zinc-300 hover:bg-zinc-800 transition-colors" title={copied ? 'Copied' : 'Copy message'}>
        {copied ? <Check size={14} className="text-emerald-400" /> : <Copy size={14} />}
      </button>
      {isLast && !isGenerating && onRetry && (
        <button onClick={onRetry} className="p-1.5 rounded-md text-zinc-500 hover:text-zinc-300 hover:bg-zinc-800 transition-colors" title="Regenerate response">
          <RotateCcw size={14} />
        </button>
      )}
      {onPin && (
        <button onClick={onPin} className={`p-1.5 rounded-md ${pinned ? 'text-zinc-200 hover:text-zinc-100 bg-zinc-800/70' : 'text-zinc-500 hover:text-zinc-300'} hover:bg-zinc-800 transition-colors`} title={pinned ? 'Unpin message' : 'Pin message'}>
          <Pin size={14} />
        </button>
      )}
      {onDelete && (
        <button onClick={onDelete} className="p-1.5 rounded-md text-zinc-500 hover:text-red-400 hover:bg-zinc-800 transition-colors" title="Delete message">
          <Trash2 size={14} />
        </button>
      )}
    </div>
  )
}

// ── Editable user bubble ───────────────────────────────────────────────

function EditableUserBubble({ content, images, files, onResend, onDelete, onPin, pinned }: {
  content: string; images?: { path: string; mime?: string; name?: string }[]; files?: { path: string; mime?: string; name?: string }[]
  onResend?: (text: string) => void; onDelete?: () => void; onPin?: () => void; pinned?: boolean; msgId?: string
}) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(content)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const segments = useMemo(() => splitSystemReminders(content), [content])
  const hasAttachments = !!((images && images.length > 0) || (files && files.length > 0))

  useEffect(() => { if (editing) { setDraft(content); setTimeout(() => textareaRef.current?.focus(), 0) } }, [editing, content])

  const handleSave = () => {
    const trimmed = draft.trim()
    if (trimmed && trimmed !== content && onResend) onResend(trimmed)
    setEditing(false)
  }

  return (
    <div className="flex justify-end gap-2.5 sm:gap-3.5 group/msg">
      <div className="max-w-[85%] sm:max-w-[78%] lg:max-w-[72%] rounded-2xl border border-zinc-700/40 bg-zinc-800/60 px-3 sm:px-4 lg:px-5 py-3 sm:py-4 text-sm text-zinc-100 leading-relaxed shadow-sm space-y-3 transition-colors hover:border-zinc-600/50">
        {images && images.length > 0 && (
          <div className="flex flex-wrap gap-2 mb-2">
            {images.map((img, i) => <LazyImage key={i} path={img.path} mime={img.mime} name={img.name} />)}
          </div>
        )}
        {files && files.length > 0 && (
          <div className="flex flex-col gap-2 mb-2">
            {files.map((f, i) => renderFileRef(f, i))}
          </div>
        )}
        {editing ? (
          <div className="space-y-2">
            <textarea
              ref={textareaRef}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); handleSave() } if (e.key === 'Escape') setEditing(false) }}
              className="w-full bg-zinc-900 border border-zinc-700 rounded-lg px-3 py-2 text-sm text-zinc-100 outline-none focus:border-zinc-500 resize-none min-h-[60px]"
              rows={3}
            />
            <div className="flex items-center gap-2 justify-end">
              {hasAttachments && <span className="mr-auto text-[10px] text-zinc-500">Attachments won't be resent</span>}
              <button onClick={() => setEditing(false)} className="px-2 py-1 text-xs text-zinc-400 hover:text-zinc-200 rounded-lg hover:bg-zinc-700 transition-colors">Cancel</button>
              <button onClick={handleSave} className="px-3 py-1 text-xs text-zinc-200 hover:text-zinc-50 rounded-lg hover:bg-zinc-700 transition-colors font-medium">Send</button>
            </div>
          </div>
        ) : (
          <>
          <SearchContext.Consumer>{(q) => (
            <div className="space-y-1.5">
              {segments.map((segment, index) => segment.type === 'system-reminder'
                ? <SystemReminderCard key={`sys-${index}`} text={segment.text} />
                : segment.text ? <div key={`text-${index}`} className="whitespace-pre-wrap">{highlightText(segment.text, q)}</div> : null
              )}
            </div>
          )}</SearchContext.Consumer>
          {/* Edit/Delete actions */}
          <div className="flex items-center gap-0.5 justify-end mt-1">
              {onResend && (
                <button onClick={() => setEditing(true)} className="p-1.5 rounded-md text-zinc-500 hover:text-zinc-300 hover:bg-zinc-700 transition-colors" title="Edit & resend">
                  <Pencil size={14} />
                </button>
              )}
              {onPin && (
                <button onClick={onPin} className={`p-1.5 rounded-md ${pinned ? 'text-zinc-200 hover:text-zinc-100 bg-zinc-700/70' : 'text-zinc-500 hover:text-zinc-300'} hover:bg-zinc-700 transition-colors`} title={pinned ? 'Unpin message' : 'Pin message'}>
                  <Pin size={14} />
                </button>
              )}
              {onDelete && (
                <button onClick={onDelete} className="p-1.5 rounded-md text-zinc-500 hover:text-red-400 hover:bg-zinc-700 transition-colors" title="Delete message">
                  <Trash2 size={14} />
                </button>
              )}
          </div>
          </>
        )}
      </div>
      <div className="mt-0.5 h-8 w-8 sm:h-10 sm:w-10 rounded-full bg-zinc-800 border border-zinc-700 flex items-center justify-center text-lg sm:text-xl shrink-0 select-none shadow-md">👤</div>
    </div>
  )
}

// ── Memoized bubbles ─────────────────────────────────────────────────────

export const UserBubble = memo(function UserBubble({ content, images, files, onResend, onDelete, onPin, pinned, msgId }: {
  content: string; images?: { path: string; mime?: string; name?: string }[]; files?: { path: string; mime?: string; name?: string }[]
  onResend?: (text: string) => void; onDelete?: () => void; onPin?: () => void; pinned?: boolean; msgId?: string
}) {
  return <EditableUserBubble content={content} images={images} files={files} onResend={onResend} onDelete={onDelete} onPin={onPin} pinned={pinned} msgId={msgId} />
})

interface AssistantBubbleProps {
  blocks: MessageBlock[]
  done: boolean
  isLast: boolean
  isGenerating: boolean
  onRetry?: () => void
  onDelete?: () => void
  onPin?: () => void
  pinned?: boolean
}

export const AssistantBubble = memo(function AssistantBubble({ blocks, done, isLast, isGenerating, onRetry, onDelete, onPin, pinned }: AssistantBubbleProps) {
  return (
    <div className="flex gap-2.5 sm:gap-3.5 group/msg">
      <div className="mt-0.5 h-8 w-8 sm:h-10 sm:w-10 rounded-full bg-zinc-800 border border-zinc-700 flex items-center justify-center text-lg sm:text-xl shrink-0 select-none shadow-md">🦀</div>
      <div className={`flex-1 min-w-0 rounded-2xl border bg-zinc-900/40 px-3 sm:px-4 lg:px-5 py-3 sm:py-4 space-y-3 shadow-sm transition-colors ${isGenerating ? 'generating-border' : 'border-zinc-800/80 hover:border-zinc-800'}`}>
        {blocks.map((block, i) => renderBlock(block, i, isGenerating))}
        {isGenerating && blocks.length === 0 && <GeneratingDots />}
        {done && <MessageActions blocks={blocks} isLast={isLast} isGenerating={isGenerating} onRetry={onRetry} onDelete={onDelete} onPin={onPin} pinned={pinned} />}
      </div>
    </div>
  )
})
