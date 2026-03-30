import { createConfig, http } from 'wagmi'
import { sepolia, createHyliChain } from './chains'
import { DEFAULT_HYLI_RPC_URL } from '@/lib/runtimeConfig'

export function createWagmiConfig(hyliRpcUrl = DEFAULT_HYLI_RPC_URL) {
  const hyli = createHyliChain(hyliRpcUrl)

  return createConfig({
    chains: [sepolia, hyli],
    multiInjectedProviderDiscovery: true,
    transports: {
      [sepolia.id]: http(),
      [hyli.id]: http(hyli.rpcUrls.default.http[0]),
    },
  })
}

export const wagmiConfig = createWagmiConfig()
