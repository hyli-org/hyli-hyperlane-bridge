'use client'

import { createContext, useContext } from 'react'

export const DEFAULT_HYLI_RPC_URL = 'http://localhost:9002/rpc'
export const DEFAULT_HYLI_INDEXER_URL = 'http://localhost:4322'

export interface PublicRuntimeConfig {
  hyliRpcUrl: string
  hyliIndexerUrl: string
}

const RuntimeConfigContext = createContext<PublicRuntimeConfig>({
  hyliRpcUrl: DEFAULT_HYLI_RPC_URL,
  hyliIndexerUrl: DEFAULT_HYLI_INDEXER_URL,
})

export function RuntimeConfigProvider({
  children,
  value,
}: {
  children: React.ReactNode
  value: PublicRuntimeConfig
}) {
  return <RuntimeConfigContext.Provider value={value}>{children}</RuntimeConfigContext.Provider>
}

export function useRuntimeConfig() {
  return useContext(RuntimeConfigContext)
}
