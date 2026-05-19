import { useRef, useEffect } from 'react'
import { useWebSocketContext } from '../contexts/WebSocketContext'
import MessageList from '../components/MessageList'
import MessageInput from '../components/MessageInput'
import ChatHeader from '../components/ChatHeader'

export default function Chat() {
  const { status, messages, isGenerating, sendMessage, cancel, reloadHistory } = useWebSocketContext()
  const containerRef = useRef<HTMLDivElement>(null)
  const loadedFor = useRef<string | null>(null)

  // Scroll to bottom whenever messages update.
  useEffect(() => {
    const el = containerRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [messages])

  // Load the active session's history from the server once per connection.
  useEffect(() => {
    if (status === 'connected' && loadedFor.current !== 'connected' && !isGenerating) {
      loadedFor.current = 'connected'
      reloadHistory()
    } else if (status !== 'connected') {
      loadedFor.current = null
    }
  }, [status, isGenerating, reloadHistory])

  return (
    <div className="flex flex-col h-full">
      <ChatHeader />
      <MessageList messages={messages} containerRef={containerRef} />
      <MessageInput
        onSend={sendMessage}
        onCancel={cancel}
        disabled={status !== 'connected'}
        isGenerating={isGenerating}
      />
    </div>
  )
}
