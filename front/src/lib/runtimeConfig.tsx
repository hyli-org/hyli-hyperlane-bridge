'use client'

import { createContext, useContext } from 'react'

export type Address = `0x${string}`

export const DEFAULT_ETHERSCAN_BASE_URL = 'https://sepolia.etherscan.io'
export const DEFAULT_HYLI_EXPLORER_BASE_URL = 'https://explorer.hyli.dev'
export const DEFAULT_HYLI_RPC_URL = 'http://localhost:9002/rpc'
export const DEFAULT_HYLI_INDEXER_URL = 'http://localhost:4322'
export const DEFAULT_SEPOLIA_RPC_URL = 'https://ethereum-sepolia-rpc.publicnode.com'
export const DEFAULT_SEPOLIA_WARP_CONTRACT =
  '0x21f52310012d6B33ea235628087771c8E15B976A' as Address
export const DEFAULT_SEPOLIA_MAILBOX =
  '0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766' as Address
export const DEFAULT_HYLI_WARP_CONTRACT =
  '0xBE1BDc82355Be7ac9aA0BA43Adb34AEEd097B9e6' as Address
export const DEFAULT_HYLI_MAILBOX =
  '0x8F87b871a90C67B2D4F2A5a2bF368feaaCBAB21D' as Address

export interface PublicRuntimeConfig {
  hyliRpcUrl: string
  hyliIndexerUrl: string
  sepoliaRpcUrl: string
  sepoliaWarpContract: Address
  sepoliaMailbox: Address
  hyliWarpContract: Address
  hyliMailbox: Address
  etherscanBaseUrl: string
  hyliExplorerBaseUrl: string
}

export const DEFAULT_PUBLIC_RUNTIME_CONFIG: PublicRuntimeConfig = {
  hyliRpcUrl: DEFAULT_HYLI_RPC_URL,
  hyliIndexerUrl: DEFAULT_HYLI_INDEXER_URL,
  sepoliaRpcUrl: DEFAULT_SEPOLIA_RPC_URL,
  sepoliaWarpContract: DEFAULT_SEPOLIA_WARP_CONTRACT,
  sepoliaMailbox: DEFAULT_SEPOLIA_MAILBOX,
  hyliWarpContract: DEFAULT_HYLI_WARP_CONTRACT,
  hyliMailbox: DEFAULT_HYLI_MAILBOX,
  etherscanBaseUrl: DEFAULT_ETHERSCAN_BASE_URL,
  hyliExplorerBaseUrl: DEFAULT_HYLI_EXPLORER_BASE_URL,
}

const RuntimeConfigContext = createContext<PublicRuntimeConfig>(DEFAULT_PUBLIC_RUNTIME_CONFIG)

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
