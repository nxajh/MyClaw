import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter } from 'react-router-dom'
import 'katex/dist/katex.min.css'
import './index.css'
import App from './App'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <BrowserRouter basename={document.querySelector('base')?.getAttribute('href') ?? '/'}>
      <App />
    </BrowserRouter>
  </StrictMode>,
)
