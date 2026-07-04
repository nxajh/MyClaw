export type MemType = 'user' | 'feedback' | 'rule' | 'project' | 'reference'

export interface MemoryFile {
  name: string
  size: number
  mem_name?: string
  description?: string
  tags?: string[]
  type?: string
  link_count?: number
  backlink_count?: number
  created_at?: string
  updated_at?: string
}

export interface ParsedMeta {
  name: string
  type: MemType
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
    desc: 'Always-injected user preferences (highest scope)',
  },
  feedback: {
    bg: 'bg-red-500/5',
    border: 'border-red-500/20 hover:border-red-500/30',
    borderActive: 'border-red-500/50 ring-1 ring-red-500/20',
    text: 'text-red-400',
    badgeBg: 'bg-red-500/10 text-red-400 border border-red-500/20',
    label: '🎯 Feedback Correction',
    desc: 'Always-injected behavior corrections (strict constraints)',
  },
  rule: {
    bg: 'bg-emerald-500/5',
    border: 'border-emerald-500/20 hover:border-emerald-500/30',
    borderActive: 'border-emerald-500/50 ring-1 ring-emerald-500/20',
    text: 'text-emerald-400',
    badgeBg: 'bg-emerald-500/10 text-emerald-400 border border-emerald-500/20',
    label: '⚙️ Rule',
    desc: 'Always-injected operational rules and constraints',
  },
  project: {
    bg: 'bg-purple-500/5',
    border: 'border-purple-500/20 hover:border-purple-500/30',
    borderActive: 'border-purple-500/50 ring-1 ring-purple-500/20',
    text: 'text-purple-400',
    badgeBg: 'bg-purple-500/10 text-purple-400 border border-purple-500/20',
    label: '📂 Project Context',
    desc: 'On-demand workspace background knowledge',
  },
  reference: {
    bg: 'bg-amber-500/5',
    border: 'border-amber-500/20 hover:border-amber-500/30',
    borderActive: 'border-amber-500/50 ring-1 ring-amber-500/20',
    text: 'text-amber-400',
    badgeBg: 'bg-amber-500/10 text-amber-400 border border-amber-500/20',
    label: '📄 Reference Doc',
    desc: 'On-demand external APIs, specs, and definitions',
  },
}

export function getStyle(type?: string) {
  return typeStyles[type || ''] || typeStyles.project
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
  const emptyMeta: ParsedMeta = { name: '', type: 'project', description: '', tags: [], created_at: '', updated_at: '' }
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

  return { body, meta }
}
