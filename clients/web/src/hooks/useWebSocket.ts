import { useRef, useState, useCallback, useEffect } from 'react'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface UserMessage {
  role: 'user'
  content: string
  id: string
}

export interface ContentBlock { type: 'content'; text: string }
export interface ThinkingBlock { type: 'thinking'; text: string }
export interface ToolCallBlock {
  type: 'tool_call'
  id: string
  name: string
  args: Record<string, unknown>
  output?: string
}
export type MessageBlock = ContentBlock | ThinkingBlock | ToolCallBlock

export interface AssistantMessage {
  role: 'assistant'
  blocks: MessageBlock[]
  id: string
  done: boolean
}

export type ChatMessage = UserMessage | AssistantMessage

export type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'

export const AUTH_TOKEN_KEY = 'myclaw_auth_token'
const CLIENT_ID_KEY = 'myclaw_client_id'

export interface Attachment { name: string; content: string }
export interface SendOptions { images?: string[]; attachments?: Attachment[] }

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

let msgCounter = 0
function uid(): string {
  return `msg-${++msgCounter}-${Date.now()}`
}

// Stable per-browser identity so server-side sessions survive reconnects.
function getClientId(): string {
  try {
    let id = localStorage.getItem(CLIENT_ID_KEY)
    if (!id) {
      id = (crypto.randomUUID?.() ?? `c-${Date.now()}-${Math.random().toString(36).slice(2)}`)
      localStorage.setItem(CLIENT_ID_KEY, id)
    }
    return id
  } catch {
    return `c-${Date.now()}`
  }
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
        ws.send(JSON.stringify({ type: 'auth', token, client_id: getClientId() }))
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
            const blocks = [...last.blocks]
            const lb = blocks[blocks.length - 1]
            if (lb?.type === 'content') {
              blocks[blocks.length - 1] = { type: 'content', text: lb.text + delta }
            } else {
              blocks.push({ type: 'content', text: delta })
            }
            return [...prev.slice(0, -1), { ...last, blocks }]
          }
          const id = uid()
          currentAssistantId.current = id
          return [...prev, { role: 'assistant', blocks: [{ type: 'content', text: delta }], id, done: false }]
        })
        break
      }

      case 'thinking': {
        const delta = (data.delta as string) || ''
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            const blocks = [...last.blocks]
            const lb = blocks[blocks.length - 1]
            if (lb?.type === 'thinking') {
              blocks[blocks.length - 1] = { type: 'thinking', text: lb.text + delta }
            } else {
              blocks.push({ type: 'thinking', text: delta })
            }
            return [...prev.slice(0, -1), { ...last, blocks }]
          }
          const id = uid()
          currentAssistantId.current = id
          return [...prev, { role: 'assistant', blocks: [{ type: 'thinking', text: delta }], id, done: false }]
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
            const blocks: MessageBlock[] = [...last.blocks, { type: 'tool_call', id: callId, name, args }]
            return [...prev.slice(0, -1), { ...last, blocks }]
          }
          const id = uid()
          currentAssistantId.current = id
          return [...prev, { role: 'assistant', blocks: [{ type: 'tool_call', id: callId, name, args }], id, done: false }]
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
            const blocks = last.blocks.map((b): MessageBlock => {
              if (b.type !== 'tool_call') return b
              const matches = callId ? b.id === callId : (b.name === name && b.output === undefined)
              return matches ? { ...b, output } : b
            })
            return [...prev.slice(0, -1), { ...last, blocks }]
          }
          return prev
        })
        break
      }

      case 'done': {
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

      case 'error': {
        const message = (data.message as string) || 'Unknown error'
        if (authPending.current) {
          // Auth was rejected — show login overlay, stop reconnecting.
          authPending.current = false
          suppressReconnect.current = true
          setAuthFailed(true)
          break
        }
        setMessages((prev) => [
          ...prev,
          { role: 'assistant', blocks: [{ type: 'content', text: `⚠️ Error: ${message}` }], id: uid(), done: true },
        ])
        setIsGenerating(false)
        currentAssistantId.current = null
        break
      }

      case 'cancelled': {
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

      // Non-streamed server reply (slash-command output, ask_user prompts,
      // acks). Fills the pending assistant placeholder, or appends a new one.
      case 'message': {
        const content = (data.content as string) || ''
        if (!content) break
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            const blocks = [...last.blocks]
            const lb = blocks[blocks.length - 1]
            if (lb?.type === 'content') {
              blocks[blocks.length - 1] = { type: 'content', text: lb.text ? `${lb.text}\n\n${content}` : content }
            } else {
              blocks.push({ type: 'content', text: content })
            }
            return [...prev.slice(0, -1), { ...last, blocks, done: true }]
          }
          return [...prev, { role: 'assistant', blocks: [{ type: 'content', text: content }], id: uid(), done: true }]
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
    (content: string, opts?: SendOptions) => {
      const images = opts?.images ?? []
      const attachments = opts?.attachments ?? []
      // What the user sees in their bubble (server inlines file bodies itself).
      const hints: string[] = []
      if (images.length) hints.push(`🖼️ ${images.length} image${images.length > 1 ? 's' : ''}`)
      attachments.forEach((a) => hints.push(`📎 ${a.name}`))
      const display = [content, hints.join('  ')].filter(Boolean).join('\n\n')

      const userMsg: ChatMessage = { role: 'user', content: display || '(empty)', id: uid() }
      setMessages((prev) => [...prev, userMsg])
      // Prepare assistant placeholder
      const assistantId = uid()
      currentAssistantId.current = assistantId
      setMessages((prev) => [
        ...prev,
        { role: 'assistant', blocks: [], id: assistantId, done: false },
      ])
      setIsGenerating(true)
      const payload: Record<string, unknown> = { type: 'message', content }
      if (images.length) payload.image_base64 = images
      if (attachments.length) payload.attachments = attachments
      sendRaw(payload)
    },
    [sendRaw],
  )

  const cancel = useCallback(() => {
    sendRaw({ type: 'cancel' })
    setIsGenerating(false)
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
      ws.send(JSON.stringify({ type: 'auth', token, client_id: getClientId() }))
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
