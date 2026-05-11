import { useRef, useCallback, useEffect } from 'react'

// ---------------------------------------------------------------------------
// Generic API request builder
// ---------------------------------------------------------------------------

let apiCounter = 0

export function buildApiRequest(
  method: string,
  params?: Record<string, unknown>,
): Record<string, unknown> {
  const id = `api-${++apiCounter}-${Date.now()}`
  const req: Record<string, unknown> = { type: 'api', id, method }
  if (params) req.params = params
  return req
}

// ---------------------------------------------------------------------------
// Session-specific helpers
// ---------------------------------------------------------------------------

export const sessionApi = {
  list: () => buildApiRequest('sessions.list'),
  create: (name: string) => buildApiRequest('sessions.create', { name }),
  switch: (id: string) => buildApiRequest('sessions.switch', { id }),
  delete: (id: string) => buildApiRequest('sessions.delete', { id }),
}

// ---------------------------------------------------------------------------
// Hook to send API requests and await responses
// ---------------------------------------------------------------------------

export function useApi(
  sendRaw: (obj: Record<string, unknown>) => void,
  addMessageListener: (fn: (data: Record<string, unknown>) => void) => () => void,
) {
  const pending = useRef<Map<string, {
    resolve: (result: unknown) => void
    reject: (error: string) => void
  }>>(new Map())

  // Register a listener for api_response / api_error messages
  useEffect(() => {
    const unsubscribe = addMessageListener((data) => {
      if (data.type === 'api_response') {
        const id = data.id as string
        const entry = pending.current.get(id)
        if (entry) {
          pending.current.delete(id)
          entry.resolve(data.result)
        }
      } else if (data.type === 'api_error') {
        const id = data.id as string
        const entry = pending.current.get(id)
        if (entry) {
          pending.current.delete(id)
          entry.reject((data.error as string) || 'Unknown API error')
        }
      }
    })
    return unsubscribe
  }, [addMessageListener])

  const request = useCallback(
    (method: string, params?: Record<string, unknown>): Promise<unknown> => {
      return new Promise((resolve, reject) => {
        const req = buildApiRequest(method, params)
        pending.current.set(req.id as string, { resolve, reject })
        sendRaw(req)
        // Timeout after 10 s
        setTimeout(() => {
          if (pending.current.has(req.id as string)) {
            pending.current.delete(req.id as string)
            reject(new Error('API request timed out'))
          }
        }, 10_000)
      })
    },
    [sendRaw],
  )

  return { request }
}
