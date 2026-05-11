import { Construction } from 'lucide-react'
import { useWebSocketContext } from '../contexts/WebSocketContext'

interface Props {
  title: string
}

export default function Placeholder({ title }: Props) {
  const { status } = useWebSocketContext()

  return (
    <>
      <div className="flex-1 flex flex-col items-center justify-center gap-4 text-zinc-500">
        <Construction size={48} className="text-zinc-600" />
        <div className="text-center">
          <h2 className="text-xl font-semibold text-zinc-300 mb-1">{title}</h2>
          <p className="text-sm">Coming soon</p>
        </div>
      </div>
    </>
  )
}
