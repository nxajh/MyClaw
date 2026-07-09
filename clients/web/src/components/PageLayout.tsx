import type { ReactNode } from 'react'
import { Loader2 } from 'lucide-react'

// ── Common UI atoms ──────────────────────────────────────────────────────────

export function ErrorBanner({ message }: { message: string }) {
  return (
    <div className="rounded-xl bg-red-900/25 border border-red-800/50 px-4 py-3 text-sm text-red-300">
      {message}
    </div>
  )
}

export function LoadingRow({ label = 'Loading…' }: { label?: string }) {
  return (
    <div className="flex items-center gap-2 text-sm text-zinc-500 py-1">
      <Loader2 size={13} className="animate-spin shrink-0" />
      {label}
    </div>
  )
}

export function EmptyState({ children }: { children: ReactNode }) {
  return (
    <div className="rounded-2xl border border-dashed border-zinc-800 p-8 text-center space-y-2">
      <p className="text-sm text-zinc-500">{children}</p>
    </div>
  )
}

// ── Shared input / button styles (exported as class strings) ─────────────────

export const inputCls =
  'rounded-xl border border-zinc-800 bg-zinc-900 px-4 py-2.5 text-sm text-zinc-100 placeholder-zinc-600 outline-none focus:border-zinc-700 focus:ring-1 focus:ring-zinc-700/40 transition-colors disabled:opacity-50 w-full'

/** Full-width search field used across management pages */
export const searchInputCls =
  'w-full rounded-2xl border border-zinc-800 bg-zinc-900/40 pl-10 pr-4 py-2.5 text-xs text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-700 focus:ring-1 focus:ring-zinc-700/30 transition-colors'

export const btnPrimary =
  'flex items-center gap-1.5 rounded-xl bg-zinc-100 hover:bg-white text-zinc-900 disabled:opacity-40 disabled:cursor-not-allowed px-4 py-2.5 text-sm font-medium transition-colors shrink-0'

export const btnGhost =
  'flex items-center gap-1.5 rounded-xl border border-zinc-800 hover:bg-zinc-800 disabled:opacity-40 disabled:cursor-not-allowed px-3 py-2 text-sm text-zinc-400 hover:text-zinc-200 transition-colors shrink-0'

export const btnDanger =
  'flex items-center gap-1.5 rounded-xl bg-red-600/80 hover:bg-red-600 disabled:opacity-40 disabled:cursor-not-allowed px-3 py-2 text-sm font-medium transition-colors shrink-0'
