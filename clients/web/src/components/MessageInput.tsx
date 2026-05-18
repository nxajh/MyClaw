import { useState, useRef, type KeyboardEvent } from 'react'
import { ArrowUp, Square } from 'lucide-react'

interface Props {
  onSend: (content: string) => void
  onCancel: () => void
  disabled: boolean
  isGenerating: boolean
}

export default function MessageInput({ onSend, onCancel, disabled, isGenerating }: Props) {
  const [text, setText] = useState('')
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const handleSend = () => {
    const trimmed = text.trim()
    if (!trimmed || disabled) return
    onSend(trimmed)
    setText('')
    if (textareaRef.current) {
      textareaRef.current.style.height = 'auto'
    }
  }

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
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

  const canSend = !disabled && text.trim().length > 0

  return (
    <div className="px-4 pb-5 pt-3 shrink-0">
      <div className="max-w-3xl mx-auto">
        {/* Input box */}
        <div className="relative rounded-2xl border border-zinc-700/60 bg-zinc-900 shadow-xl focus-within:border-zinc-600 transition-colors">
          <textarea
            ref={textareaRef}
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={handleKeyDown}
            onInput={handleInput}
            placeholder={disabled ? 'Connecting…' : 'Message MyClaw…'}
            disabled={disabled && !isGenerating}
            rows={1}
            className="w-full resize-none bg-transparent px-4 pt-4 pb-14 text-sm text-zinc-100 placeholder-zinc-600 outline-none disabled:opacity-50 leading-relaxed"
            style={{ maxHeight: 200 }}
          />

          {/* Toolbar inside box */}
          <div className="absolute bottom-3 left-4 right-3 flex items-center justify-between">
            <span className="text-xs text-zinc-600 select-none">
              {isGenerating ? 'Generating…' : 'Shift+Enter for new line'}
            </span>
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
