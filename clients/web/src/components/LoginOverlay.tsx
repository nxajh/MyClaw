import { useState, useEffect, type FormEvent } from 'react'
import { KeyRound, Loader2 } from 'lucide-react'
import { CLIENT_ID_KEY } from '../hooks/useWebSocket'

interface Props {
  onSubmit: (token: string, clientId: string) => void
  isRetry?: boolean
  /** true while the server is validating the submitted token */
  isConnecting?: boolean
}

export default function LoginOverlay({ onSubmit, isRetry = false, isConnecting = false }: Props) {
  const [token, setToken] = useState('')
  const [clientId, setClientId] = useState(() => {
    try { return localStorage.getItem(CLIENT_ID_KEY) ?? '' } catch { return '' }
  })

  // When a retry occurs (auth failed), clear the input so the user can re-enter.
  useEffect(() => {
    if (isRetry) {
      setToken('')
    }
  }, [isRetry])

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault()
    if (token.trim() && clientId.trim() && !isConnecting) onSubmit(token.trim(), clientId.trim())
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-zinc-950/80 backdrop-blur-sm">
      <div className="w-full max-w-sm mx-4 rounded-2xl border border-zinc-700/60 bg-zinc-900 p-8 shadow-2xl">
        <div className="flex flex-col items-center gap-2 mb-6">
          <div className={`rounded-full p-3 ${isRetry ? 'bg-red-900/40' : 'bg-zinc-800'}`}>
            {isConnecting
              ? <Loader2 size={24} className="text-zinc-300 animate-spin" />
              : <KeyRound size={24} className={isRetry ? 'text-red-400' : 'text-zinc-300'} />}
          </div>
          <h1 className="text-lg font-semibold text-zinc-100">MyClaw</h1>
          <p className="text-sm text-center" style={{ color: isRetry ? '#f87171' : '#a1a1aa' }}>
            {isRetry
              ? 'Token validation failed — please re-enter your access token.'
              : isConnecting
                ? 'Validating token…'
                : 'Enter your access token to continue.'}
          </p>
        </div>

        <form onSubmit={handleSubmit} className="flex flex-col gap-3">
          <input
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            placeholder="Access token"
            autoFocus
            disabled={isConnecting}
            className="w-full rounded-xl border border-zinc-700/50 bg-zinc-800 px-4 py-3 text-sm text-zinc-100 placeholder-zinc-500 outline-none focus:border-zinc-600 focus:ring-1 focus:ring-zinc-600 disabled:opacity-50"
          />
          <input
            type="text"
            value={clientId}
            onChange={(e) => setClientId(e.target.value)}
            placeholder="Client ID"
            disabled={isConnecting}
            className="w-full rounded-xl border border-zinc-700/50 bg-zinc-800 px-4 py-3 text-sm text-zinc-100 placeholder-zinc-500 outline-none focus:border-zinc-600 focus:ring-1 focus:ring-zinc-600 disabled:opacity-50"
          />
          <button
            type="submit"
            disabled={!token.trim() || !clientId.trim() || isConnecting}
            className="w-full rounded-xl bg-blue-600 hover:bg-blue-500 disabled:opacity-40 disabled:cursor-not-allowed px-4 py-3 text-sm font-medium transition flex items-center justify-center gap-2"
          >
            {isConnecting && <Loader2 size={14} className="animate-spin" />}
            {isConnecting ? 'Connecting…' : 'Connect'}
          </button>
        </form>
      </div>
    </div>
  )
}
