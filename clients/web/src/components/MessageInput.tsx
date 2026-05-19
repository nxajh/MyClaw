import { useState, useRef, useEffect, useCallback, type KeyboardEvent } from 'react'
import { ArrowUp, Square, Paperclip, X, FileText, Image as ImageIcon } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import type { SendOptions } from '../hooks/useWebSocket'

interface Props {
  onSend: (content: string, opts?: SendOptions) => void
  onCancel: () => void
  disabled: boolean
  isGenerating: boolean
}

interface Command { name: string; description: string }
interface PickedImage { name: string; dataUrl: string }
interface PickedText { name: string; content: string }

const IMAGE_MAX = 5 * 1024 * 1024
const TEXT_MAX = 256 * 1024
const ACCEPT = 'image/*,.txt,.md,.json,.js,.ts,.tsx,.jsx,.py,.rs,.go,.java,.c,.cpp,.h,.hpp,.css,.html,.yml,.yaml,.toml,.sh,.log,.csv,.xml,.sql'

export default function MessageInput({ onSend, onCancel, disabled, isGenerating }: Props) {
  const { status, request } = useWebSocketContext()
  const [text, setText] = useState('')
  const [images, setImages] = useState<PickedImage[]>([])
  const [texts, setTexts] = useState<PickedText[]>([])
  const [note, setNote] = useState<string | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileRef = useRef<HTMLInputElement>(null)

  // ── Slash-command autocomplete ────────────────────────────────────────────
  const [commands, setCommands] = useState<Command[]>([])
  const [cmdSel, setCmdSel] = useState(0)
  const loadedCmds = useRef(false)

  useEffect(() => {
    if (status !== 'connected' || loadedCmds.current) return
    loadedCmds.current = true
    request('commands.list')
      .then((r) => setCommands((r as Command[]) || []))
      .catch(() => { loadedCmds.current = false })
  }, [status, request])

  const slashQuery = text.startsWith('/') && !text.includes(' ') && !text.includes('\n')
    ? text.slice(1).toLowerCase()
    : null
  const matches = slashQuery !== null
    ? commands.filter((c) => c.name.toLowerCase().startsWith(slashQuery))
    : []
  const showCmds = matches.length > 0

  useEffect(() => { setCmdSel(0) }, [slashQuery])

  const acceptCommand = useCallback((cmd: Command) => {
    setText(`/${cmd.name} `)
    textareaRef.current?.focus()
  }, [])

  // ── File handling ─────────────────────────────────────────────────────────
  const handleFiles = useCallback((files: FileList | null) => {
    if (!files) return
    setNote(null)
    Array.from(files).forEach((file) => {
      const isImage = file.type.startsWith('image/')
      if (isImage) {
        if (file.size > IMAGE_MAX) { setNote(`${file.name} skipped (image > 5MB)`); return }
        const reader = new FileReader()
        reader.onload = () => setImages((p) => [...p, { name: file.name, dataUrl: reader.result as string }])
        reader.readAsDataURL(file)
      } else {
        if (file.size > TEXT_MAX) { setNote(`${file.name} skipped (file > 256KB)`); return }
        const reader = new FileReader()
        reader.onload = () => setTexts((p) => [...p, { name: file.name, content: reader.result as string }])
        reader.readAsText(file)
      }
    })
  }, [])

  // ── Send ──────────────────────────────────────────────────────────────────
  const handleSend = () => {
    const trimmed = text.trim()
    if (disabled) return
    if (!trimmed && images.length === 0 && texts.length === 0) return
    onSend(trimmed, {
      images: images.map((i) => i.dataUrl),
      attachments: texts.map((t) => ({ name: t.name, content: t.content })),
    })
    setText('')
    setImages([])
    setTexts([])
    setNote(null)
    if (textareaRef.current) textareaRef.current.style.height = 'auto'
  }

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (showCmds) {
      if (e.key === 'ArrowDown') { e.preventDefault(); setCmdSel((i) => Math.min(i + 1, matches.length - 1)); return }
      if (e.key === 'ArrowUp') { e.preventDefault(); setCmdSel((i) => Math.max(i - 1, 0)); return }
      if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
        e.preventDefault(); acceptCommand(matches[cmdSel]); return
      }
      if (e.key === 'Escape') { e.preventDefault(); setText(''); return }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      handleSend()
    }
  }

  const handleInput = () => {
    const el = textareaRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = Math.min(el.scrollHeight, 200) + 'px'
  }

  const hasAttachments = images.length > 0 || texts.length > 0
  const canSend = !disabled && (text.trim().length > 0 || hasAttachments)

  return (
    <div className="px-4 pb-5 pt-3 shrink-0">
      <div className="max-w-3xl mx-auto relative">
        {/* Slash command dropdown */}
        {showCmds && (
          <div className="absolute bottom-full mb-2 left-0 right-0 max-h-64 overflow-y-auto rounded-xl border border-zinc-800 bg-zinc-900 shadow-2xl py-1 z-20">
            {matches.map((c, i) => (
              <button
                key={c.name}
                onMouseEnter={() => setCmdSel(i)}
                onClick={() => acceptCommand(c)}
                className={`w-full flex items-baseline gap-2 px-3 py-2 text-left transition-colors ${
                  i === cmdSel ? 'bg-zinc-800' : 'hover:bg-zinc-800/60'
                }`}
              >
                <span className="font-mono text-sm text-blue-400 shrink-0">/{c.name}</span>
                <span className="text-xs text-zinc-500 truncate">{c.description}</span>
              </button>
            ))}
          </div>
        )}

        {/* Input box */}
        <div className="relative rounded-2xl border border-zinc-700/60 bg-zinc-900 shadow-xl focus-within:border-zinc-600 transition-colors">
          {/* Attachment chips */}
          {hasAttachments && (
            <div className="flex flex-wrap gap-2 px-4 pt-3">
              {images.map((img, i) => (
                <div key={`i${i}`} className="relative group">
                  <img src={img.dataUrl} alt={img.name} className="h-16 w-16 object-cover rounded-lg border border-zinc-700" />
                  <button
                    onClick={() => setImages((p) => p.filter((_, j) => j !== i))}
                    className="absolute -top-1.5 -right-1.5 h-5 w-5 rounded-full bg-zinc-800 border border-zinc-600 flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity"
                  >
                    <X size={11} className="text-zinc-300" />
                  </button>
                </div>
              ))}
              {texts.map((t, i) => (
                <div key={`t${i}`} className="flex items-center gap-1.5 h-8 px-2.5 rounded-lg bg-zinc-800 border border-zinc-700 text-xs text-zinc-300">
                  <FileText size={12} className="text-blue-400 shrink-0" />
                  <span className="max-w-[140px] truncate">{t.name}</span>
                  <button onClick={() => setTexts((p) => p.filter((_, j) => j !== i))} className="ml-1 text-zinc-500 hover:text-zinc-300">
                    <X size={11} />
                  </button>
                </div>
              ))}
            </div>
          )}

          <textarea
            ref={textareaRef}
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={handleKeyDown}
            onInput={handleInput}
            placeholder={disabled ? 'Connecting…' : 'Message MyClaw…  (/ for commands)'}
            disabled={disabled && !isGenerating}
            rows={1}
            className="w-full resize-none bg-transparent px-4 pt-4 pb-14 text-sm text-zinc-100 placeholder-zinc-600 outline-none disabled:opacity-50 leading-relaxed"
            style={{ maxHeight: 200 }}
          />

          {/* Toolbar inside box */}
          <div className="absolute bottom-3 left-3 right-3 flex items-center justify-between">
            <div className="flex items-center gap-2">
              <button
                onClick={() => fileRef.current?.click()}
                disabled={disabled}
                className="flex items-center justify-center h-8 w-8 rounded-lg text-zinc-500 hover:text-zinc-200 hover:bg-zinc-800 disabled:opacity-30 transition-colors"
                title="Attach files or images"
              >
                <Paperclip size={15} />
              </button>
              <input
                ref={fileRef}
                type="file"
                multiple
                accept={ACCEPT}
                className="hidden"
                onChange={(e) => { handleFiles(e.target.files); e.target.value = '' }}
              />
              <span className="text-xs text-zinc-600 select-none">
                {note ? <span className="text-amber-500">{note}</span>
                  : isGenerating ? 'Generating…'
                  : <span className="flex items-center gap-1"><ImageIcon size={11} /> Shift+Enter for newline</span>}
              </span>
            </div>
            {isGenerating ? (
              <button
                onClick={onCancel}
                className="flex items-center justify-center h-8 w-8 rounded-lg bg-zinc-700 hover:bg-zinc-600 transition-colors"
                title="Stop generating"
              >
                <Square size={13} className="text-zinc-300" />
              </button>
            ) : (
              <button
                onClick={handleSend}
                disabled={!canSend}
                className="flex items-center justify-center h-8 w-8 rounded-lg bg-zinc-100 hover:bg-white disabled:opacity-25 disabled:cursor-not-allowed transition-colors"
                title="Send (Enter)"
              >
                <ArrowUp size={15} className="text-zinc-900" />
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
