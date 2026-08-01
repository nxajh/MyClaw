// Lazy-load KaTeX CSS only when math content is detected
let katexCssLoaded = false
export function ensureKatexCss() {
  if (katexCssLoaded) return Promise.resolve()
  katexCssLoaded = true
  return import('katex/dist/katex.min.css')
}

/** Detect $ / $$ or LaTeX \( \) / \[ \] math delimiters (used before/after normalize). */
export const hasMath = (text: string) =>
  /\$\$[\s\S]+?\$\$|\$[^$\n]+?\$|\\\([\s\S]+?\\\)|\\\[[\s\S]+?\\\]/.test(text)

// Lazy-load rehype-katex only when needed
export let RehypeKatex: any = null
export async function loadRehypeKatex() {
  if (!RehypeKatex) {
    const mod = await import('rehype-katex')
    RehypeKatex = mod.default
  }
  return RehypeKatex
}

/**
 * Convert LaTeX math delimiters to remark-math form:
 *   \[...\] → $$...$$  (display)
 *   \(...\) → $...$    (inline)
 * Skips fenced code blocks and inline code so examples stay literal.
 * Display-only; does not mutate stored message text.
 */
export function normalizeMathDelimiters(text: string): string {
  type Seg = { kind: 'code' | 'prose'; text: string }
  const segs: Seg[] = []
  const lines = text.split('\n')
  let buf: string[] = []
  let inFence = false
  let fenceMarker = ''
  let fenceLen = 0

  const flush = (kind: 'code' | 'prose') => {
    if (buf.length === 0) return
    segs.push({ kind, text: buf.join('\n') })
    buf = []
  }

  for (const line of lines) {
    const m = line.match(/^(\s{0,3})(`{3,}|~{3,})(.*)$/)
    if (m) {
      const marker = m[2][0]
      const len = m[2].length
      const after = m[3]
      if (!inFence) {
        flush('prose')
        inFence = true
        fenceMarker = marker
        fenceLen = len
        buf = [line]
        continue
      }
      if (marker === fenceMarker && len >= fenceLen && after.trim() === '') {
        buf.push(line)
        flush('code')
        inFence = false
        continue
      }
    }
    buf.push(line)
  }
  flush(inFence ? 'code' : 'prose')

  return segs
    .map((seg) => (seg.kind === 'code' ? seg.text : normalizeMathInProse(seg.text)))
    .join('\n')
}

export function normalizeMathInProse(text: string): string {
  const stash: string[] = []
  // Protect inline code spans (`` `...` `` / `...`)
  const masked = text.replace(/(?<!\\)(`+)((?:(?!\1)[\s\S])*?)\1/g, (m) => {
    const i = stash.length
    stash.push(m)
    return `\uE000${i}\uE001`
  })

  // \[...\] display first (allow newlines); skip if backslash-escaped
  let out = masked.replace(/(?<!\\)\\\[([\s\S]*?)(?<!\\)\\\]/g, (_m, body: string) => `$$${body}$$`)
  // \(...\) inline
  out = out.replace(/(?<!\\)\\\(([\s\S]*?)(?<!\\)\\\)/g, (_m, body: string) => `$${body}$`)

  return out.replace(/\uE000(\d+)\uE001/g, (_m, i: string) => stash[Number(i)] ?? '')
}

/// Auto-close unclosed fenced code blocks so the rest of the document
/// isn't swallowed into a code block per CommonMark spec.
export function closeUnclosedFences(text: string): string {
  let inFence = false
  let fenceMarker = ''
  let fenceLen = 0
  for (const line of text.split('\n')) {
    const m = line.match(/^\s{0,3}(`{3,}|~{3,})/)
    if (!m) continue
    const marker = m[1][0]
    const len = m[1].length
    if (!inFence) {
      inFence = true
      fenceMarker = marker
      fenceLen = len
    } else if (marker === fenceMarker && len >= fenceLen && line.slice(m[0].length).trim() === '') {
      inFence = false
    }
  }
  return inFence ? text + '\n' + fenceMarker.repeat(Math.max(fenceLen, 3)) + '\n' : text
}
