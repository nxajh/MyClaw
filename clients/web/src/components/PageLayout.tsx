import type { ReactNode } from 'react'
import type { LucideIcon } from 'lucide-react'
import { Loader2 } from 'lucide-react'

// ── Page shell ───────────────────────────────────────────────────────────────

interface PageProps {
  icon: LucideIcon
  title: string
  meta?: string
  actions?: ReactNode
  children: ReactNode
}

export function Page({ icon: Icon, title, meta, actions, children }: PageProps) {
  return (
    <div className="flex flex-col h-full">
      <header className="border-b border-zinc-800 px-6 h-12 flex items-center justify-between shrink-0">
        <div className="flex items-center gap-2 text-sm font-medium text-zinc-300">
          <Icon size={15} className="text-zinc-500" />
          {title}
          {meta && <span className="text-zinc-600 font-normal">{meta}</span>}
        </div>
        {actions && <div className="flex items-center gap-2">{actions}</div>}
      </header>
      <div className="flex-1 overflow-y-auto">
        <div className="max-w-2xl mx-auto px-6 py-6 space-y-4">
          {children}
        </div>
      </div>
    </div>
  )
}

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
  return <p className="text-sm text-zinc-500 py-1">{children}</p>
}

// ── Shared input / button styles (exported as class strings) ─────────────────

export const inputCls =
  'rounded-xl border border-zinc-800 bg-zinc-900 px-4 py-2.5 text-sm text-zinc-100 placeholder-zinc-600 outline-none focus:border-zinc-700 transition-colors disabled:opacity-50 w-full'

export const btnPrimary =
  'flex items-center gap-1.5 rounded-xl bg-blue-600 hover:bg-blue-500 disabled:opacity-40 disabled:cursor-not-allowed px-4 py-2.5 text-sm font-medium transition-colors shrink-0'

export const btnGhost =
  'flex items-center gap-1.5 rounded-xl border border-zinc-800 hover:bg-zinc-800 disabled:opacity-40 disabled:cursor-not-allowed px-3 py-2 text-sm text-zinc-400 hover:text-zinc-200 transition-colors shrink-0'

export const btnDanger =
  'flex items-center gap-1.5 rounded-xl bg-red-600/80 hover:bg-red-600 disabled:opacity-40 disabled:cursor-not-allowed px-3 py-2 text-sm font-medium transition-colors shrink-0'
