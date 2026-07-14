import type { ReactNode, MouseEvent } from 'react'
import { Loader2, Search } from 'lucide-react'

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

/** Skeleton placeholders for browse lists */
export function SkeletonCards({ count = 4, cols = false }: { count?: number; cols?: boolean }) {
  return (
    <div className={cols ? 'grid grid-cols-1 md:grid-cols-2 gap-2' : 'space-y-2'}>
      {Array.from({ length: count }).map((_, i) => (
        <div
          key={i}
          className="rounded-2xl border border-zinc-800 bg-zinc-900/40 px-4 py-3 space-y-2"
        >
          <div className="flex items-center gap-3">
            <div className="skeleton-line w-8 h-8 !rounded-lg shrink-0" />
            <div className="flex-1 space-y-2 min-w-0">
              <div className="skeleton-line w-1/3 h-3" />
              <div className="skeleton-line w-2/3 h-2.5" />
            </div>
            <div className="skeleton-line w-12 h-2.5 shrink-0" />
          </div>
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

/** Full-page scroll shell used by management views */
export function PageShell({ children, className = '' }: { children: ReactNode; className?: string }) {
  return (
    <div className={`flex flex-col h-full bg-zinc-950 ${className}`}>
      <div className="flex-1 overflow-y-auto">
        <div className="px-3 sm:px-8 py-4 sm:py-6 space-y-4 page-enter">
          {children}
        </div>
      </div>
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

/** Shared search field with leading icon */
export function SearchField({
  value,
  onChange,
  placeholder = 'Search…',
  className = '',
}: {
  value: string
  onChange: (value: string) => void
  placeholder?: string
  className?: string
}) {
  return (
    <div className={`relative ${className}`}>
      <Search size={14} className="absolute left-3.5 top-1/2 -translate-y-1/2 text-zinc-500 pointer-events-none" />
      <input
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className={searchInputCls}
      />
    </div>
  )
}

/**
 * Unified browse entity row used by Sessions / Memory / Skills.
 * density:
 *  - dense: single-line primary + compact meta (Sessions)
 *  - comfortable: title + description + tags (Memory/Skills)
 */
export function EntityListItem({
  leading,
  title,
  subtitle,
  description,
  tags,
  meta,
  actions,
  active = false,
  density = 'comfortable',
  onClick,
  className = '',
}: {
  leading?: ReactNode
  title: ReactNode
  subtitle?: ReactNode
  description?: ReactNode
  tags?: ReactNode
  meta?: ReactNode
  actions?: ReactNode
  active?: boolean
  density?: 'dense' | 'comfortable'
  onClick?: () => void
  className?: string
}) {
  const interactive = typeof onClick === 'function'
  const base = active
    ? 'border-zinc-700/70 bg-zinc-800/60 shadow-sm'
    : `${panelCls} ${interactive ? cardHoverCls : ''}`

  const handleActionsClick = (e: MouseEvent) => {
    e.stopPropagation()
  }

  const body = (
    <>
      {leading && <div className="shrink-0 mt-0.5">{leading}</div>}
      <div className="flex-1 min-w-0 space-y-1">
        <div className="flex items-start gap-2 min-w-0">
          <div className="min-w-0 flex-1">
            <div className={`text-sm font-medium truncate ${active ? 'text-zinc-100' : 'text-zinc-200'}`}>
              {title}
            </div>
            {subtitle && (
              <div className="text-xs text-zinc-500 mt-0.5 truncate">{subtitle}</div>
            )}
          </div>
          {meta && (
            <div className="shrink-0 text-xs text-zinc-500 font-mono pt-0.5">{meta}</div>
          )}
        </div>
        {density === 'comfortable' && description && (
          <p className="text-sm text-zinc-400 leading-relaxed line-clamp-2">{description}</p>
        )}
        {density === 'comfortable' && tags && (
          <div className="flex flex-wrap gap-1.5 pt-0.5">{tags}</div>
        )}
      </div>
      {actions && (
        <div className="shrink-0 flex items-center gap-1 self-center" onClick={handleActionsClick}>
          {actions}
        </div>
      )}
    </>
  )

  const cls = `group flex items-start gap-3 rounded-2xl border px-4 ${density === 'dense' ? 'py-3' : 'py-3.5 min-h-[72px]'} w-full text-left transition-colors ${base} ${interactive ? 'cursor-pointer' : ''} ${className}`

  if (interactive) {
    return (
      <button type="button" onClick={onClick} className={cls}>
        {body}
      </button>
    )
  }

  return <div className={cls}>{body}</div>
}

/** Small mono/chip meta pill for entity rows */
export function EntityMetaChip({ children }: { children: ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1 text-xs text-zinc-500 bg-zinc-950/60 px-2 py-0.5 rounded-md border border-zinc-800">
      {children}
    </span>
  )
}

// ── Shared surface / input / button styles ───────────────────────────────────

/** L1 panel surface */
export const panelCls =
  'rounded-2xl border border-zinc-800 bg-zinc-900/50 shadow-sm'

/** L1 interactive card hover */
export const cardHoverCls =
  'hover:bg-zinc-800/40 hover:border-zinc-700 transition-colors'

/** Shared interactive list/card treatment */
export const listItemCls =
  `w-full text-left ${panelCls} ${cardHoverCls}`

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
