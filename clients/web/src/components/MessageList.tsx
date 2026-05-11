import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { ChatMessage } from '../hooks/useWebSocket'
import ToolCallCard from './ToolCallCard'

interface Props {
  messages: ChatMessage[]
}

export default function MessageList({ messages }: Props) {
  if (messages.length === 0) {
    return (
      <div className="flex-1 flex items-center justify-center text-zinc-500 text-sm">
        Send a message to start chatting
      </div>
    )
  }

  return (
    <div className="flex-1 overflow-y-auto px-4 py-6 space-y-6">
      {messages.map((msg) => {
        if (msg.role === 'user') {
          return (
            <div key={msg.id} className="flex justify-end">
              <div className="max-w-[75%] rounded-2xl rounded-br-sm bg-blue-600/20 border border-blue-500/30 px-4 py-3 text-sm text-zinc-100 whitespace-pre-wrap">
                {msg.content}
              </div>
            </div>
          )
        }

        // Assistant message
        return (
          <div key={msg.id} className="flex justify-start">
            <div className="max-w-[80%] space-y-3">
              {/* Thinking */}
              {msg.thinking && (
                <details className="text-xs text-zinc-400 border border-zinc-700/50 rounded-lg">
                  <summary className="px-3 py-2 cursor-pointer select-none hover:text-zinc-300">
                    💭 Thinking…
                  </summary>
                  <div className="px-3 pb-3 whitespace-pre-wrap opacity-70">{msg.thinking}</div>
                </details>
              )}

              {/* Tool calls */}
              {msg.toolCalls.map((tc) => (
                <ToolCallCard key={tc.id} toolCall={tc} />
              ))}

              {/* Content */}
              {msg.content && (
                <div className="prose prose-invert prose-sm max-w-none rounded-xl bg-zinc-800/50 border border-zinc-700/40 px-4 py-3">
                  <Markdown remarkPlugins={[remarkGfm]}>{msg.content}</Markdown>
                </div>
              )}

              {/* Generating indicator */}
              {!msg.done && !msg.content && msg.toolCalls.length === 0 && (
                <div className="flex items-center gap-2 text-zinc-400 text-sm">
                  <span className="inline-block h-2 w-2 rounded-full bg-emerald-400 animate-pulse" />
                  Generating…
                </div>
              )}
            </div>
          </div>
        )
      })}
    </div>
  )
}
