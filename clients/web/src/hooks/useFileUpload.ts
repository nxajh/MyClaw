import { useState, useCallback } from 'react'

const IMAGE_MAX = 10 * 1024 * 1024
const TEXT_MAX = 256 * 1024
const BINARY_MAX = 150 * 1024 * 1024

export interface PickedImage {
  name: string
  dataUrl: string
}

export interface PickedText {
  name: string
  content: string
}

export interface PickedBinary {
  name: string
  mime: string
  dataUrl: string
}

export function useFileUpload() {
  const [images, setImages] = useState<PickedImage[]>([])
  const [texts, setTexts] = useState<PickedText[]>([])
  const [binaries, setBinaries] = useState<PickedBinary[]>([])

  const isTextMime = (t: string) => t.startsWith('text/') || ['application/json', 'application/xml', 'application/javascript', 'application/x-sh'].includes(t)
  const isTextExt = (name: string) => /\.(txt|md|json|js|ts|tsx|jsx|py|rs|go|java|c|cpp|h|hpp|css|html|yml|yaml|toml|sh|log|csv|xml|sql|rb|php|swift|kt|scala|lua|r|jl|m|mm|vue|svelte|astro|mdx|conf|cfg|ini|env|dockerfile|makefile)$/i.test(name)

  const handleFiles = useCallback((files: FileList | null, setNote: (msg: string | null) => void, clearNote: () => void) => {
    if (!files) return
    setNote(null)
    let hasNote = false
    Array.from(files).forEach((file) => {
      const isImage = file.type.startsWith('image/')
      const isText = isTextMime(file.type) || isTextExt(file.name)
      if (isImage) {
        if (file.size > IMAGE_MAX) { setNote(`${file.name} skipped (image > 10MB)`); hasNote = true; return }
        const reader = new FileReader()
        reader.onload = () => setImages((p) => [...p, { name: file.name, dataUrl: reader.result as string }])
        reader.readAsDataURL(file)
      } else if (isText) {
        if (file.size > TEXT_MAX) { setNote(`${file.name} skipped (file > 256KB)`); hasNote = true; return }
        const reader = new FileReader()
        reader.onload = () => setTexts((p) => [...p, { name: file.name, content: reader.result as string }])
        reader.readAsText(file)
      } else {
        if (file.size > BINARY_MAX) { setNote(`${file.name} skipped (file > ${Math.round(BINARY_MAX / 1024 / 1024)}MB)`); hasNote = true; return }
        const reader = new FileReader()
        reader.onload = () => setBinaries((p) => [...p, { name: file.name, mime: file.type || 'application/octet-stream', dataUrl: reader.result as string }])
        reader.readAsDataURL(file)
      }
    })
    if (hasNote) clearNote()
  }, [])

  const clearFiles = useCallback(() => {
    setImages([])
    setTexts([])
    setBinaries([])
  }, [])

  return {
    images,
    texts,
    binaries,
    setImages,
    setTexts,
    setBinaries,
    handleFiles,
    clearFiles,
  }
}
