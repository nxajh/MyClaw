import { useState, useRef, useEffect, useCallback, type KeyboardEvent } from 'react'
import { ArrowUp, Square, Paperclip, X, FileText, Image as ImageIcon, Film, Music, File } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import type { SendOptions, FileAttachment } from '../hooks/useWebSocket'

interface Props {
  onSend: (content: string, opts?: SendOptions) => void
  onCancel: () => void
  disabled: boolean
  isGenerating: boolean
}

interface Command { name: string; description: string }
interface PickedImage { name: string; dataUrl: string }
interface PickedText { name: string; content: string }
interface PickedBinary { name: string; mime: string; dataUrl: string }

const IMAGE_MAX = 10 * 1024 * 1024
const TEXT_MAX = 256 * 1024
const BINARY_MAX = 50 * 1024 * 1024
const ACCEPT = '*/*'

export default function MessageInput({ onSend, onCancel, disabled, isGenerating }: Props) {
  const { status, request } = useWebSocketContext()
  const [text, setText] = useState('')
  const [images, setImages] = useState<PickedImage[]>([])
  const [texts, setTexts] = useState<PickedText[]>([])
  const [binaries, setBinaries] = useState<PickedBinary[]>([])
  const [note, setNote] = useState<string | null>(null)
  const [isDragging, setIsDragging] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileRef = useRef<HTMLInputElement>(null)

  const handleDragOver = (e: React.DragEvent) => {
    e.preventDefault()
    if (!disabled) setIsDragging(true)
  }

  const handleDragLeave = () => {
    setIsDragging(false)
  }

  const handleDrop = (e: React.DragEvent) => {
    e.preventDefault()
    setIsDragging(false)
    if (disabled) return
    if (e.dataTransfer.files && e.dataTransfer.files.length > 0) {
      handleFiles(e.dataTransfer.files)
    }
  }

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
  const isTextMime = (t: string) => t.startsWith('text/') || ['application/json', 'application/xml', 'application/javascript', 'application/x-sh'].includes(t)
  const isTextExt = (name: string) => /\.(txt|md|json|js|ts|tsx|jsx|py|rs|go|java|c|cpp|h|hpp|css|html|yml|yaml|toml|sh|log|csv|xml|sql|rb|php|swift|kt|scala|lua|r|jl|m|mm|vue|svelte|astro|mdx|conf|cfg|ini|env|dockerfile|makefile)$/i.test(name)

  const handleFiles = useCallback((files: FileList | null) => {
    if (!files) return
    setNote(null)
    Array.from(files).forEach((file) => {
      const isImage = file.type.startsWith('image/')
      const isText = isTextMime(file.type) || isTextExt(file.name)
      if (isImage) {
        if (file.size > IMAGE_MAX) { setNote(`${file.name} skipped (image > 10MB)`); return }
        const reader = new FileReader()
        reader.onload = () => setImages((p) => [...p, { name: file.name, dataUrl: reader.result as string }])
        reader.readAsDataURL(file)
      } else if (isText) {
        if (file.size > TEXT_MAX) { setNote(`${file.name} skipped (file > 256KB)`); return }
        const reader = new FileReader()
        reader.onload = () => setTexts((p) => [...p, { name: file.name, content: reader.result as string }])
        reader.readAsText(file)
      } else {
        if (file.size > BINARY_MAX) { setNote(`${file.name} skipped (file > 50MB)`); return }
        const reader = new FileReader()
        reader.onload = () => setBinaries((p) => [...p, { name: file.name, mime: file.type || 'application/octet-stream', dataUrl: reader.result as string }])
        reader.readAsDataURL(file)
      }
    })
  }, [])

  // ── Send ──────────────────────────────────────────────────────────────────
  const handleSend = () => {
    const trimmed = text.trim()
    if (disabled) return
    if (!trimmed && images.length === 0 && texts.length === 0 && binaries.length === 0) return
    const fileAttachments: FileAttachment[] = binaries.map((b) => {
      const b64 = b.dataUrl.split(',')[1] || b.dataUrl
      return { data: b64, mime_type: b.mime, file_name: b.name }
    })
    onSend(trimmed, {
      images: images.map((i) => i.dataUrl),
      attachments: texts.map((t) => ({ name: t.name, content: t.content })),
      files: fileAttachments,
    })
    setText('')
    setImages([])
    setTexts([])
    setBinaries([])
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

  const hasAttachments = images.length > 0 || texts.length > 0 || binaries.length > 0
  const canSend = !disabled && (text.trim().length > 0 || hasAttachments)

  const fileIcon = (_name: string, mime: string) => {
    if (mime.startsWith('video/')) return <Film size={12} className="text-purple-400 shrink-0" />
    if (mime.startsWith('audio/')) return <Music size={12} className="text-green-400 shrink-0" />
    return <File size={12} className="text-blue-400 shrink-0" />
  }

  return (
    <div className="px-2 sm:px-4 pb-3 sm:pb-5 pt-2 sm:pt-3 shrink-0 safe-area-bottom">
      <div className="max-w-3xl lg:max-w-4xl 2xl:max-w-5xl mx-auto relative">
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
        <div
          onDragOver={handleDragOver}
          onDragLeave={handleDragLeave}
          onDrop={handleDrop}
          className={`relative rounded-2xl border shadow-xl transition-all ${
            isDragging
              ? 'border-blue-500 bg-zinc-900/90 scale-[1.01] ring-1 ring-blue-500/30'
              : 'border-zinc-700/60 bg-zinc-900 focus-within:border-zinc-600'
          }`}
        >
          {isDragging && (
            <div className="absolute inset-0 z-30 rounded-2xl bg-zinc-950/75 backdrop-blur-[2px] flex items-center justify-center pointer-events-none">
              <span className="text-sm font-medium text-blue-400 flex items-center gap-2">
                <Paperclip size={16} className="animate-bounce" />
                Drop files here to attach
              </span>
            </div>
          )}

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
              {binaries.map((b, i) => (
                <div key={`b${i}`} className="flex items-center gap-1.5 h-8 px-2.5 rounded-lg bg-zinc-800 border border-zinc-700 text-xs text-zinc-300">
                  {fileIcon(b.name, b.mime)}
                  <span className="max-w-[140px] truncate">{b.name}</span>
                  <button onClick={() => setBinaries((p) => p.filter((_, j) => j !== i))} className="ml-1 text-zinc-500 hover:text-zinc-300">
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
            onPaste={(e) => {
              if (e.clipboardData.files && e.clipboardData.files.length > 0) {
                e.preventDefault()
                handleFiles(e.clipboardData.files)
              }
            }}
            placeholder={disabled ? 'Connecting…' : 'Message MyClaw…  (/ for commands)'}
            disabled={disabled && !isGenerating}
            rows={1}
            className="w-full resize-none bg-transparent px-3 sm:px-4 pt-3 sm:pt-4 pb-14 text-sm sm:text-base text-zinc-100 placeholder-zinc-600 outline-none disabled:opacity-50 leading-relaxed"
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
                  : <span className="hidden sm:flex items-center gap-1"><ImageIcon size={11} /> Shift+Enter for newline</span>}
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
