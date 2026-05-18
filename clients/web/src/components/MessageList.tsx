import { useState } from 'react'
import type { RefObject } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { ChevronDown, ChevronRight } from 'lucide-react'
import type { ChatMessage } from '../hooks/useWebSocket'
import ToolCallCard from './ToolCallCard'

function GeneratingDots() {
  return (
    <div className="flex items-center gap-1 h-5">
      <span className="h-1.5 w-1.5 rounded-full bg-zinc-500 animate-bounce [animation-delay:-0.3s]" />
      <span className="h-1.5 w-1.5 rounded-full bg-zinc-500 animate-bounce [animation-delay:-0.15s]" />
      <span className="h-1.5 w-1.5 rounded-full bg-zinc-500 animate-bounce" />
    </div>
  )
}

function ThinkingBlock({ text }: { text: string }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="rounded-xl border border-zinc-800 overflow-hidden text-xs">
      <button
        onClick={() => setOpen(!open)}
        className="w-full flex items-center gap-2 px-3.5 py-2.5 text-left text-zinc-500 hover:text-zinc-400 hover:bg-zinc-800/30 transition-colors"
      >
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        <span>Thinking</span>
      </button>
      {open && (
        <div className="px-3.5 py-3 text-zinc-500 whitespace-pre-wrap leading-5 border-t border-zinc-800 bg-zinc-900/40">
          {text}
        </div>
      )}
    </div>
  )
}

interface Props {
  messages: ChatMessage[]
  containerRef: RefObject<HTMLDivElement | null>
}

export default function MessageList({ messages, containerRef }: Props) {
  if (messages.length === 0) {
    return (
      <div ref={containerRef} className="flex-1 overflow-y-auto flex flex-col items-center justify-center gap-4 px-6 text-center">
        <div className="text-5xl select-none">🦀</div>
        <div>
          <h2 className="text-lg font-semibold text-zinc-200 mb-1">How can I help?</h2>
          <p className="text-sm text-zinc-500 max-w-xs">
            Ask me anything — I can use tools, write code, search, and more.
          </p>
        </div>
      </div>
    )
  }

  return (
    <div ref={containerRef} className="flex-1 overflow-y-auto">
      <div className="max-w-3xl mx-auto px-4 py-8 space-y-8">
        {messages.map((msg) => {
          if (msg.role === 'user') {
            return (
              <div key={msg.id} className="flex justify-end">
                <div className="max-w-[78%] rounded-3xl rounded-br-lg bg-zinc-800 px-5 py-3.5 text-sm text-zinc-100 whitespace-pre-wrap leading-relaxed">
                  {msg.content}
                </div>
              </div>
            )
          }

          // Assistant
          return (
            <div key={msg.id} className="flex gap-3.5">
              {/* Avatar */}
              <div className="mt-0.5 h-7 w-7 rounded-full bg-zinc-800 border border-zinc-700 flex items-center justify-center text-base shrink-0 select-none">
                🦀
              </div>

              <div className="flex-1 min-w-0 space-y-3 pt-0.5">
                {/* Thinking */}
                {msg.thinking && <ThinkingBlock text={msg.thinking} />}

                {/* Tool calls */}
                {msg.toolCalls.length > 0 && (
                  <div className="space-y-1.5">
                    {msg.toolCalls.map((tc) => (
                      <ToolCallCard key={tc.id} toolCall={tc} />
                    ))}
                  </div>
                )}

                {/* Text content */}
                {msg.content && (
                  <div className="prose prose-invert prose-sm max-w-none
                    prose-p:leading-7 prose-p:my-2 first:prose-p:mt-0
                    prose-headings:text-zinc-100 prose-headings:font-semibold prose-headings:mt-5 prose-headings:mb-2
                    prose-code:text-zinc-200 prose-code:bg-zinc-800 prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded-md prose-code:text-[0.8em] prose-code:before:content-none prose-code:after:content-none
                    prose-pre:bg-zinc-950 prose-pre:border prose-pre:border-zinc-800 prose-pre:rounded-xl prose-pre:text-xs
                    prose-blockquote:border-zinc-700 prose-blockquote:text-zinc-400
                    prose-a:text-blue-400 prose-a:no-underline hover:prose-a:underline
                    prose-strong:text-zinc-200 prose-strong:font-semibold
                    prose-ul:my-2 prose-ol:my-2 prose-li:my-0.5
                    prose-hr:border-zinc-800">
                    <Markdown remarkPlugins={[remarkGfm]}>{msg.content}</Markdown>
                  </div>
                )}

                {/* Generating */}
                {!msg.done && !msg.content && msg.toolCalls.length === 0 && !msg.thinking && (
                  <GeneratingDots />
                )}
              </div>
            </div>
          )
        })}
      </div>
    </div>
  )
}
