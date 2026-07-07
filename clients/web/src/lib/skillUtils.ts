// Simple YAML frontmatter parser for skill SKILL.md files.
// Skills have: name, description, keywords, version, when_to_use, argument_hint.

export interface SkillMeta {
  name: string
  description: string
  keywords: string[]
  version?: string
  when_to_use?: string
  argument_hint?: string
}

export interface ParsedSkill {
  body: string
  meta: SkillMeta
}

export function parseSkillFrontmatter(content: string): ParsedSkill {
  const trimmed = content.trimStart()
  if (!trimmed.startsWith('---')) {
    return { body: trimmed, meta: { name: '', description: '', keywords: [] } }
  }
  const afterOpen = trimmed.slice(3)
  const closeIdx = afterOpen.indexOf('\n---')
  if (closeIdx === -1) {
    return { body: trimmed, meta: { name: '', description: '', keywords: [] } }
  }

  const frontMatter = afterOpen.slice(0, closeIdx).trim()
  const body = afterOpen.slice(closeIdx + 4).trim()

  const extract = (key: string): string | undefined => {
    const re = new RegExp(`^${key}:\\s*(.*?)\\s*$`, 'm')
    const m = frontMatter.match(re)
    if (!m) return undefined
    let val = m[1].trim()
    if ((val.startsWith('"') && val.endsWith('"')) || (val.startsWith("'") && val.endsWith("'"))) {
      val = val.slice(1, -1)
    }
    return val
  }

  const extractList = (key: string): string[] => {
    const re = new RegExp(`^${key}:\\s*\\[(.*?)\\]\\s*$`, 'm')
    const m = frontMatter.match(re)
    if (!m) return []
    return m[1]
      .split(',')
      .map(s => s.trim().replace(/^["']|["']$/g, ''))
      .filter(Boolean)
  }

  const name = extract('name') || ''
  const description = extract('description') || ''
  const keywords = extractList('keywords')
  const version = extract('version')
  const when_to_use = extract('when_to_use')
  const argument_hint = extract('argument_hint')

  return {
    body,
    meta: { name, description, keywords, version, when_to_use, argument_hint },
  }
}
