import { useRef, useState, useCallback, useEffect } from 'react'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface FileRef { path: string; mime?: string; name?: string }

export interface UserMessage {
  role: 'user'
  content: string
  id: string
  /** Image file references from history (loaded on demand via file.read). */
  images?: FileRef[]
  /** Non-image file references from history (audio, video, PDF, etc.). */
  files?: FileRef[]
}

export interface ContentBlock { type: 'content'; text: string }
export interface ThinkingBlock { type: 'thinking'; text: string }
export interface ToolCallBlock {
  type: 'tool_call'
  id: string
  name: string
  args: Record<string, unknown>
  output?: string
  error?: boolean
  startedAt?: number
  completedAt?: number
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
export const CLIENT_ID_KEY = 'myclaw_client_id'

export interface Attachment { name: string; content: string }
export interface FileAttachment { data: string; mime_type: string; file_name: string }
export interface SendOptions { images?: string[]; attachments?: Attachment[]; files?: FileAttachment[] }

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

export interface LastCloseInfo {
  code?: number
  reason?: string
  wasClean?: boolean
  visibilityState?: string
  uptimeMs?: number | null
  lastPingAgoMs?: number | null
  lastPongAgoMs?: number | null
  lastMessageAgoMs?: number | null
}

export function useWebSocket() {
  const wsRef = useRef<WebSocket | null>(null)
  const reconnectTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const reconnectAttempts = useRef<number>(0)
  const pongTimeoutTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const connectedAtRef = useRef<number | null>(null)
  const lastPingAtRef = useRef<number | null>(null)
  const lastMessageAtRef = useRef<number | null>(null)
  const lastPongAtRef = useRef<number | null>(null)
  const [status, setStatus] = useState<ConnectionStatus>('disconnected')
  const [lastCloseInfo, setLastCloseInfo] = useState<LastCloseInfo | null>(null)
  const [messages, setMessages] = useState<ChatMessage[]>([])
  const [isGenerating, setIsGenerating] = useState(false)
  // true = auth was attempted and rejected by the server
  const [authFailed, setAuthFailed] = useState(false)
  // true = token submitted, waiting for server to validate
  const [authValidating, setAuthValidating] = useState(false)
  // Track active session so we can filter stale stream events after session switch
  const activeSessionIdRef = useRef<string | null>(null)
  const [activeSessionId, setActiveSessionIdState] = useState<string | null>(null)
  const clearInputTokenRef = useRef(0)
  const [clearInputToken, _setClearInputToken] = useState(0)
  const triggerClearInput = useCallback(() => {
    clearInputTokenRef.current += 1
    _setClearInputToken(clearInputTokenRef.current)
  }, [])
  const setActiveSessionId = useCallback((id: string | null) => {
    activeSessionIdRef.current = id
    setActiveSessionIdState(id)
  }, [])

  // Registry of listeners for raw server messages (used by useApi)
  const listenersRef = useRef<Set<(data: Record<string, unknown>) => void>>(new Set())

  // We keep a ref to the latest assistant message id so we can append chunks
  // without depending on state in the onmessage handler.
  const currentAssistantId = useRef<string | null>(null)
  // True while we are waiting for the server's auth_ok response.
  const authPending = useRef(false)
  // When true, suppress the automatic reconnect (e.g. after auth failure).
  const suppressReconnect = useRef(false)

  // ── Streaming buffer: batch rapid chunk updates ──────────────────────
  // Instead of calling setMessages on every token (causing O(n) array copy),
  // we accumulate deltas in a mutable ref and flush to state on rAF.
  const pendingDeltaRef = useRef<{ type: 'content' | 'thinking'; delta: string } | null>(null)
  const flushRafRef = useRef<number | null>(null)

  const flushPendingDelta = useCallback(() => {
    const pending = pendingDeltaRef.current
    if (!pending) return
    pendingDeltaRef.current = null
    const { type, delta } = pending
    setMessages((prev) => {
      const last = prev[prev.length - 1]
      if (last && last.role === 'assistant' && !last.done) {
        const blocks = [...last.blocks]
        const lb = blocks[blocks.length - 1]
        if (lb?.type === type) {
          blocks[blocks.length - 1] = { type, text: lb.text + delta }
        } else {
          blocks.push({ type, text: delta })
        }
        return [...prev.slice(0, -1), { ...last, blocks }]
      }
      const id = uid()
      currentAssistantId.current = id
      return [...prev, { role: 'assistant', blocks: [{ type, text: delta }], id, done: false }]
    })
  }, [])

  const enqueueDelta = useCallback((type: 'content' | 'thinking', delta: string) => {
    // Accumulate: if same type, concat; otherwise flush old and start new
    if (pendingDeltaRef.current && pendingDeltaRef.current.type === type) {
      pendingDeltaRef.current.delta += delta
    } else {
      // Different type or first delta — flush previous immediately, then start new
      if (pendingDeltaRef.current) {
        flushPendingDelta()
      }
      pendingDeltaRef.current = { type, delta }
    }
    if (!flushRafRef.current) {
      flushRafRef.current = requestAnimationFrame(() => {
        flushRafRef.current = null
        flushPendingDelta()
      })
    }
  }, [flushPendingDelta])

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
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) return
    if (ws && ws.readyState === WebSocket.CONNECTING) {
      return
    }

    try {
      const ws = new WebSocket(getWsUrl())
      wsRef.current = ws
      setStatus('connecting')

      ws.onopen = () => {
        if (wsRef.current !== ws) return
        connectedAtRef.current = Date.now()
        lastMessageAtRef.current = null
        lastPingAtRef.current = null
        lastPongAtRef.current = null
        // Send auth immediately; status stays 'connecting' until auth_ok.
        // If the server has no auth configured it responds auth_ok right away.
        authPending.current = true
        const token = localStorage.getItem(AUTH_TOKEN_KEY) ?? ''
        ws.send(JSON.stringify({ type: 'auth', token, client_id: getClientId() }))
      }

      ws.onclose = (event) => {
        if (wsRef.current !== ws) return
        const now = Date.now()
        const closeInfo: LastCloseInfo = {
          code: event.code,
          reason: event.reason || '',
          wasClean: event.wasClean,
          visibilityState: document.visibilityState,
          uptimeMs: connectedAtRef.current ? now - connectedAtRef.current : null,
          lastPingAgoMs: lastPingAtRef.current ? now - lastPingAtRef.current : null,
          lastPongAgoMs: lastPongAtRef.current ? now - lastPongAtRef.current : null,
          lastMessageAgoMs: lastMessageAtRef.current ? now - lastMessageAtRef.current : null,
        }
        console.info('[myclaw-webui] WebSocket closed', closeInfo)
        setLastCloseInfo(closeInfo)
        connectedAtRef.current = null
        setStatus('disconnected')
        // Mark any in-progress assistant message as done so the UI doesn't
        // show a stale "generating" animation after reconnect.
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            return [...prev.slice(0, -1), { ...last, done: true }]
          }
          return prev
        })
        setIsGenerating(false)
        currentAssistantId.current = null
        authPending.current = false
        if (pongTimeoutTimer.current) {
          clearTimeout(pongTimeoutTimer.current)
          pongTimeoutTimer.current = null
        }
        if (!suppressReconnect.current) {
          const delay = Math.min(1000 * Math.pow(2, reconnectAttempts.current), 30000)
          reconnectAttempts.current += 1
          reconnectTimer.current = setTimeout(connect, delay)
        }
      }

      ws.onerror = (event) => {
        if (wsRef.current !== ws) return
        console.warn('[myclaw-webui] WebSocket error', {
          eventType: event.type,
          readyState: ws.readyState,
          visibilityState: document.visibilityState,
        })
        // onclose will fire after this
      }

      ws.onmessage = (event) => {
        if (wsRef.current !== ws) return
        lastMessageAtRef.current = Date.now()
        if (pongTimeoutTimer.current) {
          clearTimeout(pongTimeoutTimer.current)
          pongTimeoutTimer.current = null
        }
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
      if (pongTimeoutTimer.current) {
        clearTimeout(pongTimeoutTimer.current)
        pongTimeoutTimer.current = null
      }
      const delay = Math.min(1000 * Math.pow(2, reconnectAttempts.current), 30000)
      reconnectAttempts.current += 1
      reconnectTimer.current = setTimeout(connect, delay)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // -----------------------------------------------------------------------
  // Server message handler
  // -----------------------------------------------------------------------

  const handleServerMessage = useCallback((data: Record<string, unknown>) => {
    const type = data.type as string

    // NOTE: session_id filtering intentionally removed — the server bus
    // key is a routing identifier, not the session UUID the frontend tracks.
    // The backend already ensures correct per-session event routing.

    switch (type) {
      case 'auth_ok': {
        authPending.current = false
        setAuthFailed(false)
        setAuthValidating(false)
        setStatus('connected')
        setLastCloseInfo(null)
        reconnectAttempts.current = 0
        break
      }

      case 'pong': {
        lastPongAtRef.current = Date.now()
        break
      }

      case 'chunk': {
        enqueueDelta('content', (data.delta as string) || '')
        break
      }

      case 'thinking': {
        enqueueDelta('thinking', (data.delta as string) || '')
        break
      }

      case 'tool_call': {
        const callId = (data.id as string) || uid()
        const name = (data.name as string) || 'unknown'
        const args = (data.args as Record<string, unknown>) || {}
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            const blocks: MessageBlock[] = [...last.blocks, { type: 'tool_call', id: callId, name, args, startedAt: Date.now() }]
            return [...prev.slice(0, -1), { ...last, blocks }]
          }
          const id = uid()
          currentAssistantId.current = id
          return [...prev, { role: 'assistant', blocks: [{ type: 'tool_call', id: callId, name, args, startedAt: Date.now() }], id, done: false }]
        })
        break
      }

      case 'tool_result': {
        const callId = (data.id as string) || ''
        const name = (data.name as string) || 'unknown'
        const output = (data.output as string) || ''
        const isError = !!(data.error as boolean)
        const now = Date.now()
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          if (last && last.role === 'assistant' && !last.done) {
            const blocks = last.blocks.map((b): MessageBlock => {
              if (b.type !== 'tool_call') return b
              const matches = callId ? b.id === callId : (b.name === name && b.output === undefined)
              return matches ? { ...b, output, error: isError, completedAt: now } : b
            })
            return [...prev.slice(0, -1), { ...last, blocks }]
          }
          return prev
        })
        break
      }

      case 'done': {
        flushPendingDelta()
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
        flushPendingDelta()
        const message = (data.message as string) || 'Unknown error'
        if (authPending.current) {
          // Auth was rejected — show login overlay, stop reconnecting.
          authPending.current = false
          suppressReconnect.current = true
          setAuthFailed(true)
          setAuthValidating(false)
          // Clear the bad token so reload shows login instead of silent retry.
          try { localStorage.removeItem(AUTH_TOKEN_KEY) } catch { /* ignore */ }
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
        flushPendingDelta()
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
        flushPendingDelta()
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

      // Inbound file from server (e.g. tool-generated files, channel forwards).
      case 'file': {
        const fileName = (data.file_name as string) || 'file'
        const fileMime = (data.mime_type as string) || 'application/octet-stream'
        const fileData = (data.data as string) || ''
        const caption = (data.caption as string) || ''
        if (fileData) {
          // Convert base64 to blob URL for display
          const bin = atob(fileData)
          const bytes = new Uint8Array(bin.length)
          for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
          const blob = new Blob([bytes], { type: fileMime })
          const blobUrl = URL.createObjectURL(blob)
          const isImage = fileMime.startsWith('image/')
          const marker = isImage
            ? `[image: ${fileName}](${blobUrl})`
            : `[file: ${fileName}](${blobUrl})`
          const text = caption ? `${caption}\n${marker}` : marker
          setMessages((prev) => {
            const last = prev[prev.length - 1]
            if (last && last.role === 'assistant' && !last.done) {
              const blocks = [...last.blocks]
              const lb = blocks[blocks.length - 1]
              if (lb?.type === 'content') {
                blocks[blocks.length - 1] = { type: 'content', text: lb.text ? `${lb.text}\n${text}` : text }
              } else {
                blocks.push({ type: 'content', text })
              }
              return [...prev.slice(0, -1), { ...last, blocks }]
            }
            return [...prev, { role: 'assistant', blocks: [{ type: 'content', text }], id: uid(), done: false }]
          })
        }
        break
      }

      // api_response / api_error are handled by external listeners (useApi)
      default:
        break
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [enqueueDelta, flushPendingDelta])

  // -----------------------------------------------------------------------
  // Send helpers
  // -----------------------------------------------------------------------

  const sendRaw = useCallback((obj: Record<string, unknown>): boolean => {
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(obj))
      return true
    }
    return false
  }, [])

  const sendMessage = useCallback(
    (content: string, opts?: SendOptions) => {
      const images = opts?.images ?? []
      const attachments = opts?.attachments ?? []
      const files = opts?.files ?? []
      // What the user sees in their bubble (server inlines file bodies itself).
      const hints: string[] = []
      if (images.length) hints.push(`🖼️ ${images.length} image${images.length > 1 ? 's' : ''}`)
      attachments.forEach((a) => hints.push(`📎 ${a.name}`))
      files.forEach((f) => hints.push(`📎 ${f.file_name}`))
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
      if (files.length) payload.files_base64 = files
      if (!sendRaw(payload)) {
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          const errorMsg: AssistantMessage = { role: 'assistant', blocks: [{ type: 'content', text: '⚠️ Message not sent — disconnected. Reconnect and try again.' }], id: assistantId, done: true }
          if (last && last.role === 'assistant' && last.id === assistantId) {
            return [...prev.slice(0, -1), errorMsg]
          }
          return [...prev, errorMsg]
        })
        setIsGenerating(false)
        currentAssistantId.current = null
      }
    },
    [sendRaw],
  )

  const cancel = useCallback(() => {
    sendRaw({ type: 'cancel' })
    flushPendingDelta()
    setIsGenerating(false)
    setMessages((prev) => {
      const last = prev[prev.length - 1]
      if (last && last.role === 'assistant' && !last.done) {
        return [...prev.slice(0, -1), { ...last, done: true }]
      }
      return prev
    })
    currentAssistantId.current = null
  }, [sendRaw, flushPendingDelta])

  const reconnectNow = useCallback(() => {
    suppressReconnect.current = false
    reconnectAttempts.current = 0
    if (reconnectTimer.current) {
      clearTimeout(reconnectTimer.current)
      reconnectTimer.current = null
    }
    if (pongTimeoutTimer.current) {
      clearTimeout(pongTimeoutTimer.current)
      pongTimeoutTimer.current = null
    }
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) {
      try { ws.close(4000, 'manual reconnect') } catch { /* ignore */ }
    }
    if (ws && ws.readyState === WebSocket.CONNECTING) {
      try { ws.close() } catch { /* ignore */ }
    }
    wsRef.current = null
    setStatus('connecting')
    connect()
  }, [connect])

  const ping = useCallback(() => {
    if (wsRef.current && wsRef.current.readyState === WebSocket.OPEN) {
      if (pongTimeoutTimer.current) {
        clearTimeout(pongTimeoutTimer.current)
      }
      lastPingAtRef.current = Date.now()
      pongTimeoutTimer.current = setTimeout(() => {
        if (wsRef.current) {
          console.warn('[myclaw-webui] WebSocket pong timeout, closing socket', {
            readyState: wsRef.current.readyState,
            visibilityState: document.visibilityState,
            lastPingAgoMs: lastPingAtRef.current ? Date.now() - lastPingAtRef.current : null,
            lastPongAgoMs: lastPongAtRef.current ? Date.now() - lastPongAtRef.current : null,
            lastMessageAgoMs: lastMessageAtRef.current ? Date.now() - lastMessageAtRef.current : null,
          })
          wsRef.current.close()
        }
      }, 10000)
      sendRaw({ type: 'ping' })
    }
  }, [sendRaw])

  const submitToken = useCallback((token: string, clientId: string) => {
    if (token) {
      localStorage.setItem(AUTH_TOKEN_KEY, token)
    } else {
      localStorage.removeItem(AUTH_TOKEN_KEY)
    }
    if (clientId) {
      try { localStorage.setItem(CLIENT_ID_KEY, clientId) } catch { /* ignore */ }
    }
    suppressReconnect.current = false
    setAuthFailed(false)
    setAuthValidating(true)
    // Close existing connection so we reconnect fresh with the new token.
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) {
      // Already connected — send auth directly.
      authPending.current = true
      ws.send(JSON.stringify({ type: 'auth', token, client_id: getClientId() }))
    } else {
      // If ws exists but is closing/closed, null it out so connect() proceeds.
      if (ws && ws.readyState !== WebSocket.OPEN) {
        wsRef.current = null
      }
      reconnectAttempts.current = 0
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

    // When the tab comes back to the foreground, reconnect immediately
    // instead of waiting for the next throttled timer tick.
    const onVisibilityChange = () => {
      if (document.visibilityState === 'visible') {
        const ws = wsRef.current
        if (!ws || ws.readyState !== WebSocket.OPEN) {
          // Reset backoff so we reconnect instantly.
          reconnectAttempts.current = 0
          if (reconnectTimer.current) {
            clearTimeout(reconnectTimer.current)
            reconnectTimer.current = null
          }
          if (pongTimeoutTimer.current) {
            clearTimeout(pongTimeoutTimer.current)
            pongTimeoutTimer.current = null
          }
          connect()
        }
      }
    }
    document.addEventListener('visibilitychange', onVisibilityChange)
    window.addEventListener('focus', onVisibilityChange)

    return () => {
      clearInterval(interval)
      document.removeEventListener('visibilitychange', onVisibilityChange)
      window.removeEventListener('focus', onVisibilityChange)
      if (reconnectTimer.current) clearTimeout(reconnectTimer.current)
      if (pongTimeoutTimer.current) clearTimeout(pongTimeoutTimer.current)
      if (flushRafRef.current) cancelAnimationFrame(flushRafRef.current)
      flushPendingDelta()
      wsRef.current?.close()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  return {
    status,
    lastCloseInfo,
    messages,
    isGenerating,
    authFailed,
    authValidating,
    activeSessionId,
    setActiveSessionId,
    submitToken,
    sendMessage,
    cancel,
    reconnectNow,
    sendRaw,
    setMessages,
    addMessageListener,
    clearInputToken,
    triggerClearInput,
  }
}
