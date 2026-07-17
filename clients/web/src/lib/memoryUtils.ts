export type MemType = 'user' | 'feedback' | 'rule' | 'project' | 'reference'
export type InjectPolicy = 'always' | 'search'

export interface MemoryFile {
  name: string
  size: number
  mem_name?: string
  description?: string
  tags?: string[]
  type?: string
  /** Injection policy: always → system-reminder every turn; search → on-demand only */
  inject?: InjectPolicy | string
  link_count?: number
  backlink_count?: number
  created_at?: string
  updated_at?: string
  content?: string
}

export interface ParsedMeta {
  name: string
  type: MemType
  inject: InjectPolicy
  description: string
  tags: string[]
  created_at: string
  updated_at: string
}

export interface ParsedFrontmatter {
  body: string
  meta: ParsedMeta
}

export const typeStyles: Record<string, {
  bg: string
  border: string
  borderActive: string
  text: string
  badgeBg: string
  label: string
  desc: string
}> = {
  user: {
    bg: 'bg-blue-500/5',
    border: 'border-blue-500/20 hover:border-blue-500/30',
    borderActive: 'border-blue-500/50 ring-1 ring-blue-500/20',
    text: 'text-blue-400',
    badgeBg: 'bg-blue-500/10 text-blue-400 border border-blue-500/20',
    label: '👤 User Preference',
    desc: 'Semantic category: user preferences and personal facts',
  },
  feedback: {
    bg: 'bg-red-500/5',
    border: 'border-red-500/20 hover:border-red-500/30',
    borderActive: 'border-red-500/50 ring-1 ring-red-500/20',
    text: 'text-red-400',
    badgeBg: 'bg-red-500/10 text-red-400 border border-red-500/20',
    label: '🎯 Feedback Correction',
    desc: 'Semantic category: corrections and alignment notes',
  },
  rule: {
    bg: 'bg-emerald-500/5',
    border: 'border-emerald-500/20 hover:border-emerald-500/30',
    borderActive: 'border-emerald-500/50 ring-1 ring-emerald-500/20',
    text: 'text-emerald-400',
    badgeBg: 'bg-emerald-500/10 text-emerald-400 border border-emerald-500/20',
    label: '⚙️ Rule',
    desc: 'Semantic category: operational rules and constraints',
  },
  project: {
    bg: 'bg-purple-500/5',
    border: 'border-purple-500/20 hover:border-purple-500/30',
    borderActive: 'border-purple-500/50 ring-1 ring-purple-500/20',
    text: 'text-purple-400',
    badgeBg: 'bg-purple-500/10 text-purple-400 border border-purple-500/20',
    label: '📂 Project Context',
    desc: 'Semantic category: workspace / project background knowledge',
  },
  reference: {
    bg: 'bg-amber-500/5',
    border: 'border-amber-500/20 hover:border-amber-500/30',
    borderActive: 'border-amber-500/50 ring-1 ring-amber-500/20',
    text: 'text-amber-400',
    badgeBg: 'bg-amber-500/10 text-amber-400 border border-amber-500/20',
    label: '📄 Reference Doc',
    desc: 'Semantic category: external APIs, specs, and definitions',
  },
}

/** Visual style for inject policy chips (orthogonal to type). */
export const injectStyles: Record<InjectPolicy, {
  badgeBg: string
  label: string
  desc: string
}> = {
  always: {
    badgeBg: 'bg-indigo-500/10 text-indigo-300 border border-indigo-500/25',
    label: 'Always',
    desc: 'Description injected into every conversation system-reminder',
  },
  search: {
    badgeBg: 'bg-zinc-800/80 text-zinc-400 border border-zinc-700/50',
    label: 'Search',
    desc: 'Available via search only — never auto-injected',
  },
}

export function getStyle(type?: string) {
  return typeStyles[type || ''] || typeStyles.project
}

export function normalizeInject(v?: string | null): InjectPolicy {
  return v === 'always' ? 'always' : 'search'
}

export function getInjectStyle(inject?: string | null) {
  return injectStyles[normalizeInject(inject)]
}

export function formatBytes(b: number) {
  if (b < 1024) return `${b} B`
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`
  return `${(b / (1024 * 1024)).toFixed(1)} MB`
}

const KNOWN_TYPES: MemType[] = ['user', 'feedback', 'rule', 'project', 'reference']

function asMemType(v: string): MemType {
  return (KNOWN_TYPES as string[]).includes(v) ? v as MemType : 'project'
}

export function parseFrontmatter(raw: string): ParsedFrontmatter {
  const emptyMeta: ParsedMeta = {
    name: '',
    type: 'project',
    inject: 'search',
    description: '',
    tags: [],
    created_at: '',
    updated_at: '',
  }
  const trimmed = raw.trim()
  if (!trimmed.startsWith('---')) return { body: raw, meta: emptyMeta }
  const nextDash = trimmed.indexOf('\n---', 3)
  if (nextDash === -1) return { body: raw, meta: emptyMeta }

  const yaml = trimmed.slice(3, nextDash)
  const body = trimmed.slice(nextDash + 4).trim()
  const meta: ParsedMeta = { ...emptyMeta }

  let descriptionVal = ''
  let summaryVal = ''
  let abstractVal = ''
  let injectVal = ''

  yaml.split('\n').forEach((line) => {
    const colon = line.indexOf(':')
    if (colon !== -1) {
      const k = line.slice(0, colon).trim()
      let v = line.slice(colon + 1).trim()
      // strip surrounding quotes
      if ((v.startsWith('"') && v.endsWith('"')) || (v.startsWith("'") && v.endsWith("'"))) {
        v = v.slice(1, -1)
      }
      if (k === 'name') meta.name = v
      else if (k === 'type') meta.type = asMemType(v)
      else if (k === 'inject') injectVal = v
      else if (k === 'description') descriptionVal = v
      else if (k === 'summary') summaryVal = v
      else if (k === 'abstract') abstractVal = v
      else if (k === 'created_at') meta.created_at = v
      else if (k === 'updated_at') meta.updated_at = v
      else if (k === 'tags') {
        const inner = v.startsWith('[') && v.endsWith(']') ? v.slice(1, -1) : v
        meta.tags = inner.split(',').map((x) => x.trim().replace(/^["']|["']$/g, '')).filter(Boolean)
      }
    }
  })

  meta.description = descriptionVal || summaryVal || abstractVal
  meta.inject = normalizeInject(injectVal)

  return { body, meta }
}
