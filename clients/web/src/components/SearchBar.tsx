import { useRef, useEffect } from 'react'
import { Search, ChevronDown, X } from 'lucide-react'

interface SearchBarProps {
  query: string
  setQuery: (q: string) => void
  matchCount: number
  matchIdx: number
  onPrev: () => void
  onNext: () => void
  onClose: () => void
}

export function SearchBar({ query, setQuery, matchCount, matchIdx, onPrev, onNext, onClose }: SearchBarProps) {
  const inputRef = useRef<HTMLInputElement>(null)
  useEffect(() => { inputRef.current?.focus() }, [])

  return (
    <div className="absolute top-2 left-1/2 -translate-x-1/2 z-20 flex items-center gap-2 px-3 py-2 rounded-xl bg-zinc-900 border border-zinc-700 shadow-2xl">
      <Search size={14} className="text-zinc-500 shrink-0" />
      <input
        ref={inputRef}
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') { e.preventDefault(); e.shiftKey ? onPrev() : onNext() }
          if (e.key === 'Escape') onClose()
        }}
        placeholder="Search messages…"
        className="bg-transparent text-sm text-zinc-100 placeholder-zinc-600 outline-none w-48 sm:w-64"
      />
      <span className="text-xs text-zinc-500 shrink-0">
        {matchCount > 0 ? `${matchIdx + 1}/${matchCount}` : query ? 'No results' : ''}
      </span>
      <button onClick={onPrev} disabled={matchCount === 0} className="p-1 rounded hover:bg-zinc-800 text-zinc-400 disabled:opacity-30"><ChevronDown size={14} className="rotate-180" /></button>
      <button onClick={onNext} disabled={matchCount === 0} className="p-1 rounded hover:bg-zinc-800 text-zinc-400 disabled:opacity-30"><ChevronDown size={14} /></button>
      <button onClick={onClose} className="p-1 rounded hover:bg-zinc-800 text-zinc-400"><X size={14} /></button>
    </div>
  )
}
