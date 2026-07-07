import { createContext, useCallback, useContext, useState, type ReactNode } from 'react'
import { CheckCircle, AlertTriangle, Info, X } from 'lucide-react'

type ToastKind = 'success' | 'error' | 'info'

interface Toast {
  id: number
  kind: ToastKind
  message: string
}

interface ToastCtx {
  toast: (message: string, kind?: ToastKind) => void
}

const Ctx = createContext<ToastCtx>({ toast: () => {} })

export function useToast() {
  return useContext(Ctx)
}

let counter = 0

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([])

  const remove = useCallback((id: number) => {
    setToasts((prev) => prev.filter((t) => t.id !== id))
  }, [])

  const toast = useCallback((message: string, kind: ToastKind = 'info') => {
    const id = ++counter
    setToasts((prev) => [...prev, { id, kind, message }])
    setTimeout(() => remove(id), 4000)
  }, [remove])

  const icons = {
    success: <CheckCircle size={16} className="text-emerald-400 shrink-0" />,
    error: <AlertTriangle size={16} className="text-red-400 shrink-0" />,
    info: <Info size={16} className="text-blue-400 shrink-0" />,
  }

  const bg = {
    success: 'border-emerald-800/50 bg-emerald-950/40',
    error: 'border-red-800/50 bg-red-950/40',
    info: 'border-zinc-800 bg-zinc-900/60',
  }

  return (
    <Ctx.Provider value={{ toast }}>
      {children}
      <div className="fixed top-14 sm:top-4 right-4 z-[60] flex flex-col gap-2 w-[calc(100%-2rem)] sm:w-auto sm:max-w-sm pointer-events-none">
        {toasts.map((t) => (
          <div
            key={t.id}
            className={`flex items-center gap-2.5 rounded-xl border px-4 py-3 text-sm text-zinc-200 shadow-2xl backdrop-blur-sm pointer-events-auto animate-[toastIn_0.2s_ease-out,toastOut_0.25s_ease-in_3.8s_forwards] ${bg[t.kind]}`}
          >
            {icons[t.kind]}
            <span className="flex-1">{t.message}</span>
            <button onClick={() => remove(t.id)} className="text-zinc-400 hover:text-zinc-100 shrink-0">
              <X size={14} />
            </button>
          </div>
        ))}
        <style>{`@keyframes toastIn { from { opacity: 0; transform: translateX(20px); } to { opacity: 1; transform: translateX(0); } } @keyframes toastOut { from { opacity: 1; transform: translateX(0); } to { opacity: 0; transform: translateX(20px); } }`}</style>
      </div>
    </Ctx.Provider>
  )
}
