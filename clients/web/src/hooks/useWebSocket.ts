import { useRef, useState, useCallback, useEffect } from 'react'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface UserMessage {
  role: 'user'
  content: string
  id: string
}

export interface AssistantMessage {
  role: 'assistant'
  content: string
  thinking?: string
  toolCalls: ToolCall[]
  id: string
  done: boolean
}

export interface ToolCall {
  name: string
  args: Record<string, unknown>
  output?: string
  id: string
}

export type ChatMessage = UserMessage | AssistantMessage

export type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

let msgCounter = 0
function uid(): string {
  return `msg-${++msgCounter}-${Date.now()}`
}

function getWsUrl(): string {
  if (import.meta.env.DEV) {
    return 'ws://127.0.0.1:18789/myclaw'
  }
  const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${proto}//${window.location.host}/myclaw`
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

export function useWebSocket() {
  const wsRef = useRef<WebSocket | null>(null)
  const reconnectTimer = useRef<ReturnType<typeof setTimeout>>(null)
  const [status, setStatus] = useState<ConnectionStatus>('disconnected')
  const [messages, setMessages] = useState<ChatMessage[]>([])
  const [isGenerating, setIsGenerating] = useState(false)

  // We keep a ref to the latest assistant message id so we can append chunks
  // without depending on state in the onmessage handler.
  const currentAssistantId = useRef<string | null>(null)

  // -----------------------------------------------------------------------
  // Connect
  // -----------------------------------------------------------------------

  const connect = useCallback(() => {
    if (wsRef.current && wsRef.current.readyState === WebSocket.OPEN) return

    try {
      const ws = new WebSocket(getWsUrl())
      wsRef.current = ws
      setStatus('connecting')

      ws.onopen = () => {
        setStatus('connected')
      }

      ws.onclose = () => {
        setStatus('disconnected')
        setIsGenerating(false)
        currentAssistantId.current = null
        // Auto-reconnect after 2 s
        reconnectTimer.current = setTimeout(connect, 2000)
      }

      ws.onerror = () => {
        // onclose will fire after this
      }

      ws.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data as string)
          handleServerMessage(data)
        } catch {
          // ignore malformed JSON
        }
      }
    } catch {
      setStatus('disconnected')
      reconnectTimer.current = setTimeout(connect, 2000)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // -----------------------------------------------------------------------
  // Server message handler
  // -----------------------------------------------------------------------

  const handleServerMessage = useCallback((data: Record<string, unknown>) => {
    const type = data.type as string

    switch (type) {
      case 'chunk': {
        const delta = (data.delta as string) || ''
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            return [
              ...prev.slice(0, -1),
              { ...last, content: last.content + delta },
            ]
          }
          // If no in-progress assistant message, create one
          const id = uid()
          currentAssistantId.current = id
          return [...prev, { role: 'assistant', content: delta, toolCalls: [], id, done: false }]
        })
        break
      }

      case 'thinking': {
        const delta = (data.delta as string) || ''
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            return [
              ...prev.slice(0, -1),
              { ...last, thinking: (last.thinking || '') + delta },
            ]
          }
          const id = uid()
          currentAssistantId.current = id
          return [...prev, { role: 'assistant', content: '', thinking: delta, toolCalls: [], id, done: false }]
        })
        break
      }

      case 'tool_call': {
        const name = (data.name as string) || 'unknown'
        const args = (data.args as Record<string, unknown>) || {}
        const tcId = uid()
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            return [
              ...prev.slice(0, -1),
              { ...last, toolCalls: [...last.toolCalls, { name, args: args as Record<string, unknown>, id: tcId }] },
            ]
          }
          const id = uid()
          currentAssistantId.current = id
          return [...prev, { role: 'assistant', content: '', toolCalls: [{ name, args: args as Record<string, unknown>, id: tcId }], id, done: false }]
        })
        break
      }

      case 'tool_result': {
        const name = (data.name as string) || 'unknown'
        const output = (data.output as string) || ''
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            const newToolCalls = last.toolCalls.map((tc) =>
              tc.name === name && tc.output === undefined ? { ...tc, output } : tc,
            )
            return [...prev.slice(0, -1), { ...last, toolCalls: newToolCalls }]
          }
          return prev
        })
        break
      }

      case 'done': {
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            const text = (data.text as string) || last.content
            return [...prev.slice(0, -1), { ...last, content: text, done: true }]
          }
          return prev
        })
        setIsGenerating(false)
        currentAssistantId.current = null
        break
      }

      case 'error': {
        const message = (data.message as string) || 'Unknown error'
        setMessages((prev) => [
          ...prev,
          { role: 'assistant', content: `⚠️ Error: ${message}`, toolCalls: [], id: uid(), done: true },
        ])
        setIsGenerating(false)
        currentAssistantId.current = null
        break
      }

      // api_response / api_error are handled by the API helpers via callbacks
      default:
        break
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // -----------------------------------------------------------------------
  // Send helpers
  // -----------------------------------------------------------------------

  const sendRaw = useCallback((obj: Record<string, unknown>) => {
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(obj))
    }
  }, [])

  const sendMessage = useCallback(
    (content: string) => {
      const userMsg: ChatMessage = { role: 'user', content, id: uid() }
      setMessages((prev) => [...prev, userMsg])
      // Prepare assistant placeholder
      const assistantId = uid()
      currentAssistantId.current = assistantId
      setMessages((prev) => [
        ...prev,
        { role: 'assistant', content: '', toolCalls: [], id: assistantId, done: false },
      ])
      setIsGenerating(true)
      sendRaw({ type: 'message', content })
    },
    [sendRaw],
  )

  const cancel = useCallback(() => {
    sendRaw({ type: 'cancel' })
    setIsGenerating(false)
    // Mark current assistant message as done
    setMessages((prev) => {
      const last = prev[prev.length - 1]
      if (last && last.role === 'assistant' && !last.done) {
        return [...prev.slice(0, -1), { ...last, done: true }]
      }
      return prev
    })
    currentAssistantId.current = null
  }, [sendRaw])

  const ping = useCallback(() => {
    sendRaw({ type: 'ping' })
  }, [sendRaw])

  // -----------------------------------------------------------------------
  // Lifecycle
  // -----------------------------------------------------------------------

  useEffect(() => {
    connect()
    // Keep-alive ping every 30 s
    const interval = setInterval(ping, 30_000)
    return () => {
      clearInterval(interval)
      if (reconnectTimer.current) clearTimeout(reconnectTimer.current)
      wsRef.current?.close()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  return {
    status,
    messages,
    isGenerating,
    sendMessage,
    cancel,
    sendRaw,
    setMessages,
  }
}
