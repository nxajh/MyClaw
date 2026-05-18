import { useState, type FormEvent } from 'react'
import { KeyRound } from 'lucide-react'

interface Props {
  onSubmit: (token: string) => void
  isRetry?: boolean
}

export default function LoginOverlay({ onSubmit, isRetry = false }: Props) {
  const [token, setToken] = useState('')

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault()
    if (token.trim()) onSubmit(token.trim())
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-zinc-950/80 backdrop-blur-sm">
      <div className="w-full max-w-sm mx-4 rounded-2xl border border-zinc-700/60 bg-zinc-900 p-8 shadow-2xl">
        <div className="flex flex-col items-center gap-2 mb-6">
          <div className="rounded-full bg-zinc-800 p-3">
            <KeyRound size={24} className="text-zinc-300" />
          </div>
          <h1 className="text-lg font-semibold text-zinc-100">MyClaw</h1>
          <p className="text-sm text-zinc-400 text-center">
            {isRetry
              ? 'Invalid token. Please check your access token and try again.'
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
            className="w-full rounded-xl border border-zinc-700/50 bg-zinc-800 px-4 py-3 text-sm text-zinc-100 placeholder-zinc-500 outline-none focus:border-zinc-600 focus:ring-1 focus:ring-zinc-600"
          />
          <button
            type="submit"
            disabled={!token.trim()}
            className="w-full rounded-xl bg-blue-600 hover:bg-blue-500 disabled:opacity-40 disabled:cursor-not-allowed px-4 py-3 text-sm font-medium transition"
          >
            Connect
          </button>
        </form>
      </div>
    </div>
  )
}
