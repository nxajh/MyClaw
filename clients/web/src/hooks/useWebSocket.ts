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

export const AUTH_TOKEN_KEY = 'myclaw_auth_token'
const MESSAGES_KEY = 'myclaw_messages'

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

function loadPersistedMessages(): ChatMessage[] {
  try {
    const raw = localStorage.getItem(MESSAGES_KEY)
    if (!raw) return []
    return JSON.parse(raw) as ChatMessage[]
  } catch {
    return []
  }
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

export function useWebSocket() {
  const wsRef = useRef<WebSocket | null>(null)
  const reconnectTimer = useRef<ReturnType<typeof setTimeout>>(null)
  const [status, setStatus] = useState<ConnectionStatus>('disconnected')
  const [messages, setMessages] = useState<ChatMessage[]>(loadPersistedMessages)
  const [isGenerating, setIsGenerating] = useState(false)
  // true = auth was attempted and rejected by the server
  const [authFailed, setAuthFailed] = useState(false)

  // Registry of listeners for raw server messages (used by useApi)
  const listenersRef = useRef<Set<(data: Record<string, unknown>) => void>>(new Set())

  // We keep a ref to the latest assistant message id so we can append chunks
  // without depending on state in the onmessage handler.
  const currentAssistantId = useRef<string | null>(null)
  // True while we are waiting for the server's auth_ok response.
  const authPending = useRef(false)
  // When true, suppress the automatic reconnect (e.g. after auth failure).
  const suppressReconnect = useRef(false)

  // Persist messages to localStorage whenever they change.
  useEffect(() => {
    try {
      localStorage.setItem(MESSAGES_KEY, JSON.stringify(messages))
    } catch { /* quota exceeded — ignore */ }
  }, [messages])

  // -----------------------------------------------------------------------
  // Listener management
  // -----------------------------------------------------------------------

  const addMessageListener = useCallback((fn: (data: Record<string, unknown>) => void) => {
    listenersRef.current.add(fn)
    return () => { listenersRef.current.delete(fn) }
  }, [])

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
        // Send auth immediately; status stays 'connecting' until auth_ok.
        // If the server has no auth configured it responds auth_ok right away.
        authPending.current = true
        const token = localStorage.getItem(AUTH_TOKEN_KEY) ?? ''
        ws.send(JSON.stringify({ type: 'auth', token }))
      }

      ws.onclose = () => {
        setStatus('disconnected')
        setIsGenerating(false)
        currentAssistantId.current = null
        authPending.current = false
        if (!suppressReconnect.current) {
          reconnectTimer.current = setTimeout(connect, 2000)
        }
      }

      ws.onerror = () => {
        // onclose will fire after this
      }

      ws.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data as string) as Record<string, unknown>

          // Notify external listeners first (e.g. useApi)
          listenersRef.current.forEach((fn) => {
            try { fn(data) } catch { /* swallow listener errors */ }
          })

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
      case 'auth_ok': {
        authPending.current = false
        setAuthFailed(false)
        setStatus('connected')
        break
      }

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
        const callId = (data.id as string) || uid()
        const name = (data.name as string) || 'unknown'
        const args = (data.args as Record<string, unknown>) || {}
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            return [
              ...prev.slice(0, -1),
              { ...last, toolCalls: [...last.toolCalls, { name, args, id: callId }] },
            ]
          }
          const id = uid()
          currentAssistantId.current = id
          return [...prev, { role: 'assistant', content: '', toolCalls: [{ name, args, id: callId }], id, done: false }]
        })
        break
      }

      case 'tool_result': {
        const callId = (data.id as string) || ''
        const name = (data.name as string) || 'unknown'
        const output = (data.output as string) || ''
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            const newToolCalls = last.toolCalls.map((tc) => {
              // Match by server-provided call ID; fall back to first pending by name.
              const matches = callId ? tc.id === callId : (tc.name === name && tc.output === undefined)
              return matches ? { ...tc, output } : tc
            })
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
        if (authPending.current) {
          // Auth was rejected — show login overlay, stop reconnecting.
          authPending.current = false
          suppressReconnect.current = true
          setAuthFailed(true)
          // The server closes the connection after sending this error; status
          // will transition to 'disconnected' via onclose.
          break
        }
        setMessages((prev) => [
          ...prev,
          { role: 'assistant', content: `⚠️ Error: ${message}`, toolCalls: [], id: uid(), done: true },
        ])
        setIsGenerating(false)
        currentAssistantId.current = null
        break
      }

      case 'cancelled': {
        // Server confirmed cancellation — mark the current message done and stop generating.
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            return [...prev.slice(0, -1), { ...last, done: true }]
          }
          return prev
        })
        setIsGenerating(false)
        currentAssistantId.current = null
        break
      }

      // api_response / api_error are handled by external listeners (useApi)
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

  const submitToken = useCallback((token: string) => {
    if (token) {
      localStorage.setItem(AUTH_TOKEN_KEY, token)
    } else {
      localStorage.removeItem(AUTH_TOKEN_KEY)
    }
    suppressReconnect.current = false
    setAuthFailed(false)
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) {
      // Already connected — send auth directly.
      authPending.current = true
      ws.send(JSON.stringify({ type: 'auth', token }))
    } else {
      // Reconnect; onopen will send auth automatically.
      connect()
    }
  }, [connect])

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
    authFailed,
    submitToken,
    sendMessage,
    cancel,
    sendRaw,
    setMessages,
    addMessageListener,
  }
}
