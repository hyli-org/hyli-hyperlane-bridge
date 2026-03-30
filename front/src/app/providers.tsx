'use client'

import { WagmiProvider } from 'wagmi'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createWagmiConfig } from '@/config/wagmi'
import {
  DEFAULT_HYLI_INDEXER_URL,
  DEFAULT_HYLI_RPC_URL,
  RuntimeConfigProvider,
  type PublicRuntimeConfig,
} from '@/lib/runtimeConfig'
import { useEffect, useState } from 'react'

export function Providers({ children }: { children: React.ReactNode }) {
  const [queryClient] = useState(() => new QueryClient())
  const [runtimeConfig, setRuntimeConfig] = useState<PublicRuntimeConfig | null>(null)
  const [wagmiConfig, setWagmiConfig] = useState(() => createWagmiConfig(DEFAULT_HYLI_RPC_URL))

  useEffect(() => {
    let cancelled = false

    ;(async () => {
      try {
        const response = await fetch('/api/public-config', { cache: 'no-store' })
        if (!response.ok) throw new Error(`Failed to load runtime config (${response.status})`)

        const config = (await response.json()) as PublicRuntimeConfig
        if (cancelled) return

        setRuntimeConfig(config)
        setWagmiConfig(createWagmiConfig(config.hyliRpcUrl))
      } catch {
        if (cancelled) return

        const fallbackConfig: PublicRuntimeConfig = {
          hyliRpcUrl: DEFAULT_HYLI_RPC_URL,
          hyliIndexerUrl: DEFAULT_HYLI_INDEXER_URL,
        }

        setRuntimeConfig(fallbackConfig)
        setWagmiConfig(createWagmiConfig(fallbackConfig.hyliRpcUrl))
      }
    })()

    return () => {
      cancelled = true
    }
  }, [])

  if (!runtimeConfig) {
    return (
      <div className="flex min-h-screen items-center justify-center bg-gray-950 px-6 text-sm text-gray-400">
        Loading bridge configuration...
      </div>
    )
  }

  return (
    <RuntimeConfigProvider value={runtimeConfig}>
      <WagmiProvider config={wagmiConfig}>
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      </WagmiProvider>
    </RuntimeConfigProvider>
  )
}
