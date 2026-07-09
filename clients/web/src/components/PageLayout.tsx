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

/** Skeleton placeholders for list loading states */
export function SkeletonCards({ count = 4, cols = false }: { count?: number; cols?: boolean }) {
  return (
    <div className={cols ? 'grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-3' : 'space-y-2'}>
      {Array.from({ length: count }).map((_, i) => (
        <div
          key={i}
          className="rounded-2xl border border-zinc-800 bg-zinc-900/40 p-4 space-y-3"
        >
          <div className="flex items-center gap-2">
            <div className="skeleton-line w-16 h-3" />
            <div className="skeleton-line w-28 h-3" />
          </div>
          <div className="skeleton-line w-full h-2.5" />
          <div className="skeleton-line w-4/5 h-2.5" />
          <div className="skeleton-line w-1/3 h-2.5" />
        </div>
      ))}
    </div>
  )
}

export function EmptyState({
  children,
  icon,
  action,
}: {
  children: ReactNode
  icon?: ReactNode
  action?: ReactNode
}) {
  return (
    <div className="rounded-2xl border border-dashed border-zinc-800 bg-zinc-900/20 p-10 text-center space-y-3">
      {icon && <div className="flex justify-center text-zinc-600">{icon}</div>}
      <div className="text-sm text-zinc-500">{children}</div>
      {action && <div className="pt-1 flex justify-center">{action}</div>}
    </div>
  )
}

/** Sticky page header for management views */
export function PageHeader({
  title,
  subtitle,
  icon,
  actions,
}: {
  title: ReactNode
  subtitle?: ReactNode
  icon?: ReactNode
  actions?: ReactNode
}) {
  return (
    <div className="sticky top-0 z-10 -mx-3 sm:-mx-8 px-3 sm:px-8 py-3 sm:py-4 mb-1 border-b border-zinc-800/80 bg-zinc-950/85 backdrop-blur-md">
      <div className="flex flex-col sm:flex-row sm:items-center gap-3 justify-between">
        <div className="min-w-0">
          <h1 className="text-lg font-semibold tracking-tight text-zinc-100 flex items-center gap-2">
            {icon}
            {title}
          </h1>
          {subtitle && <p className="text-sm text-zinc-500 mt-0.5">{subtitle}</p>}
        </div>
        {actions && <div className="flex items-center gap-2 shrink-0 w-full sm:w-auto">{actions}</div>}
      </div>
    </div>
  )
}

// ── Shared surface / input / button styles ───────────────────────────────────

/** L1 panel surface */
export const panelCls =
  'rounded-2xl border border-zinc-800 bg-zinc-900/50 shadow-sm'

/** L1 interactive card hover */
export const cardHoverCls =
  'hover:bg-zinc-800/40 hover:border-zinc-700 transition-colors'

export const inputCls =
  'rounded-xl border border-zinc-800 bg-zinc-900 px-4 py-2.5 text-sm text-zinc-100 placeholder-zinc-600 outline-none focus:border-zinc-700 focus:ring-1 focus:ring-zinc-700/40 transition-colors disabled:opacity-50 w-full'

/** Full-width search field used across management pages */
export const searchInputCls =
  'w-full rounded-2xl border border-zinc-800 bg-zinc-900/40 pl-10 pr-4 py-2.5 text-sm text-zinc-200 placeholder-zinc-600 outline-none focus:border-zinc-700 focus:ring-1 focus:ring-zinc-700/30 transition-colors'

export const btnPrimary =
  'flex items-center gap-1.5 rounded-xl bg-zinc-100 hover:bg-white text-zinc-900 disabled:opacity-40 disabled:cursor-not-allowed px-4 py-2.5 text-sm font-medium transition-colors shrink-0'

export const btnGhost =
  'flex items-center gap-1.5 rounded-xl border border-zinc-800 hover:bg-zinc-800 disabled:opacity-40 disabled:cursor-not-allowed px-3 py-2 text-sm text-zinc-400 hover:text-zinc-200 transition-colors shrink-0'

export const btnDanger =
  'flex items-center gap-1.5 rounded-xl bg-red-600/80 hover:bg-red-600 disabled:opacity-40 disabled:cursor-not-allowed px-3 py-2 text-sm font-medium transition-colors shrink-0'
